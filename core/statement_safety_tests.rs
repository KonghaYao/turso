use super::*;
use crate::io::clock::{MonotonicInstant, WallClockInstant};
use crate::io::{Buffer, Clock, File, FileSyncType, IO};
use crate::types::IOCompletions;
use crate::{Completion, Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn connection() -> Result<Arc<Connection>> {
    Database::open_file_with_flags(
        Arc::new(MemoryIO::new()),
        ":memory:",
        OpenFlags::Create,
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )?
    .connect()
}

fn checked_row(statement: &mut Statement) -> Result<()> {
    for _attempt in 0..1000 {
        match statement.step_with_admission_check(|_| true)? {
            StepResult::Row => return Ok(()),
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            result => panic!("expected a row, got {result:?}"),
        }
    }
    panic!("statement did not yield a row")
}

fn checked_done(statement: &mut Statement) -> Result<()> {
    for _attempt in 0..1000 {
        match statement.step_with_admission_check(|_| true)? {
            StepResult::Done => return Ok(()),
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            StepResult::Row => {}
            result => panic!("expected completion, got {result:?}"),
        }
    }
    panic!("statement did not complete")
}

fn changed_view_statement(connection: &Arc<Connection>) -> Result<Statement> {
    connection.execute(
        "CREATE SEQUENCE gate_sequence START 1; CREATE VIEW gate_view AS SELECT 1 AS value;",
    )?;
    let statement = connection.prepare("SELECT value FROM gate_view")?;
    assert!(statement.program.is_readonly());
    connection.execute(
        "DROP VIEW gate_view; CREATE VIEW gate_view AS SELECT nextval('gate_sequence') AS value;",
    )?;
    Ok(statement)
}

#[test]
fn prepare_context_reprepare_checks_new_program_before_execution() -> Result<()> {
    let connection = connection()?;
    let mut statement = changed_view_statement(&connection)?;
    connection.set_cache_size(2001);
    let mut classes = Vec::new();
    let result = statement.step_with_admission_check(|readonly| {
        classes.push(readonly);
        readonly
    })?;
    assert!(matches!(result, StepResult::Yield));
    assert_eq!(classes, vec![true, false]);
    assert_eq!(statement.state.pc, 0);
    assert!(!connection.is_in_write_tx());
    checked_row(&mut statement)?;
    assert_eq!(statement.row().expect("sequence row").get::<i64>(0)?, 1);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

#[test]
fn schema_retry_checks_new_program_before_execution() -> Result<()> {
    let connection = connection()?;
    let mut statement = changed_view_statement(&connection)?;
    let mut classes = Vec::new();
    let mut callback = |readonly| {
        classes.push(readonly);
        readonly
    };
    let result = statement.finish_step(
        Err(Box::new(LimboError::SchemaUpdated)),
        None,
        &mut Some(&mut callback),
    )?;
    assert!(matches!(result, StepResult::Yield));
    assert_eq!(classes, vec![false]);
    assert_eq!(statement.state.pc, 0);
    assert!(!connection.is_in_write_tx());
    checked_row(&mut statement)?;
    assert_eq!(statement.row().expect("sequence row").get::<i64>(0)?, 1);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

#[test]
fn abort_waits_for_taken_io_without_resuming_sql() -> Result<()> {
    let connection = connection()?;
    connection.execute("CREATE TABLE ledger(value)")?;
    let mut statement = connection.prepare("INSERT INTO ledger VALUES(1) RETURNING value")?;
    checked_row(&mut statement)?;
    let completion = Completion::new_write(|_| {});
    statement.state.io_completions = Some(IOCompletions(completion.clone()));
    let _issued = statement.take_io_completions().expect("issued IO");
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::PendingIo
    );
    assert!(statement.reset().is_err());
    assert!(statement.step().is_err());
    assert!(connection.is_in_write_tx());
    completion.complete(0);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    drop(statement);
    let mut count = connection.prepare("SELECT count(*) FROM ledger")?;
    checked_row(&mut count)?;
    assert_eq!(count.row().expect("count row").get::<i64>(0)?, 0);
    Ok(())
}

#[test]
fn commit_finalization_remains_visible_after_native_done() -> Result<()> {
    let connection = connection()?;
    connection.execute("CREATE TABLE ledger(value)")?;
    let mut statement = connection.prepare("INSERT INTO ledger VALUES(1)")?;
    checked_done(&mut statement)?;
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::FinalizationAlreadyStarted
    );
    assert!(statement.reset().is_err());
    assert!(statement.step().is_err());
    Ok(())
}

#[test]
fn checkpoint_and_journal_mode_getter_remain_supported() -> Result<()> {
    let connection = connection()?;
    connection.execute("CREATE TABLE ledger(value); INSERT INTO ledger VALUES(1)")?;
    let mut checkpoint = connection.prepare("PRAGMA wal_checkpoint(TRUNCATE)")?;
    checked_row(&mut checkpoint)?;
    assert!(checkpoint.state.noncommitting_finalization_started);
    checked_done(&mut checkpoint)?;
    let mut journal_mode = connection.prepare("PRAGMA journal_mode")?;
    checked_row(&mut journal_mode)?;
    assert_eq!(
        journal_mode.row().expect("journal mode").get::<&str>(0)?,
        "wal"
    );
    checked_done(&mut journal_mode)?;
    let mut journal_noop = connection.prepare("PRAGMA journal_mode = WAL")?;
    checked_row(&mut journal_noop)?;
    checked_done(&mut journal_noop)?;
    Ok(())
}

#[test]
fn explain_write_program_is_effectively_readonly() -> Result<()> {
    let connection = connection()?;
    connection.execute("CREATE TABLE ledger(value)")?;
    let mut statement = connection.prepare("EXPLAIN INSERT INTO ledger VALUES(1)")?;
    let result = statement.step_with_admission_check(|readonly| {
        assert!(readonly);
        true
    })?;
    assert!(matches!(result, StepResult::Row));
    assert!(!connection.is_in_write_tx());
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

#[test]
fn root_ddl_trigger_and_returning_abort_share_native_rollback() -> Result<()> {
    let connection = connection()?;
    for sql in [
        "CREATE TABLE ledger(value)",
        "CREATE TABLE audit(value)",
        "CREATE TRIGGER log_value AFTER INSERT ON ledger BEGIN INSERT INTO audit VALUES(new.value); END",
    ] {
        checked_done(&mut connection.prepare(sql)?)?;
    }
    let mut statement = connection.prepare("INSERT INTO ledger VALUES(1) RETURNING value")?;
    checked_row(&mut statement)?;
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    drop(statement);
    let mut count = connection
        .prepare("SELECT (SELECT count(*) FROM ledger) + (SELECT count(*) FROM audit)")?;
    checked_row(&mut count)?;
    assert_eq!(count.row().expect("count row").get::<i64>(0)?, 0);
    Ok(())
}

#[test]
fn mvcc_assignment_is_rejected_before_callback_or_opcode() -> Result<()> {
    let connection = connection()?;
    let mut statement = connection.prepare("PRAGMA journal_mode = 'mvcc'")?;
    let result = statement.step_with_admission_check(|_| panic!("unsupported program admitted"));
    assert!(matches!(result, Err(LimboError::InvalidArgument(_))));
    assert_eq!(statement.state.pc, 0);
    assert!(!connection.is_in_write_tx());
    Ok(())
}

#[test]
fn native_constraint_keeps_explicit_writer_and_committed_prefix() -> Result<()> {
    let connection = connection()?;
    connection.execute(
        "CREATE TABLE ledger(value PRIMARY KEY); INSERT INTO ledger VALUES(10); BEGIN; INSERT INTO ledger VALUES(20);",
    )?;
    let mut statement = connection.prepare("INSERT INTO ledger VALUES(20) RETURNING value")?;
    assert!(matches!(
        checked_done(&mut statement),
        Err(LimboError::Constraint(_))
    ));
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    drop(statement);
    assert!(!connection.get_auto_commit());
    assert!(connection.is_in_write_tx());
    connection.execute("ROLLBACK")?;
    let mut count = connection.prepare("SELECT sum(value) FROM ledger")?;
    checked_row(&mut count)?;
    assert_eq!(count.row().expect("prefix sum").get::<i64>(0)?, 10);
    Ok(())
}

enum HeldIo {
    Read(Arc<dyn File>, u64, Completion),
    Sync(Arc<dyn File>, Completion, FileSyncType),
}

#[derive(Default)]
struct RetirementControl {
    hold_sync: AtomicBool,
    hold_read: AtomicBool,
    disabled: AtomicBool,
    submissions: AtomicUsize,
    pending: Mutex<Option<HeldIo>>,
}

struct RetirementIo {
    inner: MemoryIO,
    control: Arc<RetirementControl>,
}

impl Clock for RetirementIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for RetirementIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        Ok(Arc::new(RetirementFile {
            inner: self.inner.open_file(path, flags, direct)?,
            control: self.control.clone(),
        }))
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn file_id(&self, path: &str) -> Result<crate::io::FileId> {
        self.inner.file_id(path)
    }
}

struct RetirementFile {
    inner: Arc<dyn File>,
    control: Arc<RetirementControl>,
}

impl RetirementFile {
    fn submission(&self) {
        self.control.submissions.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.control.disabled.load(Ordering::SeqCst),
            "new IO after retirement guard"
        );
    }
}

impl File for RetirementFile {
    fn lock_file(&self, exclusive: bool) -> Result<()> {
        self.inner.lock_file(exclusive)
    }
    fn unlock_file(&self) -> Result<()> {
        self.inner.unlock_file()
    }
    fn size(&self) -> Result<u64> {
        self.inner.size()
    }
    fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
        self.submission();
        if self.control.hold_read.swap(false, Ordering::SeqCst) {
            *self.control.pending.lock().expect("pending IO") =
                Some(HeldIo::Read(self.inner.clone(), pos, completion.clone()));
            return Ok(completion);
        }
        self.inner.pread(pos, completion)
    }
    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, completion: Completion) -> Result<Completion> {
        self.submission();
        self.inner.pwrite(pos, buffer, completion)
    }
    fn sync(&self, completion: Completion, mode: FileSyncType) -> Result<Completion> {
        self.submission();
        if self.control.hold_sync.swap(false, Ordering::SeqCst) {
            *self.control.pending.lock().expect("pending IO") =
                Some(HeldIo::Sync(self.inner.clone(), completion.clone(), mode));
            return Ok(completion);
        }
        self.inner.sync(completion, mode)
    }
    fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
        self.submission();
        self.inner.truncate(len, completion)
    }
}

struct TestRetirementGuard(Arc<dyn IO>);

impl NativeIoRetirementGuard for TestRetirementGuard {
    fn io(&self) -> &Arc<dyn IO> {
        &self.0
    }
}

fn held_io_retirement(sql: &str, fail_read: bool) -> Result<()> {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    let path = format!(
        "native-retirement-{}.db",
        NEXT_ID.fetch_add(1, Ordering::SeqCst)
    );
    let control = Arc::new(RetirementControl::default());
    let io: Arc<dyn IO> = Arc::new(RetirementIo {
        inner: MemoryIO::new(),
        control: control.clone(),
    });
    let database = Database::open_file_with_flags(
        io.clone(),
        &path,
        OpenFlags::Create,
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )?;
    let connection = database.connect()?;
    connection.set_sync_mode(crate::SyncMode::Full);
    connection.execute("CREATE TABLE ledger(value); INSERT INTO ledger VALUES(10)")?;
    let mut sibling = if sql.starts_with("INSERT") {
        let mut reader = connection.prepare("SELECT value FROM ledger")?;
        checked_row(&mut reader)?;
        Some(reader)
    } else {
        None
    };
    let mut statement = connection.prepare(sql)?;
    let inactive = connection.prepare("SELECT value FROM ledger")?;
    let mut inactive_write = connection.prepare("INSERT INTO ledger VALUES(99)")?;
    let cloned_connection = connection.clone();
    if fail_read {
        connection.get_pager().clear_page_cache(false);
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
        if fail_read {
            NoncommittingAbort::PendingIo
        } else {
            NoncommittingAbort::FinalizationAlreadyStarted
        }
    );
    let held = control
        .pending
        .lock()
        .expect("pending IO")
        .take()
        .expect("real issued IO");
    match held {
        HeldIo::Read(_, _, completion) if fail_read => {
            completion.error(crate::CompletionError::Aborted)
        }
        HeldIo::Read(file, pos, completion) => {
            let _ = file.pread(pos, completion)?;
        }
        HeldIo::Sync(file, completion, mode) => {
            let _ = file.sync(completion, mode)?;
        }
    }
    if fail_read {
        assert!(matches!(
            statement.abort_noncommitting(),
            Err(LimboError::CompletionError(crate::CompletionError::Aborted))
        ));
    }
    control.disabled.store(true, Ordering::SeqCst);
    let guard = TestRetirementGuard(io);
    let before = control.submissions.load(Ordering::SeqCst);
    let foreign = TestRetirementGuard(Arc::new(MemoryIO::new()));
    assert!(matches!(
        statement.retire_noncommitting(&foreign),
        Err(LimboError::InvalidArgument(_))
    ));
    if let Some(mut reader) = sibling.take() {
        assert!(matches!(
            statement.retire_noncommitting(&guard),
            Err(LimboError::StatementsInProgress(_))
        ));
        assert!(!connection.is_closed());
        assert_eq!(database.n_connections.load(Ordering::SeqCst), 1);
        assert_eq!(
            reader.abort_noncommitting()?,
            NoncommittingAbort::StatementStopped
        );
    }
    let NoncommittingRetirement::RetiredUnsettled(errors) =
        statement.retire_noncommitting(&guard)?
    else {
        panic!("local IO already drained")
    };
    assert_eq!(errors.abort_error.is_some(), fail_read);
    assert!(errors.cleanup_error.is_none());
    assert!(matches!(
        statement.retire_noncommitting(&guard)?,
        NoncommittingRetirement::RetiredUnsettled(_)
    ));
    assert!(statement.step().is_err());
    let write_step = inactive_write.step();
    let write_reset = inactive_write.reset();
    drop(inactive_write);
    let clone_close = cloned_connection.close();
    assert!(connection.is_closed());
    assert!(connection.prepare("INSERT INTO ledger VALUES(30)").is_err());
    assert!(!connection.get_pager().holds_write_lock());
    assert!(!connection.get_pager().holds_read_lock());
    assert!(!connection.get_pager().is_checkpointing());
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        0
    );
    assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
    let weak = Arc::downgrade(&connection);
    drop(statement);
    drop(inactive);
    drop(connection);
    drop(cloned_connection);
    assert!(weak.upgrade().is_none());
    assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
    assert_eq!(control.submissions.load(Ordering::SeqCst), before);
    let closed_error = write_step.expect_err("closed").to_string();
    assert_eq!(closed_error, "Internal error: Connection closed");
    write_reset?;
    clone_close?;
    Ok(())
}

#[test]
fn late_commit_retires_without_submitting_more_io() -> Result<()> {
    held_io_retirement("INSERT INTO ledger VALUES(20)", false)
}

#[test]
fn late_checkpoint_retires_without_submitting_more_io() -> Result<()> {
    held_io_retirement("PRAGMA wal_checkpoint(TRUNCATE)", false)
}

#[test]
fn failed_abort_retires_without_submitting_more_io() -> Result<()> {
    held_io_retirement("SELECT value FROM ledger", true)
}
