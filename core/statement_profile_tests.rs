use super::*;
use crate::types::IOCompletions;
use crate::{
    Completion, Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, IO,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;

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

fn row(statement: &mut Statement) -> Result<()> {
    for _attempt in 0..1000 {
        match statement.step_with_admission_check(|_| true)? {
            StepResult::Row => return Ok(()),
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            result => panic!("expected row, got {result:?}"),
        }
    }
    panic!("no row")
}

fn prefix(connection: &Arc<Connection>) -> Result<()> {
    connection.execute(
        "CREATE TABLE ledger(value); INSERT INTO ledger VALUES(10); \
         CREATE VIEW gate_view AS SELECT value FROM ledger; \
         BEGIN; INSERT INTO ledger VALUES(20)",
    )
}

fn replace_view(connection: &Arc<Connection>) -> Result<()> {
    connection.execute(
        "DROP VIEW gate_view; CREATE VIEW gate_view AS \
         SELECT load_extension('/never-load-rejected-profile') AS value FROM ledger",
    )
}

fn verify_stopped(statement: &mut Statement, connection: &Arc<Connection>) -> Result<()> {
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    assert!(statement.step().is_err());
    statement.reset()?;
    assert!(!connection.get_auto_commit());
    assert!(connection.is_in_write_tx());
    assert!(connection.pager.load().holds_read_lock());
    assert!(connection.pager.load().holds_write_lock());
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        0
    );
    assert_eq!(connection.n_active_writes.load(Ordering::SeqCst), 0);
    Ok(())
}

fn verify_prefix(connection: &Arc<Connection>) -> Result<()> {
    connection.execute("ROLLBACK")?;
    let mut statement = connection.prepare("SELECT sum(value) FROM ledger")?;
    row(&mut statement)?;
    assert_eq!(statement.row().expect("prefix row").get::<i64>(0)?, 10);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

fn rejected(statement: &mut Statement) {
    let error = statement
        .step_with_admission_check(|_| panic!("unsupported program reached callback"))
        .expect_err("profile must reject");
    assert!(error.to_string().contains("noncommitting admission"));
}

#[test]
fn initial_profile_rejection_stops_without_touching_existing_writer() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    let mut statement = connection.prepare("PRAGMA journal_mode = 'mvcc'")?;
    rejected(&mut statement);
    assert!(matches!(statement.cleanup_state, CleanupState::Unentered));
    verify_stopped(&mut statement, &connection)?;
    drop(statement);
    verify_prefix(&connection)
}

#[test]
fn prepare_context_rejection_cleans_only_reset_replacement() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    let mut statement = connection.prepare("SELECT value FROM gate_view")?;
    replace_view(&connection)?;
    connection.set_cache_size(2001);
    let mut callbacks = 0;
    let error = statement
        .step_with_admission_check(|readonly| {
            callbacks += 1;
            assert!(readonly);
            true
        })
        .expect_err("replacement must reject");
    assert!(error.to_string().contains("extension loading"));
    assert_eq!(callbacks, 1);
    assert!(statement.state.metrics.reprepares > 0);
    assert!(matches!(statement.cleanup_state, CleanupState::Unentered));
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        1
    );
    verify_stopped(&mut statement, &connection)?;
    drop(statement);
    verify_prefix(&connection)
}

#[test]
fn schema_retry_rejection_preserves_live_read_snapshot_and_writer() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    let mut statement = connection.prepare("SELECT value FROM gate_view")?;
    row(&mut statement)?;
    assert_eq!(statement.row().expect("old view row").get::<i64>(0)?, 10);
    assert!(matches!(statement.cleanup_state, CleanupState::Checked));
    assert!(connection.pager.load().holds_read_lock());
    replace_view(&connection)?;
    let mut callback = |_| panic!("replacement must not be admitted");
    let error = statement
        .finish_step(
            Err(Box::new(LimboError::SchemaUpdated)),
            None,
            &mut Some(&mut callback),
        )
        .expect_err("replacement must reject");
    assert!(error.to_string().contains("extension loading"));
    assert!(statement.state.metrics.reprepares > 0);
    assert!(matches!(statement.cleanup_state, CleanupState::Unentered));
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        1
    );
    verify_stopped(&mut statement, &connection)?;
    drop(statement);
    verify_prefix(&connection)
}

struct SynchronousMemoryGuard(Arc<dyn IO>);

impl NativeIoRetirementGuard for SynchronousMemoryGuard {
    fn io(&self) -> &Arc<dyn IO> {
        &self.0
    }
}

#[test]
fn rejected_replacement_preserves_prior_finalization_and_can_retire() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    connection.execute("COMMIT")?;
    let mut statement = connection.prepare("INSERT INTO ledger SELECT value FROM gate_view")?;
    loop {
        match statement.step_with_admission_check(|_| true)? {
            StepResult::Done => break,
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            result => panic!("expected completion, got {result:?}"),
        }
    }
    assert!(statement.state.noncommitting_finalization_started);
    replace_view(&connection)?;
    statement.reprepare()?;
    rejected(&mut statement);
    assert!(matches!(statement.cleanup_state, CleanupState::Unentered));
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::FinalizationAlreadyStarted
    );
    let guard = SynchronousMemoryGuard(statement.pager.io.clone());
    let database = connection.db.clone();
    let weak = Arc::downgrade(&connection);
    for _attempt in 0..2 {
        assert!(matches!(
            statement.retire_noncommitting(&guard)?,
            NoncommittingRetirement::RetiredUnsettled(_)
        ));
        assert_eq!(
            connection.n_active_root_statements.load(Ordering::SeqCst),
            0
        );
        assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
    }
    drop(statement);
    drop(connection);
    assert!(weak.upgrade().is_none());
    let reader = database.connect()?;
    let mut statement = reader.prepare("SELECT sum(value) FROM ledger")?;
    row(&mut statement)?;
    assert_eq!(statement.row().expect("committed row").get::<i64>(0)?, 60);
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    Ok(())
}

#[test]
fn unentered_rejection_does_not_discard_detached_io_failure() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    let mut statement = connection.prepare("PRAGMA journal_mode = 'mvcc'")?;
    rejected(&mut statement);
    let completion = Completion::new_write(|_| {});
    statement.state.io_completions = Some(IOCompletions(completion.clone()));
    let _issued = statement.take_io_completions().expect("detached IO");
    assert_eq!(
        statement.abort_noncommitting()?,
        NoncommittingAbort::PendingIo
    );
    completion.error(crate::CompletionError::Aborted);
    let original = statement.abort_noncommitting().expect_err("IO failure");
    let guard = SynchronousMemoryGuard(statement.pager.io.clone());
    let result = statement.retire_noncommitting(&guard)?;
    let NoncommittingRetirement::RetiredUnsettled(errors) = result else {
        panic!("finished local callback must retire");
    };
    assert_eq!(
        errors.abort_error.expect("original error").to_string(),
        original.to_string()
    );
    assert!(errors.io_error.is_some());
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        0
    );
    assert_eq!(connection.db.n_connections.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn failed_internal_reset_is_not_an_unentered_proof() -> Result<()> {
    let connection = connection()?;
    prefix(&connection)?;
    let mut statement = connection.prepare("SELECT value FROM gate_view")?;
    row(&mut statement)?;
    let completion = Completion::new_write(|_| {});
    completion.error(crate::CompletionError::Aborted);
    statement.state.io_completions = Some(IOCompletions(completion));
    let original = statement.reprepare().expect_err("reset must fail");
    assert!(matches!(statement.cleanup_state, CleanupState::Checked));
    assert_eq!(
        statement
            .abort_noncommitting()
            .expect_err("sticky error")
            .to_string(),
        original.to_string()
    );
    let guard = SynchronousMemoryGuard(statement.pager.io.clone());
    let NoncommittingRetirement::RetiredUnsettled(errors) =
        statement.retire_noncommitting(&guard)?
    else {
        panic!("failed cleanup must retire");
    };
    assert_eq!(
        errors.abort_error.expect("reset error").to_string(),
        original.to_string()
    );
    Ok(())
}

#[test]
fn executed_unsupported_legacy_is_rejected_before_sticky_abort() -> Result<()> {
    let connection = connection()?;
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info('missing')")?;
    let _legacy_result = statement.step();
    assert!(matches!(statement.cleanup_state, CleanupState::Unchecked));
    assert!(statement
        .abort_noncommitting()
        .expect_err("unsupported legacy execution")
        .to_string()
        .contains("unsupported unchecked execution"));
    assert!(matches!(
        statement.noncommitting_abort,
        AbortState::NotRequested
    ));
    statement.reset()?;
    Ok(())
}

#[test]
fn root_subprogram_reset_preserves_rows_finalization_failure_and_counts() -> Result<()> {
    fn rejected_reset(statement: &mut Statement) -> LimboError {
        let execution = std::mem::discriminant(&statement.state.execution_state);
        let abort = std::mem::discriminant(&statement.noncommitting_abort);
        let cleanup = statement.cleanup_state;
        let finalization = statement.state.noncommitting_finalization_started;
        let returned_row = statement.has_returned_row;
        let row = statement.row().map(|row| {
            row.get_values()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        });
        let roots = statement
            .program
            .connection
            .n_active_root_statements
            .load(Ordering::SeqCst);
        let writers = statement
            .program
            .connection
            .n_active_writes
            .load(Ordering::SeqCst);
        let error = statement
            .reset_for_subprogram_reuse()
            .expect_err("root or aborted subprogram reuse must reject");
        assert_eq!(
            std::mem::discriminant(&statement.state.execution_state),
            execution
        );
        assert_eq!(
            std::mem::discriminant(&statement.noncommitting_abort),
            abort
        );
        assert!(statement.cleanup_state == cleanup);
        assert_eq!(
            statement.state.noncommitting_finalization_started,
            finalization
        );
        assert_eq!(statement.has_returned_row, returned_row);
        assert_eq!(
            statement.row().map(|row| {
                row.get_values()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            }),
            row
        );
        assert_eq!(
            statement
                .program
                .connection
                .n_active_root_statements
                .load(Ordering::SeqCst),
            roots
        );
        assert_eq!(
            statement
                .program
                .connection
                .n_active_writes
                .load(Ordering::SeqCst),
            writers
        );
        error
    }

    for finalize in [false, true] {
        let connection = connection()?;
        prefix(&connection)?;
        if finalize {
            connection.execute("COMMIT")?;
        }
        let mut statement = connection.prepare("INSERT INTO ledger VALUES(21) RETURNING value")?;
        row(&mut statement)?;
        assert_eq!(statement.row().expect("returning row").get::<i64>(0)?, 21);
        let error = rejected_reset(&mut statement);
        assert!(matches!(error, LimboError::InvalidArgument(_)));
        assert!(error
            .to_string()
            .contains("requires a subprogram statement"));
        if finalize {
            loop {
                match statement.step_with_admission_check(|_| true)? {
                    StepResult::Done => break,
                    StepResult::IO | StepResult::Yield => statement._io().step()?,
                    result => panic!("expected completion, got {result:?}"),
                }
            }
            assert!(statement.state.noncommitting_finalization_started);
            assert_eq!(
                statement.abort_noncommitting()?,
                NoncommittingAbort::FinalizationAlreadyStarted
            );
            assert!(rejected_reset(&mut statement)
                .to_string()
                .contains("execution is disabled"));
        } else {
            let completion = Completion::new_write(|_| {});
            statement.state.io_completions = Some(IOCompletions(completion.clone()));
            assert_eq!(
                statement.abort_noncommitting()?,
                NoncommittingAbort::PendingIo
            );
            assert!(rejected_reset(&mut statement)
                .to_string()
                .contains("execution is disabled"));
            completion.error(crate::CompletionError::Aborted);
            let original = statement.abort_noncommitting().expect_err("failed IO");
            assert_eq!(
                rejected_reset(&mut statement).to_string(),
                original.to_string()
            );
        }
        let guard = SynchronousMemoryGuard(statement.pager.io.clone());
        assert!(matches!(
            statement.retire_noncommitting(&guard)?,
            NoncommittingRetirement::RetiredUnsettled(_)
        ));
        let _retired_error = rejected_reset(&mut statement);
        assert_eq!(
            connection.n_active_root_statements.load(Ordering::SeqCst),
            0
        );
        assert_eq!(connection.n_active_writes.load(Ordering::SeqCst), 0);
        assert_eq!(connection.db.n_connections.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[test]
fn cached_trigger_subprogram_reuse_still_handles_multiple_rows() -> Result<()> {
    let connection = connection()?;
    connection.execute(
        "CREATE TABLE ledger(value); CREATE TABLE audit(value); \
         CREATE TRIGGER audit_insert AFTER INSERT ON ledger \
         BEGIN INSERT INTO audit VALUES(new.value); END",
    )?;
    let mut statement =
        connection.prepare("INSERT INTO ledger VALUES(1),(2),(3) RETURNING value")?;
    let mut rows = 0;
    loop {
        match statement.step_with_admission_check(|_| true)? {
            StepResult::Row => rows += 1,
            StepResult::Done => break,
            StepResult::IO | StepResult::Yield => statement._io().step()?,
            result => panic!("expected completion, got {result:?}"),
        }
    }
    assert_eq!(rows, 3);
    drop(statement);
    let mut sums = connection
        .prepare("SELECT (SELECT sum(value) FROM ledger), count(*), sum(value) FROM audit")?;
    row(&mut sums)?;
    let result = sums.row().expect("trigger totals");
    assert_eq!(result.get::<i64>(0)?, 6);
    assert_eq!(result.get::<i64>(1)?, 3);
    assert_eq!(result.get::<i64>(2)?, 6);
    assert_eq!(
        sums.abort_noncommitting()?,
        NoncommittingAbort::StatementStopped
    );
    assert!(sums.reset_for_subprogram_reuse().is_err());
    assert!(matches!(sums.noncommitting_abort, AbortState::Stopped));
    assert_eq!(
        connection.n_active_root_statements.load(Ordering::SeqCst),
        0
    );
    Ok(())
}
