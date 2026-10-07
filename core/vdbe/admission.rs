use super::{builder::CursorType, insn::Insn, PreparedProgram, Program};
use crate::function::{AccumulatorFunc, AggFunc, Func, ScalarFunc, WindowFunc};
use crate::storage::journal_mode::JournalMode;
use crate::{Connection, LimboError, Result, MAIN_DB_ID};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

impl Program {
    pub(crate) fn validate_noncommitting_admission(&self, executes: bool) -> Result<()> {
        self.validate_noncommitting_runtime(executes)?;
        if !executes {
            return Ok(());
        }
        if !self.connection.schema.read().incremental_views.is_empty()
            || !self.connection.syms.read().collations.is_empty()
        {
            return unsupported("materialized views or collations");
        }
        let mut pending = vec![self.prepared().clone()];
        let mut visited = HashSet::new();
        while let Some(program) = pending.pop() {
            if !visited.insert(Arc::as_ptr(&program)) {
                continue;
            }
            validate_program(&program, &mut pending)?;
        }
        Ok(())
    }

    pub(crate) fn validate_noncommitting_runtime(&self, executes: bool) -> Result<()> {
        self.connection.validate_noncommitting_runtime(executes)
    }
}

impl Connection {
    pub(crate) fn validate_noncommitting_runtime(&self, executes: bool) -> Result<()> {
        if self.mv_store().is_some()
            || !self.attached_database_names().is_empty()
            || self.temp.database.read().is_some()
            || self.get_capture_data_changes_info().is_some()
        {
            return unsupported("MVCC, attached/TEMP databases, or active CDC");
        }
        if self.pager.load().io.supports_shared_wal_coordination() {
            return unsupported("host-filesystem shared WAL coordination");
        }
        if !executes {
            return Ok(());
        }
        if !self.view_transaction_states.is_empty()
            || !self.vtab_txn_states.read().is_empty()
            || !self.index_method_tx_cursors.lock().is_empty()
        {
            return unsupported("extension transaction state");
        }
        Ok(())
    }
}

fn validate_program(
    program: &PreparedProgram,
    pending: &mut Vec<Arc<PreparedProgram>>,
) -> Result<()> {
    if program
        .read_databases
        .iter()
        .chain(program.write_databases.iter())
        .any(|database| database != MAIN_DB_ID)
    {
        return unsupported("non-main database bytecode");
    }
    if program.cursor_ref.iter().any(|(_, cursor)| {
        matches!(
            cursor,
            CursorType::IndexMethod(_)
                | CursorType::VirtualTable(_)
                | CursorType::MaterializedView(..)
        )
    }) {
        return unsupported("virtual-table, custom-index, or materialized-view cursor");
    }
    for (instruction, _) in &program.insns {
        match instruction {
            Insn::Program { program, .. } => pending.push(program.prepared_program()?),
            Insn::Vacuum { .. } | Insn::VacuumInto { .. } => {
                return unsupported("VACUUM helper execution");
            }
            Insn::JournalMode {
                new_mode: Some(mode),
                ..
            } if matches!(JournalMode::from_str(mode), Ok(JournalMode::Mvcc)) => {
                return unsupported("journal-mode change to MVCC");
            }
            Insn::InitCdcVersion { .. } => return unsupported("CDC initialization"),
            Insn::PopulateMaterializedViews { .. } => {
                return unsupported("materialized-view helper execution");
            }
            Insn::VOpen { .. }
            | Insn::VCreate { .. }
            | Insn::VUpdate { .. }
            | Insn::VDestroy { .. }
            | Insn::VRename { .. }
            | Insn::VBegin { .. }
            | Insn::IndexMethodCreate { .. }
            | Insn::IndexMethodDestroy { .. }
            | Insn::IndexMethodOptimize { .. }
            | Insn::IndexMethodQuery { .. } => {
                return unsupported("virtual-table or custom-index execution");
            }
            Insn::Function { func, .. } => match &func.func {
                Func::External(_) | Func::Dialect(_) => {
                    return unsupported("external or dialect function execution");
                }
                Func::Scalar(
                    ScalarFunc::Attach | ScalarFunc::Detach | ScalarFunc::LoadExtension,
                ) => {
                    return unsupported("ATTACH, DETACH, or extension loading");
                }
                _ => {}
            },
            Insn::AggStep { data } => validate_aggregate(&data.func)?,
            Insn::AggInverse { func, .. }
            | Insn::AggFinal { func, .. }
            | Insn::AggValue { func, .. } => validate_aggregate(func)?,
            _ => {}
        }
    }
    Ok(())
}

fn validate_aggregate(function: &AccumulatorFunc) -> Result<()> {
    if matches!(
        function,
        AccumulatorFunc::Agg(AggFunc::External(_))
            | AccumulatorFunc::Window(WindowFunc::External(_))
    ) {
        return unsupported("external aggregate execution");
    }
    Ok(())
}

fn unsupported<T>(feature: &str) -> Result<T> {
    Err(LimboError::InvalidArgument(format!(
        "noncommitting admission does not support {feature}"
    )))
}
