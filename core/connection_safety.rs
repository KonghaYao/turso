use super::Connection;
use crate::{
    LimboError, NativeIoRetirementGuard, NoncommittingRetirement, Result, RetirementErrors,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl Connection {
    pub fn retire_inactive_noncommitting(
        &self,
        guard: &(impl NativeIoRetirementGuard + ?Sized),
    ) -> Result<NoncommittingRetirement> {
        let pager = self.pager.load_full();
        if !Arc::ptr_eq(guard.io(), &pager.io) || !Arc::ptr_eq(guard.io(), &self.db.io) {
            return Err(LimboError::InvalidArgument(
                "retirement guard belongs to a different IO instance".into(),
            ));
        }
        self.validate_noncommitting_cleanup_runtime()?;
        if self.n_active_root_statements.load(Ordering::SeqCst) != 0
            || self.n_active_writes.load(Ordering::SeqCst) != 0
            || self.n_active_blob_statements.load(Ordering::SeqCst) != 0
            || self.is_nested_stmt()
            || self.statement_activity.lock().explicit_checkpoint_active
        {
            return Err(LimboError::StatementsInProgress(
                "connection retirement requires all native statements and checkpoints stopped",
            ));
        }
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.db.n_connections.fetch_sub(1, Ordering::SeqCst);
        }
        if pager.is_checkpointing() {
            pager.cleanup_after_checkpoint_failure();
        }
        self.cleanup_internal_temp_checkpoint();
        self.rollback_current_txn_state(&pager, true);
        self.clear_named_savepoints();
        self.clear_deferred_foreign_key_violations();
        self.clear_mvcc_log_meta();
        self.set_cdc_transaction_id(-1);
        Ok(NoncommittingRetirement::RetiredUnsettled(
            RetirementErrors {
                abort_error: None,
                io_error: None,
                cleanup_error: None,
            },
        ))
    }

    pub fn verify_noncommitting_retired(
        &self,
        guard: &(impl NativeIoRetirementGuard + ?Sized),
    ) -> Result<()> {
        let pager = self.pager.load_full();
        if !Arc::ptr_eq(guard.io(), &self.db.io) || !Arc::ptr_eq(guard.io(), &pager.io) {
            return Err(LimboError::InvalidArgument(
                "retirement guard belongs to a different IO instance".into(),
            ));
        }
        self.validate_noncommitting_cleanup_runtime()?;
        if !self.is_closed()
            || self.get_tx_state() != super::TransactionState::None
            || self.get_mv_tx().is_some()
            || self.next_attached_mv_tx().is_some()
            || !self.get_auto_commit()
            || self.n_active_root_statements.load(Ordering::SeqCst) != 0
            || self.n_active_writes.load(Ordering::SeqCst) != 0
            || self.n_active_blob_statements.load(Ordering::SeqCst) != 0
            || self.is_nested_stmt()
            || self.statement_activity.lock().explicit_checkpoint_active
            || pager.holds_read_lock()
            || pager.holds_write_lock()
            || pager.is_checkpointing()
            || self.temp.database.read().as_ref().is_some_and(|temp| {
                temp.pager.holds_read_lock()
                    || temp.pager.holds_write_lock()
                    || temp.pager.is_checkpointing()
            })
        {
            return Err(LimboError::InvalidArgument(
                "noncommitting retirement is not physically complete".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn cleanup_internal_temp_checkpoint(&self) {
        if let Some(temp) = self.temp.database.read().as_ref() {
            if temp.pager.is_checkpointing() {
                temp.pager.cleanup_after_checkpoint_failure();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::clock::{MonotonicInstant, WallClockInstant};
    use crate::io::{Buffer, Clock, File, FileId, FileSyncType, IO};
    use crate::{
        Completion, Database, DatabaseOpts, MemoryIO, NoncommittingAbort, OpenFlags, SqliteDialect,
        StepResult,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[derive(Default)]
    struct Dispatch {
        disabled: AtomicBool,
        calls: AtomicUsize,
    }

    impl Dispatch {
        fn enter(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.disabled.load(Ordering::SeqCst),
                "new primary IO entry"
            );
        }
    }

    struct CountingIo {
        inner: MemoryIO,
        dispatch: Arc<Dispatch>,
    }

    impl Clock for CountingIo {
        fn current_time_monotonic(&self) -> MonotonicInstant {
            self.inner.current_time_monotonic()
        }
        fn current_time_wall_clock(&self) -> WallClockInstant {
            self.inner.current_time_wall_clock()
        }
    }

    impl IO for CountingIo {
        fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
            self.dispatch.enter();
            Ok(Arc::new(CountingFile {
                inner: self.inner.open_file(path, flags, direct)?,
                dispatch: self.dispatch.clone(),
            }))
        }
        fn remove_file(&self, path: &str) -> Result<()> {
            self.dispatch.enter();
            self.inner.remove_file(path)
        }
        fn file_id(&self, path: &str) -> Result<FileId> {
            self.dispatch.enter();
            self.inner.file_id(path)
        }
        fn step(&self) -> Result<()> {
            self.dispatch.enter();
            self.inner.step()
        }
        fn cancel(&self, completions: &[Completion]) -> Result<()> {
            self.dispatch.enter();
            self.inner.cancel(completions)
        }
        fn drain_completions(&self, completions: &[Completion]) -> Result<()> {
            self.dispatch.enter();
            self.inner.drain_completions(completions)
        }
        fn wait_for_completion(&self, completion: Completion) -> Result<()> {
            self.dispatch.enter();
            self.inner.wait_for_completion(completion)
        }
    }

    struct CountingFile {
        inner: Arc<dyn File>,
        dispatch: Arc<Dispatch>,
    }

    impl File for CountingFile {
        fn lock_file(&self, exclusive: bool) -> Result<()> {
            self.dispatch.enter();
            self.inner.lock_file(exclusive)
        }
        fn unlock_file(&self) -> Result<()> {
            self.dispatch.enter();
            self.inner.unlock_file()
        }
        fn size(&self) -> Result<u64> {
            self.dispatch.enter();
            self.inner.size()
        }
        fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
            self.dispatch.enter();
            self.inner.pread(pos, completion)
        }
        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<Buffer>,
            completion: Completion,
        ) -> Result<Completion> {
            self.dispatch.enter();
            self.inner.pwrite(pos, buffer, completion)
        }
        fn pwritev(
            &self,
            pos: u64,
            buffers: Vec<Arc<Buffer>>,
            completion: Completion,
        ) -> Result<Completion> {
            self.dispatch.enter();
            self.inner.pwritev(pos, buffers, completion)
        }
        fn sync(&self, completion: Completion, mode: FileSyncType) -> Result<Completion> {
            self.dispatch.enter();
            self.inner.sync(completion, mode)
        }
        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.dispatch.enter();
            self.inner.truncate(len, completion)
        }
        fn checkpoint_wal_position(&self, salts: [u32; 2], frames: u64) -> Result<()> {
            self.dispatch.enter();
            self.inner.checkpoint_wal_position(salts, frames)
        }
    }

    struct LocalGuard(Arc<dyn IO>);

    impl NativeIoRetirementGuard for LocalGuard {
        fn io(&self) -> &Arc<dyn IO> {
            &self.0
        }
    }

    fn fixture() -> Result<(Arc<Database>, Arc<Dispatch>, LocalGuard)> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let dispatch = Arc::new(Dispatch::default());
        let io: Arc<dyn IO> = Arc::new(CountingIo {
            inner: MemoryIO::new(),
            dispatch: dispatch.clone(),
        });
        let database = Database::open_file_with_flags(
            io.clone(),
            &format!(
                "inactive-retirement-{}.db",
                NEXT_ID.fetch_add(1, Ordering::SeqCst)
            ),
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(SqliteDialect),
        )?;
        Ok((database, dispatch, LocalGuard(io)))
    }

    #[test]
    fn different_connections_release_read_write_transactions_without_primary_io() -> Result<()> {
        let (database, dispatch, guard) = fixture()?;
        let default = database.connect()?;
        default.execute("CREATE TABLE ledger(value); INSERT INTO ledger VALUES(10)")?;
        let reader = database.connect()?;
        let writer = database.connect()?;
        assert!(!Arc::ptr_eq(&default, &reader));
        assert!(!Arc::ptr_eq(&reader, &writer));
        reader.execute("BEGIN; SELECT value FROM ledger")?;
        writer.execute("BEGIN; INSERT INTO ledger VALUES(20)")?;
        assert!(reader.pager.load().holds_read_lock());
        assert!(writer.pager.load().holds_write_lock());
        let mut siblings = Vec::new();
        for connection in [&default, &reader, &writer] {
            siblings.push(connection.prepare("SELECT value FROM ledger")?);
            siblings.push(connection.prepare("INSERT INTO ledger VALUES(99)")?);
            assert_eq!(
                connection.n_active_root_statements.load(Ordering::SeqCst),
                0
            );
        }
        let weak_connections = [
            Arc::downgrade(&default),
            Arc::downgrade(&reader),
            Arc::downgrade(&writer),
        ];
        let weak_database = Arc::downgrade(&database);
        let before = dispatch.calls.load(Ordering::SeqCst);
        dispatch.disabled.store(true, Ordering::SeqCst);
        for (index, connection) in [&default, &reader, &writer].into_iter().enumerate() {
            for _attempt in 0..2 {
                assert!(matches!(
                    connection.retire_inactive_noncommitting(&guard)?,
                    NoncommittingRetirement::RetiredUnsettled(_)
                ));
                assert!(connection.is_closed());
                assert!(!connection.pager.load().holds_read_lock());
                assert!(!connection.pager.load().holds_write_lock());
                assert_eq!(database.n_connections.load(Ordering::SeqCst), 2 - index);
            }
        }
        let mut results = Vec::new();
        for mut sibling in siblings.drain(..) {
            results.push((sibling.step(), sibling.reset()));
            drop(sibling);
        }
        for connection in [&default, &reader, &writer] {
            assert_eq!(
                connection.n_active_root_statements.load(Ordering::SeqCst),
                0
            );
            assert_eq!(connection.n_active_writes.load(Ordering::SeqCst), 0);
        }
        drop(default);
        drop(reader);
        drop(writer);
        assert!(weak_connections.iter().all(|weak| weak.upgrade().is_none()));
        assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
        drop(database);
        assert!(weak_database.upgrade().is_none());
        assert_eq!(dispatch.calls.load(Ordering::SeqCst), before);
        for (step, reset) in results {
            assert_eq!(
                step.expect_err("closed statement").to_string(),
                "Internal error: Connection closed"
            );
            reset?;
        }
        Ok(())
    }

    #[test]
    fn connection_retirement_rejects_foreign_guard_and_active_statement() -> Result<()> {
        let (database, dispatch, guard) = fixture()?;
        let connection = database.connect()?;
        connection.execute("CREATE TABLE ledger(value); INSERT INTO ledger VALUES(10)")?;
        let mut statement = connection.prepare("SELECT value FROM ledger")?;
        loop {
            match statement.step_with_admission_check(|_| true)? {
                StepResult::Row => break,
                StepResult::IO | StepResult::Yield => statement._io().step()?,
                result => panic!("expected row, got {result:?}"),
            }
        }
        let foreign = LocalGuard(Arc::new(MemoryIO::new()));
        assert!(matches!(
            connection.retire_inactive_noncommitting(&foreign),
            Err(LimboError::InvalidArgument(_))
        ));
        assert!(matches!(
            connection.retire_inactive_noncommitting(&guard),
            Err(LimboError::StatementsInProgress(_))
        ));
        assert!(!connection.is_closed());
        assert_eq!(database.n_connections.load(Ordering::SeqCst), 1);
        assert_eq!(
            statement.abort_noncommitting()?,
            NoncommittingAbort::StatementStopped
        );
        drop(statement);
        let before = dispatch.calls.load(Ordering::SeqCst);
        dispatch.disabled.store(true, Ordering::SeqCst);
        assert!(matches!(
            connection.retire_inactive_noncommitting(&guard)?,
            NoncommittingRetirement::RetiredUnsettled(_)
        ));
        drop(connection);
        assert_eq!(database.n_connections.load(Ordering::SeqCst), 0);
        drop(database);
        assert_eq!(dispatch.calls.load(Ordering::SeqCst), before);
        Ok(())
    }
}
