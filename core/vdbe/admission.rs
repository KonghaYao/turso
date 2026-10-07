use super::{builder::CursorType, insn::Insn, BranchOffset, PreparedProgram, Program};
use crate::function::{AccumulatorFunc, AggFunc, Func, ScalarFunc, WindowFunc};
use crate::storage::journal_mode::JournalMode;
use crate::{Connection, LimboError, Result, TempStore, TransactionMode, MAIN_DB_ID, TEMP_DB_ID};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

pub(crate) struct InternalTempCertificate {
    program: Arc<PreparedProgram>,
    generation: u64,
}

impl InternalTempCertificate {
    pub(super) fn validate(&self, program: &Program) -> Result<()> {
        if !Arc::ptr_eq(&self.program, program.prepared())
            || self.generation != program.connection.internal_temp_generation()
            || program.connection.get_temp_store() != TempStore::Memory
        {
            return unsupported("expired internal TEMP certificate");
        }
        program.validate_noncommitting_runtime(true)
    }

    pub(super) fn pager(&self, program: &Program) -> Result<Arc<crate::Pager>> {
        self.validate(program)?;
        program
            .connection
            .internal_begin_temp_pager(self.generation)
    }
}

impl Program {
    pub(crate) fn validate_noncommitting_admission(&self, executes: bool) -> Result<()> {
        self.validate_noncommitting_runtime(executes)?;
        if !executes {
            return Ok(());
        }
        self.validate_noncommitting_bytecode(true)
    }

    pub(crate) fn validate_noncommitting_cleanup_admission(&self, executes: bool) -> Result<()> {
        self.connection.validate_noncommitting_cleanup_runtime()?;
        if !executes {
            return Ok(());
        }
        self.validate_noncommitting_bytecode(false)
    }

    fn validate_noncommitting_bytecode(&self, execution: bool) -> Result<()> {
        if !self.connection.schema.read().incremental_views.is_empty()
            || !self.connection.syms.read().collations.is_empty()
        {
            return unsupported("materialized views or collations");
        }
        let mut pending = vec![self.prepared().clone()];
        let internal_begin = pure_internal_temp_begin(self.prepared());
        if execution && internal_begin && self.connection.get_temp_store() != TempStore::Memory {
            return unsupported("BEGIN auxiliary TEMP without explicit Memory selection");
        }
        let mut visited = HashSet::new();
        while let Some(program) = pending.pop() {
            if !visited.insert(Arc::as_ptr(&program)) {
                continue;
            }
            let allow_internal_begin = internal_begin && Arc::ptr_eq(&program, self.prepared());
            validate_program(&program, &mut pending, allow_internal_begin)?;
        }
        Ok(())
    }

    pub(crate) fn validate_noncommitting_runtime(&self, executes: bool) -> Result<()> {
        self.connection.validate_noncommitting_runtime(executes)
    }

    pub(crate) fn certify_internal_temp(&self) -> Option<InternalTempCertificate> {
        pure_internal_temp_begin(self.prepared()).then(|| InternalTempCertificate {
            program: self.prepared().clone(),
            generation: self.connection.internal_temp_generation(),
        })
    }
}

impl Connection {
    pub(crate) fn validate_noncommitting_runtime(&self, executes: bool) -> Result<()> {
        self.validate_noncommitting_environment(executes, true)
    }

    pub(crate) fn validate_noncommitting_cleanup_runtime(&self) -> Result<()> {
        self.validate_noncommitting_environment(true, false)
    }

    fn validate_noncommitting_environment(&self, executes: bool, execution: bool) -> Result<()> {
        self.validate_internal_temp(execution)?;
        if self.mv_store().is_some()
            || !self.attached_database_names().is_empty()
            || self.get_capture_data_changes_info().is_some()
        {
            return unsupported("MVCC, attached databases, or active CDC");
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
    internal_begin: bool,
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
        if let Some(database) = instruction_database(instruction) {
            if database != MAIN_DB_ID
                && !(internal_begin
                    && database == TEMP_DB_ID
                    && matches!(instruction, Insn::Transaction { .. }))
            {
                return unsupported("non-main database operand");
            }
        }
        match instruction {
            Insn::ParseSchema {
                trigger_target_database_id: Some(database),
                ..
            } if *database != MAIN_DB_ID => return unsupported("non-main trigger target"),
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

fn pure_internal_temp_begin(program: &PreparedProgram) -> bool {
    if program.is_subprogram
        || !program.cursor_ref.is_empty()
        || !program.read_databases.is_empty()
        || !program.write_databases.is_empty()
    {
        return false;
    }
    matches!(
        program.insns.as_slice(),
        [
            (Insn::Init { target_pc: BranchOffset::Offset(5) }, _),
            (Insn::Transaction { db: MAIN_DB_ID, tx_mode: TransactionMode::Write, .. }, _),
            (Insn::Transaction { db: TEMP_DB_ID, tx_mode: TransactionMode::Write, schema_cookie: 0 }, _),
            (Insn::AutoCommit { auto_commit: false, rollback: false }, _),
            (Insn::Halt { err_code: 0, description, on_error: None, description_reg: None }, _),
            (Insn::Goto { target_pc: BranchOffset::Offset(1) }, _),
        ] if description.is_empty()
    )
}

fn instruction_database(instruction: &Insn) -> Option<usize> {
    match instruction {
        Insn::OpenRead { db, .. }
        | Insn::VDestroy { db, .. }
        | Insn::Transaction { db, .. }
        | Insn::OpenWrite { db, .. }
        | Insn::CreateBtree { db, .. }
        | Insn::IndexMethodCreate { db, .. }
        | Insn::IndexMethodDestroy { db, .. }
        | Insn::IndexMethodOptimize { db, .. }
        | Insn::IndexMethodQuery { db, .. }
        | Insn::ClearBtree { db, .. }
        | Insn::Destroy { db, .. }
        | Insn::DropTable { db, .. }
        | Insn::DropView { db, .. }
        | Insn::DropIndex { db, .. }
        | Insn::DropTrigger { db, .. }
        | Insn::DropType { db, .. }
        | Insn::DropSequence { db, .. }
        | Insn::SequenceBeginInnerTx { db, .. }
        | Insn::SequenceCommitInnerTx { db, .. }
        | Insn::SequenceComputeNext { db, .. }
        | Insn::SequenceTrackAllocation { db, .. }
        | Insn::SequenceRegisterAllocation { db, .. }
        | Insn::AddType { db, .. }
        | Insn::ParseSchema { db, .. }
        | Insn::PageCount { db, .. }
        | Insn::ReadCookie { db, .. }
        | Insn::SetCookie { db, .. }
        | Insn::RenameTable { db, .. }
        | Insn::DropColumn { db, .. }
        | Insn::AlterColumn { db, .. }
        | Insn::MaxPgcnt { db, .. }
        | Insn::JournalMode { db, .. }
        | Insn::Vacuum { db, .. } => Some(*db),
        Insn::AddColumn { data } => Some(data.db),
        Insn::IntegrityCk { data } => Some(data.db),
        Insn::AddSequence { data } => Some(data.db),
        Insn::Checkpoint { database, .. } => Some(*database),
        _ => None,
    }
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
