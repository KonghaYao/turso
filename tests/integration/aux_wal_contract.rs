use crate::queued_io::{QueuedIo, QueuedIoOpKind};
use std::sync::Arc;
use turso_core::{Connection, Database, DatabaseOpts, OpenFlags, SqliteDialect};

fn open(io: Arc<QueuedIo>) -> turso_core::Result<Arc<Database>> {
    Database::open_file_with_flags(
        io,
        "aux-wal-contract.db",
        OpenFlags::default(),
        DatabaseOpts::new().with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )
}

fn values(connection: &Arc<Connection>) -> turso_core::Result<Vec<String>> {
    Ok(connection
        .prepare("SELECT a FROM aux.t1")?
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

#[test]
#[serial_test::serial]
fn aux_wal_fsync_error_exposes_same_state_after_reopen() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    let before = values(&conn)?;
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let result = conn.execute("UPDATE aux.t1 SET a=2");
    io.clear_fault();
    let same_connection = values(&conn);
    conn.close()?;
    drop(conn);
    drop(db);
    let db = open(io)?;
    let conn = db.connect()?;
    conn.execute("ATTACH 'aux.db' AS aux")?;
    let reopened = values(&conn)?;
    eprintln!("before={before:?}; update={result:?}; same_connection={same_connection:?}; reopened={reopened:?}");
    assert!(matches!(
        result,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert_eq!(before, ["1"]);
    assert!(matches!(
        same_connection,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert_eq!(reopened, ["2"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_fsync_failure_followup_write_reopens() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let failed = conn.execute("UPDATE aux.t1 SET a=2");
    io.clear_fault();
    let immediately = values(&conn);
    let followup = conn.execute("UPDATE aux.t1 SET a=3");
    let after_write = values(&conn);
    conn.close()?;
    drop(conn);
    drop(db);
    let db = open(io)?;
    let conn = db.connect()?;
    conn.execute("ATTACH 'aux.db' AS aux")?;
    let reopened = values(&conn)?;
    eprintln!("failed={failed:?}; immediately={immediately:?}; followup={followup:?}; after_write={after_write:?}; reopened={reopened:?}");
    for result in [
        failed,
        followup,
        immediately.map(|_| ()),
        after_write.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(turso_core::LimboError::CommitUncertain)),
            "{result:?}"
        );
    }
    assert_eq!(reopened, ["2"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_pwritev_failure_cannot_commit_after_reopen() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    io.fault_after("aux.db-wal", QueuedIoOpKind::Pwritev, 0);
    let failed = conn.execute("UPDATE aux.t1 SET a=2");
    io.clear_fault();
    let immediately = values(&conn);
    conn.close()?;
    drop(conn);
    drop(db);
    let db = open(io)?;
    let conn = db.connect()?;
    conn.execute("ATTACH 'aux.db' AS aux")?;
    let reopened = values(&conn)?;
    eprintln!("failed={failed:?}; immediately={immediately:?}; reopened={reopened:?}");
    assert!(matches!(
        failed,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        immediately,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert_eq!(reopened, ["1"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_repeated_sync_fault_preserves_visible_commits() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let first = conn.execute("UPDATE aux.t1 SET a=2");
    let first_value = values(&conn);
    let second = conn.execute("UPDATE aux.t1 SET a=3");
    let second_value = values(&conn);
    io.clear_fault();
    conn.close()?;
    drop(conn);
    drop(db);
    let db = open(io)?;
    let conn = db.connect()?;
    conn.execute("ATTACH 'aux.db' AS aux")?;
    let reopened = values(&conn)?;
    eprintln!("first={first:?}, first_value={first_value:?}, second={second:?}, second_value={second_value:?}, reopened={reopened:?}");
    for result in [
        first,
        second,
        first_value.map(|_| ()),
        second_value.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(turso_core::LimboError::CommitUncertain)),
            "{result:?}"
        );
    }
    assert_eq!(reopened, ["2"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_sync_then_write_fault_preserves_last_visible_commit() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let first = conn.execute("UPDATE aux.t1 SET a=2");
    io.clear_fault();
    assert!(matches!(
        first,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        values(&conn),
        Err(turso_core::LimboError::CommitUncertain)
    ));
    io.fault_after("aux.db-wal", QueuedIoOpKind::Pwritev, 0);
    let second = conn.execute("UPDATE aux.t1 SET a=3");
    io.clear_fault();
    let same = values(&conn);
    conn.close()?;
    drop(conn);
    drop(db);
    let db = open(io)?;
    let conn = db.connect()?;
    conn.execute("ATTACH 'aux.db' AS aux")?;
    let reopened = values(&conn)?;
    eprintln!("second={second:?}, same={same:?}, reopened={reopened:?}");
    assert!(matches!(
        second,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(same, Err(turso_core::LimboError::CommitUncertain)));
    assert_eq!(reopened, ["2"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_failed_sync_power_loss_may_drop_visible_commit() -> turso_core::Result<()> {
    use crate::unreliable_io::UnreliableIo;
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.db");
    let aux = dir.path().join("aux.db");
    let io = Arc::new(UnreliableIo::new());
    let db = Database::open_file_with_flags(
        io.clone(),
        main.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1")?;
    conn.execute(format!("ATTACH '{}' AS aux; PRAGMA aux.synchronous=FULL; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)", aux.display()))?;
    let second = db.connect()?;
    second.execute(format!("ATTACH '{}' AS aux", aux.display()))?;
    second.execute("BEGIN")?;
    let mut prepared = second.prepare("SELECT a FROM aux.t1")?;
    let mut active = second.prepare("SELECT a FROM aux.t1 UNION ALL SELECT a FROM aux.t1")?;
    assert!(matches!(active.step()?, turso_core::StepResult::Row));
    assert_eq!(values(&second)?, ["1"]);
    io.mark_all_durable();
    io.fail_next_sync_on(&format!("{}-wal", aux.display()));
    let errored = conn.execute("UPDATE aux.t1 SET a=2");
    let same_connection = values(&conn);
    let second_connection = values(&second);
    let prepared_read = prepared.run_collect_rows();
    let active_read = active.step();
    let durable = io.durable_files();
    eprintln!(
        "errored={errored:?}; same={same_connection:?}; second={second_connection:?}; prepared={prepared_read:?}; durable_files={:?}",
        durable.keys()
    );
    assert!(matches!(
        errored,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        same_connection,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        second_connection,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        prepared_read,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(
        matches!(active_read, Err(turso_core::LimboError::CommitUncertain)),
        "{active_read:?}"
    );
    drop(active);
    drop(prepared);
    second.close()?;
    drop(second);
    conn.close()?;
    drop(conn);
    drop(db);
    drop(io);
    for (path, bytes) in durable {
        std::fs::write(path, bytes).unwrap();
    }
    let db = Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new()?),
        main.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )?;
    let conn = db.connect()?;
    conn.execute(format!("ATTACH '{}' AS aux", aux.display()))?;
    assert_eq!(values(&conn)?, ["1"]);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_pwritev_completion_error_after_bytes_must_block_live_read() -> turso_core::Result<()> {
    use crate::unreliable_io::UnreliableIo;
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.db");
    let aux = dir.path().join("aux.db");
    let io = Arc::new(UnreliableIo::new());
    let db = Database::open_file_with_flags(
        io.clone(),
        main.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1")?;
    conn.execute(format!("ATTACH '{}' AS aux; PRAGMA aux.synchronous=FULL; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)", aux.display()))?;
    let other = db.connect()?;
    other.execute(format!("ATTACH '{}' AS aux", aux.display()))?;
    let mut active_reader = other.prepare("SELECT a FROM aux.t1 UNION ALL SELECT a FROM aux.t1")?;
    assert!(matches!(active_reader.step()?, turso_core::StepResult::Row));
    io.mark_all_durable();
    io.fail_next_pwritev_after_bytes_on(&format!("{}-wal", aux.display()));
    let result = conn.execute("UPDATE aux.t1 SET a=2");
    let same = values(&conn);
    let second = values(&other);
    let active = active_reader.step();
    eprintln!(
        "after-byte pwritev: result={result:?} same={same:?} second={second:?} active={active:?}"
    );
    assert!(matches!(
        result,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(same, Err(turso_core::LimboError::CommitUncertain)));
    assert!(matches!(
        second,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(matches!(
        active,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    drop(active_reader);
    other.close()?;
    drop(other);
    conn.close()?;
    drop(conn);
    drop(db);
    // The errored write might be complete and reach stable storage later.
    io.mark_all_durable();
    let files = io.durable_files();
    drop(io);
    for (path, bytes) in files {
        std::fs::write(path, bytes).unwrap();
    }
    let db = Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new()?),
        main.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_attach(true),
        None,
        Arc::new(SqliteDialect),
    )?;
    let conn = db.connect()?;
    conn.execute(format!("ATTACH '{}' AS aux", aux.display()))?;
    eprintln!("after-byte pwritev reopened={:?}", values(&conn)?);
    Ok(())
}

#[test]
#[serial_test::serial]
fn aux_wal_uncertainty_must_reach_independent_database_handle() -> turso_core::Result<()> {
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let conn = db.connect()?;
    conn.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; PRAGMA aux.synchronous=FULL; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1)")?;
    // Separate handle, same I/O backend and file path, already open before the fault.
    let other_db = open(io.clone())?;
    let other = other_db.connect()?;
    other.execute("ATTACH 'aux.db' AS aux")?;
    assert_eq!(values(&other)?, ["1"]);
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let result = conn.execute("UPDATE aux.t1 SET a=2");
    io.clear_fault();
    let second_handle = values(&other);
    eprintln!("independent database: result={result:?}, other={second_handle:?}");
    assert!(matches!(
        result,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(
        matches!(second_handle, Err(turso_core::LimboError::CommitUncertain)),
        "{second_handle:?}"
    );
    Ok(())
}

#[test]
#[serial_test::serial]
fn adversarial_open_read_transaction_and_cursor_after_sync_fault() -> turso_core::Result<()> {
    use turso_core::StepResult;
    let io = Arc::new(QueuedIo::new());
    let db = open(io.clone())?;
    let writer = db.connect()?;
    writer.execute("PRAGMA synchronous=FULL; PRAGMA data_sync_retry=1; ATTACH 'aux.db' AS aux; PRAGMA aux.synchronous=FULL; CREATE TABLE aux.t1(a); INSERT INTO aux.t1 VALUES(1),(3)")?;
    let reader = db.connect()?;
    reader.execute("ATTACH 'aux.db' AS aux; BEGIN")?;
    let mut cursor = reader.prepare("SELECT a FROM aux.t1 ORDER BY a")?;
    let first = loop {
        match cursor.step()? {
            StepResult::Row => break cursor.row().unwrap().get::<i64>(0)?,
            StepResult::IO | StepResult::Yield | StepResult::Sleep { .. } => {
                io.step_one()?;
            }
            StepResult::Done => panic!("expected two rows"),
            StepResult::Busy | StepResult::Interrupt => panic!("unexpected cursor state"),
        }
    };
    assert_eq!(first, 1);
    io.fault_after("aux.db-wal", QueuedIoOpKind::Sync, 0);
    let fault = writer.execute("UPDATE aux.t1 SET a=2 WHERE a=1");
    io.clear_fault();
    let next = cursor.step();
    let new_query = values(&reader);
    eprintln!("adversarial pinned cursor: writer={fault:?}; cursor-next={next:?}; new-query={new_query:?}");
    assert!(matches!(
        fault,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    assert!(
        matches!(next, Err(turso_core::LimboError::CommitUncertain)),
        "already-open cursor returned row after uncertainty: {next:?}"
    );
    assert!(matches!(
        new_query,
        Err(turso_core::LimboError::CommitUncertain)
    ));
    Ok(())
}
