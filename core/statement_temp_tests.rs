use super::temp_test_io::{Control, Held, MainIo};
use super::*;
use crate::{
    Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, TempStore, IO,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn connection() -> Result<Arc<Connection>> {
    let connection = Database::open_file_with_flags(
        Arc::new(MemoryIO::new()),
        ":memory:",
        OpenFlags::Create,
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )?
    .connect()?;
    connection.set_temp_store(TempStore::Memory);
    connection.execute("CREATE TABLE ledger(value PRIMARY KEY); INSERT INTO ledger VALUES(10)")?;
    Ok(connection)
}

fn advance(statement: &mut Statement, expected: StepResult) -> Result<()> {
    for _attempt in 0..1000 {
        let result = statement.step_with_admission_check(|_| true)?;
        if std::mem::discriminant(&result) == std::mem::discriminant(&expected) {
            return Ok(());
        }
        match result {
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            result => panic!("expected {expected:?}, got {result:?}"),
        }
    }
    panic!("statement did not advance")
}

fn execute(connection: &Arc<Connection>, sql: &str) -> Result<()> {
    advance(&mut connection.prepare(sql)?, StepResult::Done)
}

fn reject(statement: &mut Statement) {
    assert!(matches!(
        statement.step_with_admission_check(|_| panic!("denied profile reached gate")),
        Err(LimboError::InvalidArgument(_))
    ));
}

fn assert_unlocked(connection: &Connection) {
    assert!(!connection.pager.load().holds_read_lock());
    assert!(!connection.pager.load().holds_write_lock());
    assert!(!connection.pager.load().is_checkpointing());
    if let Some(temp) = connection.temp.database.read().as_ref() {
        assert!(!temp.pager.holds_read_lock());
        assert!(!temp.pager.holds_write_lock());
        assert!(!temp.pager.is_checkpointing());
    }
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        0
    );
    assert_eq!(connection.n_active_writes.load(Ordering::SeqCst), 0);
}

#[test]
fn internal_temp_immediate_exclusive_commit_rollback() -> Result<()> {
    for begin in ["BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"] {
        for end in ["COMMIT", "ROLLBACK"] {
            let connection = connection()?;
            execute(&connection, begin)?;
            connection.validate_internal_temp(true)?;
            assert!(connection.pager.load().holds_write_lock());
            assert!(connection
                .temp
                .database
                .read()
                .as_ref()
                .unwrap()
                .pager
                .holds_write_lock());
            execute(&connection, "INSERT INTO ledger VALUES(20)")?;
            execute(&connection, end)?;
            assert_unlocked(&connection);
            let mut sum = connection.prepare("SELECT sum(value) FROM ledger")?;
            advance(&mut sum, StepResult::Row)?;
            assert_eq!(
                sum.row().unwrap().get::<i64>(0)?,
                if end == "COMMIT" { 30 } else { 10 }
            );
            assert_eq!(
                sum.abort_noncommitting()?,
                NoncommittingAbort::StatementStopped
            );
        }
    }
    Ok(())
}

#[test]
fn internal_temp_returning_abort_preserves_explicit_prefix() -> Result<()> {
    let connection = connection()?;
    execute(&connection, "BEGIN IMMEDIATE")?;
    execute(&connection, "INSERT INTO ledger VALUES(20)")?;
    let mut statement = connection.prepare("INSERT INTO ledger VALUES(30),(40) RETURNING value")?;
    advance(&mut statement, StepResult::Row)?;
    assert_eq!(statement.row().unwrap().get::<i64>(0)?, 30);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    drop(statement);
    assert!(!connection.get_auto_commit());
    assert!(connection.pager.load().holds_write_lock());
    execute(&connection, "COMMIT")?;
    let mut sum = connection.prepare("SELECT sum(value) FROM ledger")?;
    advance(&mut sum, StepResult::Row)?;
    assert_eq!(sum.row().unwrap().get::<i64>(0)?, 30);
    assert_eq!(
        sum.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

#[test]
fn internal_temp_gate_pause_and_reprepare_have_zero_transaction_effects() -> Result<()> {
    let connection = connection()?;
    let mut statement = connection.prepare("BEGIN EXCLUSIVE")?;
    assert!(matches!(
        statement.step_with_admission_check(|_| false)?,
        StepResult::Yield
    ));
    assert_eq!(statement.state.pc, 0);
    assert!(matches!(statement.cleanup_state, CleanupState::Unentered));
    assert!(connection.temp.database.read().is_none());
    assert_unlocked(&connection);
    connection.set_cache_size(2001);
    let mut calls = 0;
    assert!(matches!(
        statement.step_with_admission_check(|_| {
            calls += 1;
            calls == 1
        })?,
        StepResult::Yield
    ));
    assert_eq!(calls, 2);
    assert!(statement.state.metrics.reprepares > 0);
    assert!(statement.state.internal_temp_certificate.is_none());
    assert_eq!(statement.state.pc, 0);
    assert!(connection.temp.database.read().is_none());
    assert!(!connection.pager.load().holds_read_lock());
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    assert_unlocked(&connection);
    let mut changed = connection.prepare("BEGIN IMMEDIATE")?;
    assert!(matches!(
        changed.step_with_admission_check(|_| {
            connection.set_temp_store(TempStore::File);
            true
        }),
        Err(LimboError::InvalidArgument(_))
    ));
    assert_eq!(changed.state.pc, 0);
    assert!(connection.temp.database.read().is_none());
    assert_unlocked(&connection);
    Ok(())
}

#[test]
fn internal_temp_default_file_and_existing_memory_are_not_certificates() -> Result<()> {
    for selector in [TempStore::Default, TempStore::File] {
        let connection = connection()?;
        connection.set_temp_store(selector);
        let mut statement = connection.prepare("BEGIN IMMEDIATE")?;
        reject(&mut statement);
        assert_eq!(statement.state.pc, 0);
        assert!(connection.temp.database.read().is_none());
        assert_unlocked(&connection);
    }
    let connection = connection()?;
    connection.ensure_temp_database()?;
    let mut statement = connection.prepare("BEGIN EXCLUSIVE")?;
    reject(&mut statement);
    assert_eq!(statement.state.pc, 0);
    assert_unlocked(&connection);
    Ok(())
}

#[test]
fn internal_temp_rejects_user_schema_data_vtab_and_incomplete_begin_bytecode() -> Result<()> {
    let connection = connection()?;
    for sql in [
        "CREATE TEMP TABLE private(value)",
        "SELECT * FROM temp.sqlite_schema",
        "SELECT * FROM pragma_table_info('ledger')",
        "PRAGMA temp.wal_checkpoint",
    ] {
        let mut statement = connection.prepare(sql)?;
        reject(&mut statement);
        assert_eq!(statement.state.pc, 0);
        assert!(connection.temp.database.read().is_none());
    }
    let mut statement = connection.prepare("BEGIN IMMEDIATE")?;
    Arc::make_mut(&mut statement.program.prepared)
        .insns
        .push((crate::vdbe::insn::Insn::Noop, 6));
    reject(&mut statement);
    assert_eq!(statement.state.pc, 0);
    assert_unlocked(&connection);
    Ok(())
}

struct Guard(Arc<dyn IO>);
impl NativeIoRetirementGuard for Guard {
    fn io(&self) -> &Arc<dyn IO> {
        &self.0
    }
}

#[test]
fn internal_temp_setter_and_legacy_revoke_execution_but_allow_cleanup() -> Result<()> {
    for legacy in [false, true] {
        let connection = connection()?;
        execute(&connection, "BEGIN IMMEDIATE")?;
        let mut reader = connection.prepare("SELECT value FROM ledger")?;
        advance(&mut reader, StepResult::Row)?;
        if legacy {
            connection.execute("SELECT 1")?;
        } else {
            connection.set_temp_store(TempStore::Memory);
        }
        reject(&mut reader);
        assert!(matches!(reader.cleanup_state, CleanupState::Checked));
        assert_eq!(
            reader.abort_noncommitting()?,
            NoncommittingAbort::StatementStopped
        );
        drop(reader);
        let guard = Guard(connection.db.io.clone());
        assert!(connection.verify_noncommitting_retired(&guard).is_err());
        assert!(matches!(
            connection.retire_inactive_noncommitting(&guard)?,
            NoncommittingRetirement::RetiredUnsettled(_)
        ));
        connection.verify_noncommitting_retired(&guard)?;
        assert_unlocked(&connection);
    }
    Ok(())
}

#[test]
fn internal_temp_legacy_begin_is_never_adopted() -> Result<()> {
    let connection = connection()?;
    connection.execute("BEGIN IMMEDIATE")?;
    let mut statement = connection.prepare("SELECT value FROM ledger")?;
    reject(&mut statement);
    assert_eq!(statement.state.pc, 0);
    assert!(connection.validate_internal_temp(false).is_err());
    connection.execute("ROLLBACK")?;
    Ok(())
}

#[test]
fn internal_temp_attached_and_mvcc_are_denied_before_begin() -> Result<()> {
    for setup in ["ATTACH ':memory:' AS aux", "PRAGMA journal_mode = 'mvcc'"] {
        let connection = Database::open_file_with_flags(
            Arc::new(MemoryIO::new()),
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new().with_attach(true),
            None,
            Arc::new(SqliteDialect),
        )?
        .connect()?;
        connection.set_temp_store(TempStore::Memory);
        connection.execute(setup)?;
        let mut statement = connection.prepare("BEGIN IMMEDIATE")?;
        reject(&mut statement);
        assert_eq!(statement.state.pc, 0);
        assert!(connection.temp.database.read().is_none());
        assert_unlocked(&connection);
    }
    Ok(())
}

#[test]
fn internal_temp_changed_identity_and_schema_fail_execution_proof() -> Result<()> {
    let connection = connection()?;
    execute(&connection, "BEGIN IMMEDIATE")?;
    execute(&connection, "ROLLBACK")?;
    {
        let mut guard = connection.temp.database.write();
        let temp = guard.as_mut().unwrap();
        let original = temp.pager.clone();
        temp.pager = Arc::new(temp.db._init(None, None)?);
        assert!(!Arc::ptr_eq(&temp.pager, &original));
        drop(guard);
        assert!(connection.validate_internal_temp(false).is_err());
        connection.temp.database.write().as_mut().unwrap().pager = original;
    }
    connection.validate_internal_temp(true)?;
    let guard = connection.temp.database.read();
    let temp = guard.as_ref().unwrap();
    *temp.db.schema.lock() = connection.empty_temp_schema();
    drop(guard);
    assert!(connection.validate_internal_temp(true).is_err());
    connection.validate_internal_temp(false)?;
    let guard = Guard(connection.db.io.clone());
    assert!(matches!(
        connection.retire_inactive_noncommitting(&guard)?,
        NoncommittingRetirement::RetiredUnsettled(_)
    ));
    connection.verify_noncommitting_retired(&guard)
}

#[test]
fn internal_temp_deferred_promotion_and_real_busy_snapshot() -> Result<()> {
    let connection = connection()?;
    execute(&connection, "BEGIN IMMEDIATE")?;
    execute(&connection, "ROLLBACK")?;
    execute(&connection, "BEGIN")?;
    execute(&connection, "INSERT INTO ledger VALUES(20)")?;
    execute(&connection, "COMMIT")?;
    execute(&connection, "BEGIN")?;
    let mut reader = connection.prepare("SELECT value FROM ledger")?;
    advance(&mut reader, StepResult::Row)?;
    assert_eq!(
        reader.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    drop(reader);
    let writer = connection.db.connect()?;
    writer.execute("INSERT INTO ledger VALUES(30)")?;
    let mut promotion = connection.prepare("INSERT INTO ledger VALUES(40)")?;
    assert!(matches!(
        advance(&mut promotion, StepResult::Done),
        Err(LimboError::BusySnapshot)
    ));
    assert_eq!(
        promotion.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    assert!(!connection.get_auto_commit());
    execute(&connection, "ROLLBACK")?;
    assert_unlocked(&connection);
    Ok(())
}

fn retire_with_auxiliary(read: bool, failure: bool) -> Result<()> {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    let control = Arc::new(Control::default());
    let io: Arc<dyn IO> = Arc::new(MainIo {
        memory: MemoryIO::new(),
        control: control.clone(),
    });
    let database = Database::open_file_with_flags(
        io.clone(),
        &format!(
            "temp-retirement-{}.db",
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        ),
        OpenFlags::Create,
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )?;
    let connection = database.connect()?;
    connection.set_temp_store(TempStore::Memory);
    connection.set_sync_mode(crate::SyncMode::Full);
    connection.execute("CREATE TABLE ledger(value); INSERT INTO ledger VALUES(10)")?;
    execute(&connection, "BEGIN IMMEDIATE")?;
    if !read {
        execute(&connection, "INSERT INTO ledger VALUES(20)")?;
    }
    let (weak_temp_db, weak_temp_pager) = {
        let guard = connection.temp.database.read();
        let temp = guard.as_ref().unwrap();
        (Arc::downgrade(&temp.db), Arc::downgrade(&temp.pager))
    };
    let mut statement = connection.prepare(if read {
        "SELECT value FROM ledger"
    } else {
        "COMMIT"
    })?;
    if read {
        connection.pager.load().clear_page_cache(false);
        control.hold_read.store(true, Ordering::SeqCst);
    } else {
        control.hold_sync.store(true, Ordering::SeqCst);
    }
    assert!(matches!(
        statement.step_with_admission_check(|_| true)?,
        StepResult::IO
    ));
    assert_eq!(
        statement.abort_noncommitting()?,
        if read {
            NoncommittingAbort::PendingIo
        } else {
            NoncommittingAbort::FinalizationAlreadyStarted
        }
    );
    let guard = Guard(io);
    control.disabled.store(true, Ordering::SeqCst);
    let before = control.calls.load(Ordering::SeqCst);
    assert!(matches!(
        statement.retire_noncommitting(&guard)?,
        NoncommittingRetirement::PendingIo
    ));
    assert!(connection.verify_noncommitting_retired(&guard).is_err());
    let held = control
        .pending
        .lock()
        .unwrap()
        .take()
        .expect("real MAIN completion");
    match held {
        Held::Read(_, _, completion) | Held::Sync(_, completion, _) if failure => {
            completion.error(crate::CompletionError::Aborted)
        }
        Held::Read(file, pos, completion) => {
            let _ = file.pread(pos, completion)?;
        }
        Held::Sync(file, completion, mode) => {
            let _ = file.sync(completion, mode)?;
        }
    }
    if failure && read {
        assert!(statement.abort_noncommitting().is_err());
    }
    let NoncommittingRetirement::RetiredUnsettled(errors) =
        statement.retire_noncommitting(&guard)?
    else {
        panic!("completed IO must retire")
    };
    assert_eq!(errors.io_error.is_some(), failure);
    assert_eq!(errors.abort_error.is_some(), failure && read);
    assert!(errors.cleanup_error.is_none());
    connection.verify_noncommitting_retired(&guard)?;
    assert!(connection
        .verify_noncommitting_retired(&Guard(Arc::new(MemoryIO::new())))
        .is_err());
    assert_unlocked(&connection);
    assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
    assert!(matches!(
        statement.retire_noncommitting(&guard)?,
        NoncommittingRetirement::RetiredUnsettled(_)
    ));
    let weak_connection = Arc::downgrade(&connection);
    let weak_database = Arc::downgrade(&database);
    drop(statement);
    drop(connection);
    drop(database);
    assert!(weak_connection.upgrade().is_none());
    assert!(weak_database.upgrade().is_none());
    assert!(weak_temp_db.upgrade().is_none());
    assert!(weak_temp_pager.upgrade().is_none());
    assert_eq!(control.calls.load(Ordering::SeqCst), before);
    Ok(())
}

#[test]
fn internal_temp_real_main_pending_read_failure_releases_both_pagers() -> Result<()> {
    retire_with_auxiliary(true, true)
}

#[test]
fn internal_temp_real_main_pending_read_completion_releases_both_pagers() -> Result<()> {
    retire_with_auxiliary(true, false)
}

#[test]
fn internal_temp_real_main_late_commit_releases_both_pagers() -> Result<()> {
    retire_with_auxiliary(false, false)
}

#[test]
fn internal_temp_real_main_late_commit_failure_preserves_unknown() -> Result<()> {
    retire_with_auxiliary(false, true)
}
