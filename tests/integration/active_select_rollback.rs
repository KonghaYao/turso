use std::sync::Arc;

use tempfile::TempDir;
use turso_core::{Connection, Database, PlatformIO, SqliteDialect, StepResult, IO};

use super::queued_io::{QueuedIo, QueuedIoOpKind};

#[test]
fn native_active_index_scan_full_rollback() {
    let (_directory, connection) = open_connection();
    connection
        .execute("BEGIN; DELETE FROM t1 WHERE (i%2)==1;")
        .unwrap();
    let mut statement = connection
        .prepare("SELECT i FROM t1 WHERE (i%2)==0 ORDER BY h DESC;")
        .unwrap();
    let mut rows = Vec::new();
    loop {
        match statement.step().unwrap() {
            StepResult::Row => {
                if rows.is_empty() {
                    connection.execute("ROLLBACK;").unwrap();
                }
                rows.push(statement.row().unwrap().get::<i64>(0).unwrap());
            }
            StepResult::Done => break,
            StepResult::IO => statement._io().step().unwrap(),
            StepResult::Yield => continue,
            other => panic!("unexpected SELECT step: {other:?}"),
        }
    }
    let expected: Vec<i64> = (1..=20).rev().map(|value| value * 2).collect();
    assert_eq!(rows, expected);
    assert!(connection.get_auto_commit());
    assert_eq!(
        query_values(&connection, "SELECT count(*) FROM t1;"),
        vec![40]
    );
    let integrity = connection
        .prepare("PRAGMA integrity_check;")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(integrity[0][0].to_string(), "ok");
    connection
        .execute("BEGIN; DELETE FROM t1 WHERE i=2; ROLLBACK;")
        .unwrap();
    assert_eq!(
        query_values(&connection, "SELECT count(*) FROM t1;"),
        vec![40]
    );
}

#[test]
fn active_select_rollback_at_every_row() {
    for overflow in [false, true] {
        for select in [
            "SELECT i FROM t1 WHERE (i%2)==0;",
            "SELECT i FROM t1 WHERE (i%2)==0 ORDER BY h DESC;",
            "SELECT i FROM t1 WHERE (i%2)==0 ORDER BY h ASC;",
        ] {
            for savepoint in [false, true] {
                for rollback_at in 0..=21 {
                    let (_directory, connection) = open_connection();
                    if overflow {
                        let prefix = "abcdefghij".repeat(70);
                        connection
                            .execute(format!("UPDATE t1 SET h='{prefix}'||h;"))
                            .unwrap();
                    }
                    connection.execute(if savepoint {
                        "BEGIN; DELETE FROM t1 WHERE (i%4)==1; SAVEPOINT one; DELETE FROM t1 WHERE (i%2)==1;"
                    } else {
                        "BEGIN; DELETE FROM t1 WHERE (i%2)==1;"
                    }).unwrap();
                    let mut statement = connection.prepare(select).unwrap();
                    let mut rows = Vec::new();
                    loop {
                        match statement.step().unwrap() {
                            StepResult::Row => {
                                if rows.len() == rollback_at {
                                    connection
                                        .execute(if savepoint {
                                            "ROLLBACK TO one;"
                                        } else {
                                            "ROLLBACK;"
                                        })
                                        .unwrap();
                                }
                                rows.push(statement.row().unwrap().get::<i64>(0).unwrap());
                            }
                            StepResult::Done => break,
                            StepResult::IO => statement._io().step().unwrap(),
                            StepResult::Yield => continue,
                            other => panic!("unexpected SELECT step: {other:?}"),
                        }
                    }
                    let mut expected = vec![
                        2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40,
                    ];
                    if select.ends_with("DESC;") {
                        expected.reverse();
                    }
                    assert_eq!(rows, expected, "overflow={overflow} savepoint={savepoint} rollback_at={rollback_at} select={select}");
                    let rolled_back = rollback_at < 20;
                    assert_eq!(connection.get_auto_commit(), rolled_back && !savepoint);
                    let expected_count = if !rolled_back {
                        20
                    } else if savepoint {
                        30
                    } else {
                        40
                    };
                    assert_eq!(
                        query_values(&connection, "SELECT count(*) FROM t1;"),
                        vec![expected_count]
                    );
                    if !connection.get_auto_commit() {
                        connection.execute("ROLLBACK;").unwrap();
                    }
                    let integrity = connection
                        .prepare("PRAGMA integrity_check;")
                        .unwrap()
                        .run_collect_rows()
                        .unwrap();
                    assert_eq!(integrity[0][0].to_string(), "ok");
                    connection.execute("BEGIN; UPDATE t1 SET h='changed' WHERE i=2; DELETE FROM t1 WHERE i=4; ROLLBACK;").unwrap();
                    assert_eq!(
                        query_values(&connection, "SELECT count(*) FROM t1;"),
                        vec![40]
                    );
                    assert_eq!(
                        query_values(&connection, "SELECT i FROM t1 WHERE i IN (2,4) ORDER BY i;"),
                        vec![2, 4]
                    );
                }
            }
        }
    }
}

#[test]
fn rollback_removes_current_index_key_without_skipping_next_row() {
    for overflow in [false, true] {
        for descending in [false, true] {
            for savepoint in [false, true] {
                let (_directory, connection) = open_connection();
                let prefix = if overflow {
                    "abcdefghij".repeat(70)
                } else {
                    String::new()
                };
                if overflow {
                    connection
                        .execute(format!("UPDATE t1 SET h='{prefix}'||h;"))
                        .unwrap();
                }
                connection
                    .execute(if savepoint {
                        "BEGIN; SAVEPOINT one;"
                    } else {
                        "BEGIN;"
                    })
                    .unwrap();
                connection
                    .execute(format!("INSERT INTO t1 VALUES(99,'{prefix}14a');"))
                    .unwrap();
                let mut reader = connection
                    .prepare(if descending {
                        "SELECT i FROM t1 ORDER BY h DESC;"
                    } else {
                        "SELECT i FROM t1 ORDER BY h ASC;"
                    })
                    .unwrap();
                let mut rows = Vec::new();
                loop {
                    match reader.step().unwrap() {
                        StepResult::Row => {
                            if rows.len() == 20 {
                                connection
                                    .execute(if savepoint {
                                        "ROLLBACK TO one;"
                                    } else {
                                        "ROLLBACK;"
                                    })
                                    .unwrap();
                            }
                            rows.push(reader.row().unwrap().get::<i64>(0).unwrap());
                        }
                        StepResult::Done => break,
                        StepResult::IO => reader._io().step().unwrap(),
                        StepResult::Yield => continue,
                        other => panic!("unexpected SELECT step: {other:?}"),
                    }
                }
                let mut expected: Vec<i64> = (1..=40).collect();
                if descending {
                    expected.reverse();
                }
                expected.insert(20, 99);
                assert_eq!(
                    rows, expected,
                    "overflow={overflow} descending={descending} savepoint={savepoint}"
                );
                if !connection.get_auto_commit() {
                    connection.execute("ROLLBACK;").unwrap();
                }
                assert_eq!(
                    query_values(&connection, "SELECT count(*) FROM t1;"),
                    vec![40]
                );
            }
        }
    }
}

#[test]
fn active_selects_survive_statement_rollback_and_read_only_rollback() {
    let (_directory, connection) = open_connection();
    connection
        .execute("CREATE UNIQUE INDEX unique_i ON t1(i);")
        .unwrap();
    connection
        .execute("BEGIN; DELETE FROM t1 WHERE i%2==1;")
        .unwrap();
    let mut descending = connection
        .prepare("SELECT i FROM t1 WHERE i%2==0 ORDER BY h DESC;")
        .unwrap();
    let mut ascending = connection
        .prepare("SELECT i FROM t1 WHERE i%2==0;")
        .unwrap();
    assert_eq!(next_value(&mut descending), Some(40));
    assert_eq!(next_value(&mut ascending), Some(2));
    let error = connection
        .execute("UPDATE t1 SET i=42 WHERE i IN (2,4);")
        .unwrap_err();
    assert!(error.to_string().contains("UNIQUE constraint failed"));
    assert!(!connection.get_auto_commit());
    assert_eq!(
        query_values(
            &connection,
            "SELECT i FROM t1 WHERE i IN (2,4,42) ORDER BY i;"
        ),
        vec![2, 4]
    );
    let mut descending_values = vec![40];
    while let Some(value) = next_value(&mut descending) {
        descending_values.push(value);
    }
    let mut ascending_values = vec![2];
    while let Some(value) = next_value(&mut ascending) {
        ascending_values.push(value);
    }
    assert_eq!(
        ascending_values,
        vec![2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40]
    );
    ascending_values.reverse();
    assert_eq!(descending_values, ascending_values);
    connection.execute("ROLLBACK; BEGIN;").unwrap();
    let mut read_only = connection
        .prepare("SELECT i FROM t1 WHERE i%2==0;")
        .unwrap();
    assert_eq!(next_value(&mut read_only), Some(2));
    connection.execute("ROLLBACK;").unwrap();
    let mut values = vec![2];
    while let Some(value) = next_value(&mut read_only) {
        values.push(value);
    }
    assert_eq!(
        values,
        vec![2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40]
    );
    assert!(connection.get_auto_commit());
    assert_eq!(
        query_values(&connection, "SELECT count(*) FROM t1;"),
        vec![40]
    );
}

fn next_value(statement: &mut turso_core::Statement) -> Option<i64> {
    loop {
        match statement.step().unwrap() {
            StepResult::Row => return Some(statement.row().unwrap().get::<i64>(0).unwrap()),
            StepResult::Done => return None,
            StepResult::IO => statement._io().step().unwrap(),
            StepResult::Yield => continue,
            other => panic!("unexpected SELECT step: {other:?}"),
        }
    }
}

#[test]
fn rollback_preservation_io_failure_completes_cleanup() {
    let io = Arc::new(QueuedIo::new());
    let database =
        Database::open_file(io.clone(), "bookmark-io.db", Arc::new(SqliteDialect)).unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute("PRAGMA cache_size=10; CREATE TABLE t1(i,h); CREATE INDEX i1 ON t1(h);")
        .unwrap();
    let prefix = "abcdefghij".repeat(1000);
    for value in 1..=40 {
        connection
            .execute(format!(
                "INSERT INTO t1 VALUES({value},'{prefix}{value:02X}');"
            ))
            .unwrap();
    }
    connection
        .execute("PRAGMA wal_checkpoint(TRUNCATE); BEGIN; UPDATE t1 SET i=-1 WHERE i=40;")
        .unwrap();
    let mut reader = connection
        .prepare("SELECT 1 FROM t1 ORDER BY h ASC;")
        .unwrap();
    assert_eq!(next_value(&mut reader), Some(1));
    io.fault_after("", QueuedIoOpKind::Pread, 0);
    let rollback_result = connection.execute("ROLLBACK;");
    io.clear_fault();
    let error = rollback_result.unwrap_err();
    assert!(matches!(error, turso_core::LimboError::CompletionError(_)));
    assert!(matches!(
        reader.step(),
        Err(turso_core::LimboError::CompletionError(_))
    ));
    assert!(connection.get_auto_commit());
    assert_eq!(
        query_values(&connection, "SELECT count(*) FROM t1;"),
        vec![40]
    );
    connection
        .execute("BEGIN; UPDATE t1 SET h='changed' WHERE i=2; ROLLBACK;")
        .unwrap();
    assert!(matches!(
        reader.step(),
        Err(turso_core::LimboError::CompletionError(_))
    ));
    let integrity = connection
        .prepare("PRAGMA integrity_check;")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(integrity[0][0].to_string(), "ok");
    let second = database.connect().unwrap();
    second
        .execute("BEGIN IMMEDIATE; DELETE FROM t1 WHERE i=4; ROLLBACK;")
        .unwrap();
    assert_eq!(query_values(&second, "SELECT count(*) FROM t1;"), vec![40]);
}

fn open_connection() -> (TempDir, Arc<Connection>) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("rollback.db");
    let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
    let connection = Database::open_file(io, path.to_str().unwrap(), Arc::new(SqliteDialect))
        .unwrap()
        .connect()
        .unwrap();
    connection
        .execute("CREATE TABLE t1(i,h); CREATE INDEX i1 ON t1(h);")
        .unwrap();
    for value in 1..=40 {
        connection
            .execute(format!("INSERT INTO t1 VALUES({value},'{value:02X}');"))
            .unwrap();
    }
    (directory, connection)
}

fn query_values(connection: &Arc<Connection>, sql: &str) -> Vec<i64> {
    connection
        .prepare(sql)
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[0].as_int().unwrap())
        .collect()
}
