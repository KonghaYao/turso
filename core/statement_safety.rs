use super::{ProgramExecutionState, Statement, StatementOrigin, StepResult};
use crate::{LimboError, QueryMode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum NoncommittingAbort {
    PendingIo,
    StatementStopped,
    FinalizationAlreadyStarted,
}

pub trait NativeIoRetirementGuard {
    fn io(&self) -> &std::sync::Arc<dyn crate::IO>;
}

#[derive(Debug, Clone)]
pub struct RetirementErrors {
    pub abort_error: Option<LimboError>,
    pub io_error: Option<LimboError>,
    pub cleanup_error: Option<LimboError>,
}

#[derive(Debug, Clone)]
#[must_use]
pub enum NoncommittingRetirement {
    PendingIo,
    RetiredUnsettled(RetirementErrors),
}

pub(super) enum AbortState {
    NotRequested,
    Pending,
    Stopped,
    Finalizing,
    Failed(LimboError),
    Retired(RetirementErrors),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CleanupState {
    Unentered,
    Checked,
    Unchecked,
}

impl Statement {
    pub fn step_with_admission_check(
        &mut self,
        mut admission: impl FnMut(bool) -> bool,
    ) -> Result<StepResult> {
        self.step_checked(None, &mut Some(&mut admission))
    }

    pub(super) fn check_admission(
        &self,
        admission: &mut Option<&mut dyn FnMut(bool) -> bool>,
    ) -> Result<bool> {
        let Some(admission) = admission.as_mut() else {
            return Ok(true);
        };
        self.validate_noncommitting_profile()?;
        Ok(admission(
            self.query_mode != QueryMode::Normal || self.program.is_readonly(),
        ))
    }

    pub fn abort_noncommitting(&mut self) -> Result<NoncommittingAbort> {
        match &self.noncommitting_abort {
            AbortState::Stopped => return Ok(NoncommittingAbort::StatementStopped),
            AbortState::Finalizing => return Ok(NoncommittingAbort::FinalizationAlreadyStarted),
            AbortState::Failed(error) => return Err(error.clone()),
            AbortState::Retired(_) => return self.retired_statement_error(),
            AbortState::NotRequested | AbortState::Pending => {}
        }
        self.validate_noncommitting_cleanup()?;
        self.noncommitting_abort = AbortState::Pending;
        let result = self.abort_noncommitting_inner();
        match &result {
            Ok(NoncommittingAbort::StatementStopped) => {
                self.noncommitting_abort = AbortState::Stopped;
            }
            Ok(NoncommittingAbort::FinalizationAlreadyStarted) => {
                self.noncommitting_abort = AbortState::Finalizing;
            }
            Err(error) => self.noncommitting_abort = AbortState::Failed(error.clone()),
            Ok(NoncommittingAbort::PendingIo) => {}
        }
        result
    }

    fn abort_noncommitting_inner(&mut self) -> Result<NoncommittingAbort> {
        if self.state.noncommitting_finalization_started {
            return Ok(NoncommittingAbort::FinalizationAlreadyStarted);
        }
        let pending = self
            .state
            .io_completions
            .as_ref()
            .map(|completions| &completions.0)
            .into_iter()
            .chain(self.detached_io.iter())
            .filter(|completion| !completion.is_wait());
        for completion in pending {
            if !completion.finished() {
                return Ok(NoncommittingAbort::PendingIo);
            }
            if let Some(error) = completion.get_error() {
                return Err(error.into());
            }
        }
        self.state.io_completions = None;
        self.detached_io.clear();
        if self.cleanup_state != CleanupState::Unentered {
            self.program.abort(
                &self.pager,
                None,
                &mut self.state,
                self.counted_as_active_root,
            )?;
        }
        self.finish_noncommitting_cleanup();
        Ok(NoncommittingAbort::StatementStopped)
    }

    fn finish_noncommitting_cleanup(&mut self) {
        if self.state.is_active_write {
            let previous = self
                .program
                .connection
                .n_active_writes
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            crate::turso_assert!(
                previous == 1,
                "aborting a writer with {previous} active writers"
            );
            self.state.is_active_write = false;
        }
        self.release_active_root_if_counted();
        self.analyze_refresh = None;
        self.state.reset(None, None);
        self.state.execution_state = ProgramExecutionState::Interrupted;
        self.busy = false;
        self.busy_handler_state = None;
        self.query_timeout_override = None;
        self.has_returned_row = false;
    }

    pub fn retire_noncommitting(
        &mut self,
        guard: &(impl NativeIoRetirementGuard + ?Sized),
    ) -> Result<NoncommittingRetirement> {
        if !std::sync::Arc::ptr_eq(guard.io(), &self.pager.io) {
            return Err(LimboError::InvalidArgument(
                "retirement guard belongs to a different IO instance".into(),
            ));
        }
        let abort_error = match &self.noncommitting_abort {
            AbortState::Retired(errors) => {
                return Ok(NoncommittingRetirement::RetiredUnsettled(errors.clone()));
            }
            AbortState::Failed(error) => Some(error.clone()),
            AbortState::Pending | AbortState::Finalizing => None,
            AbortState::NotRequested | AbortState::Stopped => {
                return Err(LimboError::InvalidArgument(
                    "retirement requires an unsettled noncommitting abort".into(),
                ));
            }
        };
        self.validate_noncommitting_cleanup()?;
        self.program.validate_noncommitting_runtime(true)?;
        let expected_roots = i32::from(self.counted_as_active_root);
        if self
            .program
            .connection
            .n_active_root_statements
            .load(std::sync::atomic::Ordering::SeqCst)
            != expected_roots
        {
            return Err(LimboError::StatementsInProgress(
                "retirement requires exclusive connection ownership",
            ));
        }
        let mut io_error = None;
        for completion in self
            .state
            .io_completions
            .as_ref()
            .map(|completions| &completions.0)
            .into_iter()
            .chain(self.detached_io.iter())
            .filter(|completion| !completion.is_wait())
        {
            if !completion.finished() {
                return Ok(NoncommittingRetirement::PendingIo);
            }
            if io_error.is_none() {
                io_error = completion.get_error().map(LimboError::from);
            }
        }
        let connection = self.program.connection.clone();
        if !connection
            .closed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            connection
                .db
                .n_connections
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.state.io_completions = None;
        self.detached_io.clear();
        if self.pager.is_checkpointing() {
            self.pager.cleanup_after_checkpoint_failure();
        }
        let cleanup_error = if self.cleanup_state == CleanupState::Unentered {
            None
        } else {
            self.program
                .abort(
                    &self.pager,
                    None,
                    &mut self.state,
                    self.counted_as_active_root,
                )
                .err()
        };
        connection.rollback_current_txn_state(&self.pager, true);
        connection.clear_named_savepoints();
        connection.clear_deferred_foreign_key_violations();
        connection.clear_mvcc_log_meta();
        connection.set_cdc_transaction_id(-1);
        self.finish_noncommitting_cleanup();
        let errors = RetirementErrors {
            abort_error,
            io_error,
            cleanup_error,
        };
        self.noncommitting_abort = AbortState::Retired(errors.clone());
        Ok(NoncommittingRetirement::RetiredUnsettled(errors))
    }

    fn retired_statement_error<T>(&self) -> Result<T> {
        Err(LimboError::InvalidArgument(
            "statement connection was retired with an unsettled SQL outcome".into(),
        ))
    }

    pub(super) fn ensure_not_aborted(&self) -> Result<()> {
        match &self.noncommitting_abort {
            AbortState::NotRequested => Ok(()),
            AbortState::Failed(error) => Err(error.clone()),
            _ => Err(LimboError::InvalidArgument(
                "statement execution is disabled after noncommitting abort was requested".into(),
            )),
        }
    }

    fn validate_noncommitting_profile(&self) -> Result<()> {
        self.validate_noncommitting_origin()?;
        self.program
            .validate_noncommitting_admission(self.query_mode == QueryMode::Normal)
    }

    pub(super) fn record_execution_entry(&mut self, checked: bool) {
        self.cleanup_state = if checked {
            CleanupState::Checked
        } else {
            CleanupState::Unchecked
        };
    }

    fn validate_noncommitting_cleanup(&self) -> Result<()> {
        self.validate_noncommitting_origin()?;
        match self.cleanup_state {
            CleanupState::Unentered => Ok(()),
            CleanupState::Checked => self.program.validate_noncommitting_runtime(true),
            CleanupState::Unchecked => self.validate_noncommitting_profile().map_err(|error| {
                LimboError::InvalidArgument(format!(
                    "noncommitting cleanup cannot adopt unsupported unchecked execution: {error}"
                ))
            }),
        }
    }

    fn validate_noncommitting_origin(&self) -> Result<()> {
        if self.origin != StatementOrigin::Root || self.is_blob_handle {
            return Err(LimboError::InvalidArgument(
                "noncommitting admission requires an ordinary root statement".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "statement_safety_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "statement_profile_tests.rs"]
mod profile_tests;
