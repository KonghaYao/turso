use crate::queued_io::{QueuedIo, QueuedIoOpKind};
use std::sync::Arc;
use turso_core::{Connection, Database, DatabaseOpts, OpenFlags, SqliteDialect};

#[test]
#[serial_test::serial]
fn rename_view_tokens_dependencies_and_reopen() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let database = open(io.clone())?;
    let connection = database.connect()?;
    connection.execute("CREATE TABLE t1(a,b); INSERT INTO t1 VALUES(11,12),(21,22)")?;
    let original = "CREATE VIEW v1 AS SELECT /* t1 */ t1.a+1,'t1' AS literal FROM [t1] LIMIT 0,2";
    connection.execute(original)?;
    connection.execute("CREATE VIEW v2 AS SELECT * FROM v1; CREATE VIEW cv AS SELECT (VALUES(a),(b)) FROM (SELECT a,b FROM t1)")?;
    assert_eq!(
        rows(&connection, "SELECT sql FROM sqlite_schema WHERE name='v1'")?,
        [original]
    );
    connection.execute("ALTER TABLE t1 RENAME TO \"new\"\"table\"")?;
    let rewritten = "CREATE VIEW v1 AS SELECT /* t1 */ \"new\"\"table\".a+1,'t1' AS literal FROM \"new\"\"table\" LIMIT 0,2";
    assert_eq!(
        rows(&connection, "SELECT sql FROM sqlite_schema WHERE name='v1'")?,
        [rewritten]
    );
    assert_eq!(rows(&connection, "SELECT * FROM v2")?, ["12|t1", "22|t1"]);
    assert_eq!(rows(&connection, "SELECT * FROM cv")?, ["11", "21"]);
    let columns = rows(&connection, "SELECT name FROM pragma_table_info('v2')")?;
    assert_eq!(columns, ["\"new\"\"table\".a+1", "literal"]);
    connection.close()?;
    drop(connection);
    drop(database);
    let reopened = open(io)?;
    let connection = reopened.connect()?;
    assert_eq!(rows(&connection, "SELECT * FROM v2")?, ["12|t1", "22|t1"]);
    assert_eq!(rows(&connection, "SELECT * FROM cv")?, ["11", "21"]);
    assert_eq!(
        rows(&connection, "SELECT name FROM pragma_table_info('v2')")?,
        columns
    );
    assert_eq!(
        rows(&connection, "SELECT sql FROM sqlite_schema WHERE name='v1'")?,
        [rewritten]
    );
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_database_and_alias_scope() -> turso_core::Result<()> {
    let database = open(Arc::new(QueuedIo::new()))?;
    let connection = database.connect()?;
    connection.execute("ATTACH 'aux.db' AS aux; CREATE TABLE main.t1(a); CREATE TABLE aux.t1(a); INSERT INTO main.t1 VALUES(11); INSERT INTO aux.t1 VALUES(22); CREATE TEMP VIEW vt AS SELECT main.t1.a,aux.t1.a FROM main.t1,aux.t1; CREATE VIEW aliases AS SELECT t1.a FROM t1 AS t1; CREATE VIEW ctes AS WITH t1(a) AS (VALUES(99)) SELECT * FROM t1")?;
    connection.execute("ALTER TABLE main.t1 RENAME TO t2")?;
    assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["11|22"]);
    assert_eq!(
        rows(
            &connection,
            "SELECT sql FROM temp.sqlite_schema WHERE name='vt'"
        )?,
        ["CREATE VIEW vt AS SELECT main.\"t2\".a,aux.t1.a FROM main.\"t2\",aux.t1"]
    );
    assert_eq!(
        rows(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE name='aliases'"
        )?,
        ["CREATE VIEW aliases AS SELECT t1.a FROM \"t2\" AS t1"]
    );
    assert_eq!(rows(&connection, "SELECT * FROM aliases")?, ["11"]);
    assert_eq!(rows(&connection, "SELECT * FROM ctes")?, ["99"]);
    connection.execute("ALTER TABLE aux.t1 RENAME TO t3")?;
    assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["11|22"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_rollback_restores_sql_columns_and_cookies() -> turso_core::Result<()> {
    for finish in [
        "ROLLBACK",
        "ROLLBACK TO before_rename; RELEASE before_rename",
    ] {
        let database = open(Arc::new(QueuedIo::new()))?;
        let connection = database.connect()?;
        setup(&connection)?;
        let before = snapshot(&connection)?;
        connection.execute("SAVEPOINT before_rename; ALTER TABLE t1 RENAME TO t2")?;
        assert_eq!(rows(&connection, "SELECT * FROM v2")?, ["34"]);
        assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["33"]);
        connection.execute(finish)?;
        assert_eq!(snapshot(&connection)?, before);
        assert_eq!(rows(&connection, "SELECT * FROM v2")?, ["34"]);
        assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["33"]);
        assert_eq!(
            rows(&connection, "SELECT name FROM pragma_table_info('v2')")?,
            ["t1.a+1"]
        );
        connection.execute("ALTER TABLE t1 RENAME TO t2")?;
        assert_eq!(rows(&connection, "SELECT * FROM v2")?, ["34"]);
    }
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_wal_fault_visibility_consistent() -> turso_core::Result<()> {
    for kind in [QueuedIoOpKind::Pwritev, QueuedIoOpKind::Sync] {
        let io = Arc::new(QueuedIo::new());
        let db = open(io.clone())?;
        let conn = db.connect()?;
        setup(&conn)?;
        let before = snapshot(&conn)?;
        let second = db.connect()?;
        let mut prepared = second.prepare("SELECT * FROM v2")?;
        assert_eq!(rows(&second, "SELECT * FROM v2")?, ["34"]);
        io.fault_after("rename-views.db-wal", kind, 0);
        let result = conn.execute("ALTER TABLE t1 RENAME TO t2");
        io.clear_fault();
        assert!(
            matches!(result, Err(turso_core::LimboError::CommitUncertain)),
            "{result:?}"
        );
        assert!(matches!(
            snapshot(&conn),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&conn, "PRAGMA main.schema_version"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&conn, "SELECT name FROM main.sqlite_schema"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&second, "SELECT * FROM v2"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            prepared.run_collect_rows(),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        let old_name = if kind == QueuedIoOpKind::Sync {
            "t2"
        } else {
            "t1"
        };
        assert!(matches!(
            conn.execute(format!("ALTER TABLE {old_name} RENAME TO t3")),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        drop(prepared);
        second.close()?;
        drop(second);
        conn.close()?;
        drop(conn);
        drop(db);
        let db = open(io)?;
        let conn = db.connect()?;
        assert_eq!(rows(&conn, "SELECT * FROM v2")?, ["34"]);
        assert_eq!(rows(&conn, "SELECT * FROM v1")?, ["34"]);
        assert_eq!(rows(&conn, "PRAGMA integrity_check")?, ["ok"]);
        let name = rows(
            &conn,
            "SELECT name FROM sqlite_schema WHERE type='table' AND name='t2'",
        )?;
        if kind == QueuedIoOpKind::Sync {
            assert_eq!(name, ["t2"]);
        } else {
            assert!(name.is_empty());
            let recovered = snapshot(&conn)?;
            assert_eq!(recovered[0], before[0]);
            assert_eq!(recovered[2], before[2]);
            assert_eq!(recovered[3], Vec::<String>::new()); // TEMP schema does not survive a close.
        }
    }
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_reprepare_includes_new_dependencies() -> turso_core::Result<()> {
    let database = open(Arc::new(QueuedIo::new()))?;
    let connection = database.connect()?;
    setup(&connection)?;
    let mut rename = connection.prepare("ALTER TABLE t1 RENAME TO t2")?;
    let other = database.connect()?;
    other.execute("CREATE VIEW later AS SELECT t1.a+2 FROM t1")?;
    rename.run_ignore_rows()?;
    assert_eq!(rows(&connection, "SELECT * FROM later")?, ["35"]);
    assert_eq!(rows(&other, "SELECT * FROM v2")?, ["34"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_nested_scopes_and_temp_shadow() -> turso_core::Result<()> {
    let database = open(Arc::new(QueuedIo::new()))?;
    let connection = database.connect()?;
    connection.execute("CREATE TABLE t1(a); INSERT INTO t1 VALUES(11); CREATE TABLE other(a); INSERT INTO other VALUES(22); CREATE VIEW nested AS SELECT t1.a,(SELECT t1.a FROM other AS t1),(SELECT t1.a) FROM t1; CREATE VIEW nested_cte AS SELECT t1.a,(WITH t1(a) AS (VALUES(99)) SELECT a FROM t1) FROM t1; CREATE TEMP TABLE t1(a); INSERT INTO temp.t1 VALUES(33); CREATE TEMP VIEW shadow AS SELECT t1.a FROM t1; CREATE TEMP VIEW explicit AS SELECT main.t1.a FROM main.t1")?;
    connection.execute("ALTER TABLE main.t1 RENAME TO t2")?;
    assert_eq!(rows(&connection, "SELECT * FROM nested")?, ["11|22|11"]);
    assert_eq!(rows(&connection, "SELECT * FROM nested_cte")?, ["11|99"]);
    assert_eq!(rows(&connection, "SELECT * FROM shadow")?, ["33"]);
    assert_eq!(
        rows(
            &connection,
            "SELECT sql FROM temp.sqlite_schema WHERE name='shadow'"
        )?,
        ["CREATE VIEW shadow AS SELECT t1.a FROM t1"]
    );
    assert_eq!(rows(&connection, "SELECT * FROM explicit")?, ["11"]);
    connection.execute("ALTER TABLE temp.t1 RENAME TO temp_source")?;
    assert_eq!(rows(&connection, "SELECT * FROM shadow")?, ["33"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_attached_transaction_rollback() -> turso_core::Result<()> {
    let database = open(Arc::new(QueuedIo::new()))?;
    let connection = database.connect()?;
    connection.execute("ATTACH 'aux.db' AS \"aux db\"; CREATE TABLE \"aux db\".t1(a); INSERT INTO \"aux db\".t1 VALUES(44); CREATE VIEW \"aux db\".v AS SELECT t1.a+1 FROM t1; CREATE TEMP VIEW vt AS SELECT * FROM \"aux db\".t1")?;
    let before = rows(
        &connection,
        "SELECT sql FROM \"aux db\".sqlite_schema ORDER BY name",
    )?;
    connection.execute("BEGIN; ALTER TABLE \"aux db\".t1 RENAME TO t2")?;
    assert_eq!(rows(&connection, "SELECT * FROM \"aux db\".v")?, ["45"]);
    assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["44"]);
    connection.execute("ROLLBACK")?;
    assert_eq!(
        rows(
            &connection,
            "SELECT sql FROM \"aux db\".sqlite_schema ORDER BY name"
        )?,
        before
    );
    assert_eq!(rows(&connection, "SELECT * FROM vt")?, ["44"]);
    connection.execute("ALTER TABLE \"aux db\".t1 RENAME TO t2")?;
    assert_eq!(rows(&connection, "SELECT * FROM \"aux db\".v")?, ["45"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_unrelated_temp_schema_untouched() -> turso_core::Result<()> {
    let database = open(Arc::new(QueuedIo::new()))?;
    let connection = database.connect()?;
    connection.execute("CREATE TABLE main.t1(a); INSERT INTO t1 VALUES(7); CREATE VIEW dependent AS SELECT a FROM t1; CREATE TEMP VIEW unrelated AS SELECT 99 AS val")?;
    let before_cookie = rows(&connection, "PRAGMA temp.schema_version")?;
    let before_sql = rows(
        &connection,
        "SELECT sql FROM temp.sqlite_schema WHERE name='unrelated'",
    )?;
    connection.execute("ALTER TABLE main.t1 RENAME TO t2")?;
    assert_eq!(rows(&connection, "SELECT * FROM dependent")?, ["7"]);
    assert_eq!(rows(&connection, "SELECT * FROM unrelated")?, ["99"]);
    assert_eq!(
        rows(
            &connection,
            "SELECT sql FROM temp.sqlite_schema WHERE name='unrelated'"
        )?,
        before_sql
    );
    assert_eq!(
        rows(&connection, "PRAGMA temp.schema_version")?,
        before_cookie
    );
    Ok(())
}

#[test]
#[serial_test::serial]
fn rename_view_aux_wal_fault_same_connection_and_reopen() -> turso_core::Result<()> {
    for kind in [QueuedIoOpKind::Pwritev, QueuedIoOpKind::Sync] {
        let io = Arc::new(QueuedIo::new());
        let db = open(io.clone())?;
        let conn = db.connect()?;
        conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; PRAGMA aux.synchronous=FULL; CREATE TABLE main.stable(a); INSERT INTO main.stable VALUES(11); CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(23); CREATE VIEW aux.v AS SELECT t1.a+1 FROM t1; CREATE TEMP VIEW vt AS SELECT a FROM aux.t1")?;
        let aux_before = rows(
            &conn,
            "SELECT name,sql FROM aux.sqlite_schema ORDER BY name",
        )?;
        let aux_cookie_before = rows(&conn, "PRAGMA aux.schema_version")?;
        let main_before = rows(
            &conn,
            "SELECT name,sql FROM main.sqlite_schema ORDER BY name",
        )?;
        let main_cookie_before = rows(&conn, "PRAGMA main.schema_version")?;
        let temp_before = rows(
            &conn,
            "SELECT name,sql FROM temp.sqlite_schema ORDER BY name",
        )?;
        let temp_cookie_before = rows(&conn, "PRAGMA temp.schema_version")?;
        let second = db.connect()?;
        second.execute("ATTACH 'aux.db' AS aux")?;
        let mut prepared = second.prepare("SELECT * FROM aux.v")?;
        assert_eq!(rows(&second, "SELECT * FROM aux.v")?, ["24"]);
        io.fault_after("aux.db-wal", kind, 0);
        let result = conn.execute("ALTER TABLE aux.t1 RENAME TO t2");
        io.clear_fault();
        assert!(
            matches!(result, Err(turso_core::LimboError::CommitUncertain)),
            "{result:?}"
        );
        assert!(matches!(
            rows(&conn, "SELECT * FROM aux.v"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&conn, "PRAGMA aux.schema_version"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&conn, "SELECT name FROM aux.sqlite_schema"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            rows(&second, "SELECT * FROM aux.v"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert!(matches!(
            prepared.run_collect_rows(),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        let active_table = if kind == QueuedIoOpKind::Sync {
            "t2"
        } else {
            "t1"
        };
        assert!(matches!(
            conn.execute(format!("UPDATE aux.{active_table} SET a=25")),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert_eq!(
            rows(
                &conn,
                "SELECT name,sql FROM main.sqlite_schema ORDER BY name"
            )?,
            main_before
        );
        assert_eq!(
            rows(&conn, "PRAGMA main.schema_version")?,
            main_cookie_before
        );
        assert_eq!(rows(&conn, "SELECT * FROM main.stable")?, ["11"]);
        if kind == QueuedIoOpKind::Pwritev {
            assert_eq!(
                rows(
                    &conn,
                    "SELECT name,sql FROM temp.sqlite_schema ORDER BY name"
                )?,
                temp_before
            );
            assert_eq!(
                rows(&conn, "PRAGMA temp.schema_version")?,
                temp_cookie_before
            );
        } else {
            assert_eq!(
                rows(
                    &conn,
                    "SELECT name,sql FROM temp.sqlite_schema ORDER BY name"
                )?,
                ["vt|CREATE VIEW vt AS SELECT a FROM aux.\"t2\""]
            );
            assert_eq!(rows(&conn, "PRAGMA temp.schema_version")?, ["2"]);
        }
        assert!(matches!(
            rows(&conn, "SELECT * FROM vt"),
            Err(turso_core::LimboError::CommitUncertain)
        ));
        assert_eq!(
            rows(&second, "SELECT name FROM temp.sqlite_schema")?,
            Vec::<String>::new()
        );
        drop(prepared);
        second.close()?;
        drop(second);
        conn.close()?;
        drop(conn);
        drop(db);
        let db = open(io)?;
        let conn = db.connect()?;
        conn.execute("ATTACH 'aux.db' AS aux")?;
        let recovered_schema = rows(
            &conn,
            "SELECT name,sql FROM aux.sqlite_schema ORDER BY name",
        )?;
        let recovered_cookie = rows(&conn, "PRAGMA aux.schema_version")?;
        if kind == QueuedIoOpKind::Sync {
            assert_eq!(
                recovered_schema,
                [
                    "t2|CREATE TABLE t2 (a)",
                    "v|CREATE VIEW v AS SELECT \"t2\".a+1 FROM \"t2\""
                ]
            );
            assert_eq!(recovered_cookie, ["3"]);
        } else {
            assert_eq!(recovered_schema, aux_before);
            assert_eq!(recovered_cookie, aux_cookie_before);
        }
        assert_eq!(rows(&conn, "SELECT * FROM aux.v")?, ["24"]);
    }
    Ok(())
}

fn open(io: Arc<QueuedIo>) -> turso_core::Result<Arc<Database>> {
    Database::open_file_with_flags(
        io,
        "rename-views.db",
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true).with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )
}

fn setup(connection: &Arc<Connection>) -> turso_core::Result<()> {
    connection.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; CREATE TABLE t1(a); INSERT INTO t1 VALUES(33); CREATE VIEW v1 AS SELECT t1.a+1 FROM t1; CREATE VIEW v2 AS SELECT * FROM v1; CREATE TEMP VIEW vt AS SELECT * FROM main.t1")
}

fn snapshot(connection: &Arc<Connection>) -> turso_core::Result<Vec<Vec<String>>> {
    [
        "PRAGMA main.schema_version",
        "PRAGMA temp.schema_version",
        "SELECT type,name,sql FROM main.sqlite_schema ORDER BY name",
        "SELECT type,name,sql FROM temp.sqlite_schema ORDER BY name",
    ]
    .iter()
    .map(|sql| rows(connection, sql))
    .collect()
}

fn rows(connection: &Arc<Connection>, sql: &str) -> turso_core::Result<Vec<String>> {
    Ok(connection
        .prepare(sql)?
        .run_collect_rows()?
        .iter()
        .map(|row| {
            row.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect())
}
