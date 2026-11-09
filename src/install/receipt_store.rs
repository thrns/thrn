//! Private, cache-independent receipt storage and read-only installed listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::PathBuf,
};

use ring::{
    digest::{Context, SHA256},
    rand::SecureRandom,
};
use thiserror::Error;

use super::{
    commit::{CommittedInstallation, InstallFaultInjector, InstallOperation, NodeKind, SecureDir},
    destination::FileIdentity,
    journal::{
        JournalDirectory, JournalError, JournalFaultInjector, JournalRecord, JournalStore,
        OperationState,
    },
    receipt::{
        InstallationReceipt, MAX_RECEIPT_BYTES, MAX_RECEIPTS, ReceiptFile, ReceiptValidationError,
    },
    removal::{
        NoRemovalFaults, RemovalError, RemovalFaultInjector, RemovalJournalStore, RemovalPlan,
        RemovalSummary,
    },
    update::{
        NoUpdateFaults, UpdateError, UpdateFaultInjector, UpdateJournalStore, UpdatePlan,
        UpdateSummary,
    },
};

use crate::agents::AgentId;

const IO_CHUNK_BYTES: usize = 8 * 1024;
const WRITER_LOCK_NAME: &str = ".writer.lock";
const TEMP_NAME_ATTEMPTS: usize = 32;

struct NoRecoveryInstallFaults;
impl InstallFaultInjector for NoRecoveryInstallFaults {}

pub struct ReceiptStore {
    directory: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectInstallationInspection {
    pub(crate) receipt: InstallationReceipt,
    pub(crate) content: ReceiptContentState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectInspectionSnapshot {
    pub(crate) project_root: PathBuf,
    pub(crate) installations: Vec<ProjectInstallationInspection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SharedSkillStatus {
    Absent,
    ManagedByCodex,
    UnverifiedCodexReceipt,
    Unmanaged,
    ChangedSincePlanning,
}

impl ReceiptStore {
    pub fn for_system() -> Result<Self, ReceiptStoreError> {
        let paths = crate::os_paths::OsPaths::for_system().map_err(map_os_path_error)?;
        let data_directory = paths.application_data_dir().map_err(map_os_path_error)?;
        Ok(Self::new(data_directory.join("receipts")))
    }

    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Read and validate all stored receipts without creating or changing any storage.
    pub fn list(&self) -> Result<Vec<InstallationReceipt>, ReceiptStoreError> {
        let Some(directory) = self.open_existing_directory()? else {
            self.ensure_no_pending_removals()?;
            return Ok(Vec::new());
        };
        let _read_reservation = WriterLock::try_acquire_existing(directory.clone())?;
        self.ensure_no_pending_removals()?;
        let receipts = read_receipts(&directory, true)?;
        validate_unique_destinations(&receipts)?;
        let mut receipts = receipts
            .into_iter()
            .map(|(_, receipt)| receipt)
            .collect::<Vec<_>>();
        receipts.sort_by(|left, right| {
            left.root_path()
                .cmp(&right.root_path())
                .then_with(|| left.catalog_resource_id().cmp(right.catalog_resource_id()))
                .then_with(|| left.skill_name().cmp(right.skill_name()))
        });
        Ok(receipts)
    }

    /// Inspect only installations recorded for one existing project. The read reservation, when
    /// present, is held while lifecycle journals, receipts, and selected project trees are read.
    /// This path never creates storage and never invokes recovery.
    #[cfg(test)]
    pub(crate) fn inspect_project(
        &self,
        project: Option<&std::path::Path>,
        current_directory: &std::path::Path,
    ) -> Result<ProjectInspectionSnapshot, ReceiptStoreError> {
        let project_root = super::removal::resolve_project_root(project, current_directory)
            .map_err(|error| ReceiptStoreError::ProjectInspection(error.to_string()))?;
        self.inspect_scope(&super::scope::ResolvedScope::from_validated(
            super::plan::InstallScope::Project,
            project_root,
        ))
    }

    /// Read-only inspection restricted to the supplied validated scope identity and root.
    pub(crate) fn inspect_scope(
        &self,
        scope: &super::scope::ResolvedScope,
    ) -> Result<ProjectInspectionSnapshot, ReceiptStoreError> {
        let Some(directory) = self.open_existing_directory()? else {
            self.ensure_no_pending_operations()?;
            return Ok(ProjectInspectionSnapshot {
                project_root: scope.root().to_path_buf(),
                installations: Vec::new(),
            });
        };
        let read_reservation = WriterLock::try_acquire_existing(directory.clone())?;
        self.ensure_no_pending_operations()?;
        let receipts = read_receipts(&directory, true)?;
        if read_reservation.is_none() && !receipts.is_empty() {
            return Err(ReceiptStoreError::WriterLockMissing);
        }

        let mut inspections = Vec::new();
        for (_, receipt) in receipts {
            if receipt.agent() != scope.agent().as_str()
                || receipt.scope() != scope.scope().as_str()
                || receipt.root_path() != scope.root()
            {
                continue;
            }
            let content = if super::removal::verify_receipt_for_check(&receipt).is_ok() {
                ReceiptContentState::Verified
            } else {
                ReceiptContentState::Modified
            };
            inspections.push(ProjectInstallationInspection { receipt, content });
        }
        if let Some(reservation) = read_reservation.as_ref() {
            reservation.validate()?;
        } else {
            let current_directory = self
                .open_existing_directory()?
                .ok_or(ReceiptStoreError::SnapshotChanged)?;
            if current_directory
                .identity()
                .map_err(|error| map_io("reidentify receipt storage", error))?
                != directory
                    .identity()
                    .map_err(|error| map_io("identify receipt storage", error))?
                || !current_directory
                    .entries()
                    .map_err(|error| map_io("recheck receipt storage", error))?
                    .is_empty()
            {
                return Err(ReceiptStoreError::SnapshotChanged);
            }
        }
        self.ensure_no_pending_operations()?;
        Ok(ProjectInspectionSnapshot {
            project_root: scope.root().to_path_buf(),
            installations: inspections,
        })
    }

    /// Classify an explicitly named skill in the shared Codex-compatible path without adopting,
    /// changing, or recovering it. The planning snapshot is compared again after receipt reads.
    pub(crate) fn inspect_shared_skill(
        &self,
        scope: super::plan::InstallScope,
        root: &std::path::Path,
        skill_name: &str,
        expected: &super::destination::DestinationSnapshot,
    ) -> Result<SharedSkillStatus, ReceiptStoreError> {
        let current = super::destination::inspect_destination_for_agent(
            AgentId::Codex,
            Some(root),
            root,
            skill_name,
        )
        .map_err(|error| ReceiptStoreError::ProjectInspection(error.to_string()))?;
        if !expected.same_shared_path_state(&current) {
            return Ok(SharedSkillStatus::ChangedSincePlanning);
        }
        if current.skill_state() == super::destination::ComponentState::Missing
            && current.conflicts().is_empty()
        {
            return Ok(SharedSkillStatus::Absent);
        }

        let codex_scope = super::scope::ResolvedScope::from_agent_validated(
            AgentId::Codex,
            scope,
            root.to_path_buf(),
        );
        let snapshot = self.inspect_scope(&codex_scope)?;
        match snapshot
            .installations
            .iter()
            .find(|installation| installation.receipt.skill_name() == skill_name)
        {
            Some(installation) if installation.content == ReceiptContentState::Verified => {
                Ok(SharedSkillStatus::ManagedByCodex)
            }
            Some(_) => Ok(SharedSkillStatus::UnverifiedCodexReceipt),
            None => Ok(SharedSkillStatus::Unmanaged),
        }
    }

    /// Build a read-only removal plan from one exact canonical-ID receipt and its current tree.
    #[cfg(test)]
    pub(crate) fn plan_removal(
        &self,
        resource_id: &str,
        project: Option<&std::path::Path>,
        current_directory: &std::path::Path,
    ) -> Result<RemovalPlan, RemovalError> {
        let project_root = super::removal::resolve_project_root(project, current_directory)?;
        self.plan_removal_at(
            resource_id,
            super::plan::InstallAgent::Codex,
            super::plan::InstallScope::Project,
            &project_root,
        )
    }

    pub(crate) fn plan_removal_in_scope(
        &self,
        resource_id: &str,
        scope: &super::scope::ResolvedScope,
    ) -> Result<RemovalPlan, RemovalError> {
        self.plan_removal_at(resource_id, scope.agent(), scope.scope(), scope.root())
    }

    fn plan_removal_at(
        &self,
        resource_id: &str,
        agent: super::plan::InstallAgent,
        scope: super::plan::InstallScope,
        scope_root: &std::path::Path,
    ) -> Result<RemovalPlan, RemovalError> {
        let directory = self
            .open_existing_directory()
            .map_err(|error| RemovalError::Store(error.to_string()))?
            .ok_or(RemovalError::MissingReceipt)?;
        check_idle_writer_lock(&directory)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        self.ensure_no_pending_operations()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let receipts = read_receipts(&directory, true)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        build_removal_plan(&directory, &receipts, resource_id, agent, scope, scope_root)
    }

    pub(crate) fn plan_update_target_in_scope(
        &self,
        resource_id: &str,
        scope: &super::scope::ResolvedScope,
    ) -> Result<RemovalPlan, RemovalError> {
        self.plan_update_target_at(resource_id, scope.agent(), scope.scope(), scope.root())
    }

    fn plan_update_target_at(
        &self,
        resource_id: &str,
        agent: super::plan::InstallAgent,
        scope: super::plan::InstallScope,
        scope_root: &std::path::Path,
    ) -> Result<RemovalPlan, RemovalError> {
        let directory = self
            .open_existing_directory()
            .map_err(|error| RemovalError::Store(error.to_string()))?
            .ok_or(RemovalError::MissingReceipt)?;
        check_idle_writer_lock(&directory)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        self.ensure_no_pending_operations()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let receipts = read_receipts(&directory, true)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        build_removal_plan(&directory, &receipts, resource_id, agent, scope, scope_root)
    }

    pub(crate) fn update_receipt_digest(
        &self,
        plan: &RemovalPlan,
    ) -> Result<String, ReceiptStoreError> {
        let directory = self
            .open_existing_directory()?
            .ok_or(ReceiptStoreError::UnsafeDirectory)?;
        if directory
            .identity()
            .map_err(|error| map_io("identify receipt storage", error))?
            != plan.receipt_directory_identity()
        {
            return Err(ReceiptStoreError::UnsafeDirectory);
        }
        receipt_file_sha256(&directory, plan.receipt_name())
    }

    /// Recover an interrupted removal and then remove one exact receipt-backed installation.
    #[cfg(test)]
    pub(crate) fn remove_by_id(
        &self,
        resource_id: &str,
        project: Option<&std::path::Path>,
        current_directory: &std::path::Path,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<RemovalSummary, RemovalError> {
        let project_root = super::removal::resolve_project_root(project, current_directory)?;
        self.remove_by_id_at(
            resource_id,
            super::plan::InstallAgent::Codex,
            super::plan::InstallScope::Project,
            &project_root,
            faults,
        )
    }

    pub(crate) fn remove_by_id_in_scope(
        &self,
        resource_id: &str,
        scope: &super::scope::ResolvedScope,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<RemovalSummary, RemovalError> {
        self.remove_by_id_at(
            resource_id,
            scope.agent(),
            scope.scope(),
            scope.root(),
            faults,
        )
    }

    fn remove_by_id_at(
        &self,
        resource_id: &str,
        agent: super::plan::InstallAgent,
        scope: super::plan::InstallScope,
        scope_root: &std::path::Path,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<RemovalSummary, RemovalError> {
        let directory = self
            .open_existing_directory()
            .map_err(|error| RemovalError::Store(error.to_string()))?
            .ok_or(RemovalError::MissingReceipt)?;
        let mut lock = WriterLock::acquire(directory.clone(), &NoReceiptFaults)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        lock.validate()
            .map_err(|error| RemovalError::Store(error.to_string()))?;

        let install_journals = self
            .journal_store()
            .map_err(|error| RemovalError::Store(error.to_string()))?
            .read_all()
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        if !install_journals.is_empty() {
            return Err(RemovalError::Store(format!(
                "{} interrupted installation journal operation(s) must be recovered first",
                install_journals.len()
            )));
        }
        let removal_store = self
            .removal_journal_store()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let update_store = self
            .update_journal_store()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let removal_records = removal_store
            .read_all()
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        let update_records = update_store
            .read_all()
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        if !removal_records.is_empty() && !update_records.is_empty() {
            return Err(RemovalError::Store(
                "conflicting removal and update journals require manual recovery".to_owned(),
            ));
        }
        if removal_records.iter().any(|record| {
            record.receipt().agent() != agent.as_str()
                || record.receipt().scope() != scope.as_str()
                || record.receipt().root_path() != scope_root
        }) || update_records.iter().any(|record| {
            record.receipt().agent() != agent.as_str()
                || record.receipt().scope() != scope.as_str()
                || record.receipt().root_path() != scope_root
        }) {
            return Err(RemovalError::Store(
                "a pending lifecycle operation belongs to a different agent or scope; recover that exact target first".to_owned(),
            ));
        }
        super::update::recover_all(
            &update_store,
            &directory,
            &lock,
            &NoReceiptFaults,
            &NoUpdateFaults,
        )
        .map_err(RemovalError::Journal)?;
        super::removal::recover_all(&removal_store, &directory, &lock, faults)?;
        lock.validate()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let receipts = read_receipts(&directory, true)
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let plan =
            build_removal_plan(&directory, &receipts, resource_id, agent, scope, scope_root)?;
        let summary = super::removal::remove_plan(&plan, &removal_store, directory, &lock, faults)?;
        lock.release()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        Ok(summary)
    }

    /// Reserve the receipt store before the internal project commit so concurrent processes
    /// cannot claim the same destination under different catalog IDs.
    pub(crate) fn reserve(
        &self,
        receipt: InstallationReceipt,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<ReceiptReservation, ReceiptStoreError> {
        receipt
            .validate_install_intent()
            .map_err(|source| validation_error("pending receipt", source))?;
        if !self.directory.is_absolute() {
            return Err(ReceiptStoreError::RelativeDirectoryOverride);
        }
        faults
            .before(ReceiptOperation::CreateStore)
            .map_err(|error| map_io("create receipt storage", error))?;
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| map_directory_error("create or inspect", error))?;
        let lock = WriterLock::acquire(directory.clone(), faults)?;
        let reservation = ReceiptReservation {
            directory,
            receipt,
            lock,
            released: false,
        };
        let journal_store = self.journal_store()?;
        let journal_records =
            journal_store
                .read_all()
                .map_err(|error| ReceiptStoreError::Journal {
                    detail: error.to_string(),
                })?;
        let removal_count = self
            .removal_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        let update_count = self
            .update_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        if !journal_records.is_empty() || removal_count != 0 || update_count != 0 {
            return Err(ReceiptStoreError::RecoveryRequired {
                operation_count: journal_records.len() + removal_count + update_count,
            });
        }
        let existing = read_receipts(&reservation.directory, true);
        match existing {
            Ok(receipts) => {
                if let Err(error) = validate_new_claim(&reservation.receipt, &receipts) {
                    reservation.release()?;
                    return Err(error);
                }
            }
            Err(error) => {
                reservation.release()?;
                return Err(error);
            }
        }
        Ok(reservation)
    }

    pub(crate) fn journal_store(&self) -> Result<JournalStore, ReceiptStoreError> {
        let data_directory = self
            .directory
            .parent()
            .ok_or(ReceiptStoreError::UnsafeDirectory)?;
        Ok(JournalStore::new(data_directory.join("operations")))
    }

    pub(crate) fn removal_journal_store(&self) -> Result<RemovalJournalStore, ReceiptStoreError> {
        let data_directory = self
            .directory
            .parent()
            .ok_or(ReceiptStoreError::UnsafeDirectory)?;
        Ok(RemovalJournalStore::new(data_directory.join("removals")))
    }

    pub(crate) fn update_journal_store(&self) -> Result<UpdateJournalStore, ReceiptStoreError> {
        let data_directory = self
            .directory
            .parent()
            .ok_or(ReceiptStoreError::UnsafeDirectory)?;
        Ok(UpdateJournalStore::new(data_directory.join("updates")))
    }

    /// Recover any prior lifecycle operation, then replace one still-matching immutable update
    /// plan while holding the shared writer lock through receipt and journal finalization.
    pub(crate) fn update(
        &self,
        plan: &UpdatePlan,
        faults: &dyn UpdateFaultInjector,
    ) -> Result<UpdateSummary, UpdateError> {
        let directory = self
            .open_existing_directory()
            .map_err(|error| UpdateError::Store(error.to_string()))?
            .ok_or(UpdateError::MissingReceipt)?;
        let mut lock = WriterLock::acquire(directory.clone(), &NoReceiptFaults)
            .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
        lock.validate()
            .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
        let result = (|| {
            super::commit::validate_shared_path_snapshot(plan.replacement()).map_err(|error| {
                UpdateError::Conflict(format!(
                    "shared .agents/skills compatibility state changed before update: {error}"
                ))
            })?;
            let install_store = self
                .journal_store()
                .map_err(|error| UpdateError::Store(error.to_string()))?;
            let install_records = install_store
                .read_all()
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            let removal_store = self
                .removal_journal_store()
                .map_err(|error| UpdateError::Store(error.to_string()))?;
            let removal_records = removal_store
                .read_all()
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            let update_store = self
                .update_journal_store()
                .map_err(|error| UpdateError::Store(error.to_string()))?;
            let update_records = update_store
                .read_all()
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            let target_receipt = plan.receipt();
            if install_records
                .iter()
                .any(|record| !same_operation_scope(record.receipt(), target_receipt))
                || removal_records
                    .iter()
                    .any(|record| !same_operation_scope(record.receipt(), target_receipt))
                || update_records
                    .iter()
                    .any(|record| !same_operation_scope(record.receipt(), target_receipt))
            {
                return Err(UpdateError::RecoveryRequired(
                    "a pending lifecycle operation belongs to a different agent or scope"
                        .to_owned(),
                ));
            }
            for record in install_records {
                lock.validate()
                    .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
                recover_operation(
                    &install_store,
                    &record,
                    &directory,
                    &NoRecoveryInstallFaults,
                    &super::journal::NoJournalFaults,
                    &NoReceiptFaults,
                )
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            }
            super::removal::recover_all(&removal_store, &directory, &lock, &NoRemovalFaults)
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            super::update::recover_all(&update_store, &directory, &lock, &NoReceiptFaults, faults)
                .map_err(UpdateError::RecoveryRequired)?;
            lock.validate()
                .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
            super::update::execute_locked(
                plan,
                &directory,
                &lock,
                &update_store,
                &NoReceiptFaults,
                faults,
            )
        })();
        match result {
            Ok(summary) => {
                lock.release()
                    .map_err(|error| UpdateError::Store(error.to_string()))?;
                Ok(summary)
            }
            Err(error) => {
                let _ = lock.release();
                Err(error)
            }
        }
    }

    fn ensure_no_pending_operations(&self) -> Result<(), ReceiptStoreError> {
        let install_count = self
            .journal_store()?
            .read_all()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?
            .len();
        let removal_count = self
            .removal_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        let update_count = self
            .update_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        let operation_count = install_count + removal_count + update_count;
        if operation_count != 0 {
            return Err(ReceiptStoreError::RecoveryRequired { operation_count });
        }
        Ok(())
    }

    fn ensure_no_pending_removals(&self) -> Result<(), ReceiptStoreError> {
        let removal_count = self
            .removal_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        let update_count = self
            .update_journal_store()?
            .pending_count()
            .map_err(|error| ReceiptStoreError::Journal {
                detail: error.to_string(),
            })?;
        if removal_count != 0 || update_count != 0 {
            return Err(ReceiptStoreError::RecoveryRequired {
                operation_count: removal_count + update_count,
            });
        }
        Ok(())
    }

    /// Internal startup/retry API. Recovery and installation share the same OS-backed writer lock.
    #[cfg(test)]
    pub(crate) fn recover_with_faults(
        &self,
        journal_faults: &dyn JournalFaultInjector,
        receipt_faults: &dyn ReceiptFaultInjector,
    ) -> Result<Vec<RecoveryOutcome>, RecoveryError> {
        self.recover_with_all_faults(&NoRecoveryInstallFaults, journal_faults, receipt_faults)
    }

    #[cfg(test)]
    pub(crate) fn recover_with_all_faults(
        &self,
        install_faults: &dyn InstallFaultInjector,
        journal_faults: &dyn JournalFaultInjector,
        receipt_faults: &dyn ReceiptFaultInjector,
    ) -> Result<Vec<RecoveryOutcome>, RecoveryError> {
        self.recover_with_scope_filter(None, install_faults, journal_faults, receipt_faults)
    }

    pub(crate) fn recover_scope_with_faults(
        &self,
        scope: &super::scope::ResolvedScope,
        journal_faults: &dyn JournalFaultInjector,
        receipt_faults: &dyn ReceiptFaultInjector,
    ) -> Result<Vec<RecoveryOutcome>, RecoveryError> {
        self.recover_with_scope_filter(
            Some(scope),
            &NoRecoveryInstallFaults,
            journal_faults,
            receipt_faults,
        )
    }

    fn recover_with_scope_filter(
        &self,
        selected_scope: Option<&super::scope::ResolvedScope>,
        install_faults: &dyn InstallFaultInjector,
        journal_faults: &dyn JournalFaultInjector,
        receipt_faults: &dyn ReceiptFaultInjector,
    ) -> Result<Vec<RecoveryOutcome>, RecoveryError> {
        if !self.directory.is_absolute() {
            return Err(RecoveryError::UnsafeStore);
        }
        let directory = SecureDir::open_or_create_absolute(&self.directory).map_err(|error| {
            RecoveryError::Store(map_directory_error("open recovery store", error))
        })?;
        let mut lock =
            WriterLock::acquire(directory.clone(), receipt_faults).map_err(RecoveryError::Store)?;
        lock.validate().map_err(RecoveryError::Store)?;
        let journal_store = self.journal_store().map_err(RecoveryError::Store)?;
        let result = (|| {
            let records = journal_store.read_all().map_err(RecoveryError::Journal)?;
            let removal_store = self.removal_journal_store().map_err(RecoveryError::Store)?;
            let update_store = self.update_journal_store().map_err(RecoveryError::Store)?;
            let removal_records = removal_store
                .read_all()
                .map_err(|error| RecoveryError::Removal(error.to_string()))?;
            let update_records = update_store
                .read_all()
                .map_err(|error| RecoveryError::Update(error.to_string()))?;
            if selected_scope.is_some_and(|scope| {
                records
                    .iter()
                    .any(|record| !receipt_matches_scope(record.receipt(), scope))
                    || removal_records
                        .iter()
                        .any(|record| !receipt_matches_scope(record.receipt(), scope))
                    || update_records
                        .iter()
                        .any(|record| !receipt_matches_scope(record.receipt(), scope))
            }) {
                return Err(RecoveryError::ForeignOperation);
            }
            let removal_count = removal_records.len();
            let update_count = update_records.len();
            if (!records.is_empty() && (removal_count != 0 || update_count != 0))
                || (removal_count != 0 && update_count != 0)
            {
                return Err(RecoveryError::LifecycleConflict);
            }
            let mut outcomes = Vec::new();
            for record in records {
                lock.validate().map_err(RecoveryError::Store)?;
                outcomes.push(recover_operation(
                    &journal_store,
                    &record,
                    &directory,
                    install_faults,
                    journal_faults,
                    receipt_faults,
                )?);
            }
            super::removal::recover_all(&removal_store, &directory, &lock, &NoRemovalFaults)
                .map_err(|error| RecoveryError::Removal(error.to_string()))?;
            super::update::recover_all(
                &update_store,
                &directory,
                &lock,
                receipt_faults,
                &NoUpdateFaults,
            )
            .map_err(RecoveryError::Update)?;
            Ok(outcomes)
        })();
        match result {
            Ok(outcomes) => {
                lock.release().map_err(RecoveryError::Store)?;
                Ok(outcomes)
            }
            Err(error) => {
                let _ = lock.release();
                Err(error)
            }
        }
    }

    /// Rehash the exact current selected-agent tree. Receipt paths are validated before
    /// any referenced project location is opened. Listing deliberately does not call this.
    #[cfg(test)]
    pub(crate) fn verify_current_content(
        &self,
        receipt: &InstallationReceipt,
    ) -> Result<ReceiptContentState, ReceiptStoreError> {
        receipt
            .validate()
            .map_err(|source| validation_error("receipt", source))?;
        verify_receipt_tree(receipt, None)
    }

    fn open_existing_directory(&self) -> Result<Option<SecureDir>, ReceiptStoreError> {
        if !self.directory.is_absolute() {
            return Err(ReceiptStoreError::RelativeDirectoryOverride);
        }
        match SecureDir::open_absolute(&self.directory) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput
                        | io::ErrorKind::PermissionDenied
                        | io::ErrorKind::NotADirectory
                ) =>
            {
                Err(ReceiptStoreError::UnsafeDirectory)
            }
            Err(error) => Err(map_io("inspect receipt storage", error)),
        }
    }
}

/// Resolve configured data-root aliases without creating anything. A link at the data root itself
/// is rejected, while a platform alias in an existing ancestor (for example macOS `/var`) is
/// canonicalized before the no-follow receipt child is opened.
fn map_os_path_error(error: crate::os_paths::OsPathError) -> ReceiptStoreError {
    match error {
        crate::os_paths::OsPathError::DirectoryUnavailable => {
            ReceiptStoreError::DirectoryUnavailable
        }
        crate::os_paths::OsPathError::RelativeDataOverride => {
            ReceiptStoreError::RelativeDirectoryOverride
        }
        crate::os_paths::OsPathError::UnsafeDataDirectory => ReceiptStoreError::UnsafeDirectory,
        crate::os_paths::OsPathError::RelativeCacheOverride => {
            ReceiptStoreError::PathResolution("THRN_CACHE_DIR must be absolute".to_owned())
        }
        #[cfg(windows)]
        crate::os_paths::OsPathError::RelativePlatformDirectory { variable } => {
            ReceiptStoreError::PathResolution(format!("{variable} must be absolute"))
        }
        crate::os_paths::OsPathError::Io { action, kind } => {
            ReceiptStoreError::PathResolution(format!("could not {action} ({kind})"))
        }
    }
}

#[cfg(test)]
fn resolve_data_directory(path: &std::path::Path) -> Result<PathBuf, ReceiptStoreError> {
    if !path.is_absolute() {
        return Err(ReceiptStoreError::RelativeDirectoryOverride);
    }
    crate::os_paths::resolve_data_directory(path).map_err(map_os_path_error)
}

fn build_removal_plan(
    directory: &SecureDir,
    receipts: &[(String, InstallationReceipt)],
    resource_id: &str,
    agent: super::plan::InstallAgent,
    scope: super::plan::InstallScope,
    scope_root: &std::path::Path,
) -> Result<RemovalPlan, RemovalError> {
    use std::str::FromStr;

    let requested = crate::models::catalog::SkillId::from_str(resource_id).map_err(|_| {
        RemovalError::Conflict("resource ID is not a valid canonical catalog ID".to_owned())
    })?;
    if requested.as_str() != resource_id {
        return Err(RemovalError::Conflict(
            "resource ID must be the exact canonical catalog ID recorded in the receipt".to_owned(),
        ));
    }
    let scope_text = scope_root.to_str().ok_or_else(|| {
        RemovalError::Conflict("canonical scope path is not valid UTF-8".to_owned())
    })?;
    let matches = receipts
        .iter()
        .filter(|(_, receipt)| {
            receipt.catalog_resource_id() == requested.as_str()
                && receipt.agent() == agent.as_str()
                && receipt.scope() == scope.as_str()
                && receipt.root_path() == scope_root
        })
        .collect::<Vec<_>>();
    let [(name, receipt)] = matches.as_slice() else {
        if matches.is_empty() {
            return Err(RemovalError::MissingReceipt);
        }
        return Err(RemovalError::Conflict(
            "multiple ownership receipts claim this canonical resource and scope".to_owned(),
        ));
    };
    if receipt.agent() != agent.as_str()
        || receipt.scope() != scope.as_str()
        || receipt.root_path().to_str() != Some(scope_text)
    {
        return Err(RemovalError::Conflict(
            "receipt agent, scope, or canonical scope path differs from the selected target"
                .to_owned(),
        ));
    }
    let file = directory.open_regular_file(name).map_err(|error| {
        RemovalError::Store(format!(
            "could not open validated ownership receipt ({})",
            error.kind()
        ))
    })?;
    if SecureDir::hard_link_count(&file).map_err(|error| {
        RemovalError::Store(format!(
            "could not inspect ownership receipt links ({})",
            error.kind()
        ))
    })? != 1
    {
        return Err(RemovalError::Conflict(
            "ownership receipt is hard-linked and cannot be retired safely".to_owned(),
        ));
    }
    let current_receipt = read_receipt_file(directory, name)
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    if current_receipt != *receipt {
        return Err(RemovalError::Conflict(
            "ownership receipt changed while removal planning was in progress".to_owned(),
        ));
    }
    let receipt_file_identity = SecureDir::file_identity(&file).map_err(|error| {
        RemovalError::Store(format!(
            "could not identify ownership receipt ({})",
            error.kind()
        ))
    })?;
    let receipt_directory_identity = directory.identity().map_err(|error| {
        RemovalError::Store(format!(
            "could not identify receipt storage ({})",
            error.kind()
        ))
    })?;
    RemovalPlan::verify(
        (*receipt).clone(),
        (*name).clone(),
        receipt_directory_identity,
        receipt_file_identity,
    )
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ReceiptOperation {
    CreateStore,
    CreateWriterLock,
    CreateTemporaryReceipt,
    WriteTemporaryReceipt,
    SyncTemporaryReceipt,
    VerifyTemporaryReceipt,
    PublishNoReplace,
    SyncReceiptDirectory,
}

pub(crate) trait ReceiptFaultInjector: Send + Sync {
    fn before(&self, _operation: ReceiptOperation) -> io::Result<()> {
        Ok(())
    }

    fn after(&self, _operation: ReceiptOperation) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct NoReceiptFaults;
impl ReceiptFaultInjector for NoReceiptFaults {}

pub(crate) struct ReceiptReservation {
    directory: SecureDir,
    receipt: InstallationReceipt,
    lock: WriterLock,
    released: bool,
}

impl ReceiptReservation {
    pub(crate) fn release(mut self) -> Result<(), ReceiptStoreError> {
        self.release_inner()
    }

    pub(crate) fn publish(
        &mut self,
        committed: &CommittedInstallation,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<InstallationReceipt, ReceiptStoreError> {
        self.publish_inner(committed, faults)
    }

    fn publish_inner(
        &mut self,
        committed: &CommittedInstallation,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<InstallationReceipt, ReceiptStoreError> {
        self.lock.validate()?;
        let expected = InstallationReceipt::from_committed_plan(
            committed.plan(),
            committed.inventory(),
            committed.project_identity(),
            self.receipt.installed_at_unix_seconds(),
        )
        .and_then(|receipt| {
            receipt.with_installed_directory_identity(committed.destination_identity())
        })
        .map_err(|source| validation_error("committed receipt", source))?;
        let reserved_intent = self.receipt.clone();
        if expected
            != reserved_intent
                .clone()
                .with_installed_directory_identity(committed.destination_identity())
                .map_err(|source| validation_error("committed receipt", source))?
        {
            return Err(ReceiptStoreError::CommittedIdentityMismatch);
        }
        self.receipt = expected.clone();
        match verify_receipt_tree(&self.receipt, Some(committed.destination_identity()))? {
            ReceiptContentState::Verified => {}
            ReceiptContentState::Missing => return Err(ReceiptStoreError::CommittedContentMissing),
            ReceiptContentState::Modified => {
                return Err(ReceiptStoreError::CommittedContentModified);
            }
        }

        let current = read_receipts(&self.directory, true)?;
        validate_new_claim(&self.receipt, &current)?;
        let bytes = self
            .receipt
            .to_json()
            .map_err(|source| validation_error("committed receipt", source))?;
        let (temporary_name, mut temporary_file) = self.create_temporary_receipt(faults)?;
        let temporary_identity = SecureDir::file_identity(&temporary_file)
            .map_err(|error| map_io("identify temporary receipt", error))?;
        let mut temporary_guard = TempReceipt {
            directory: self.directory.clone(),
            name: temporary_name.clone(),
            identity: temporary_identity,
            active: true,
        };

        let split_at = (bytes.len() / 2).max(1);
        faults
            .before(ReceiptOperation::WriteTemporaryReceipt)
            .map_err(|error| map_io("write temporary receipt", error))?;
        temporary_file
            .write_all(&bytes[..split_at])
            .map_err(|error| map_io("write temporary receipt", error))?;
        faults
            .before(ReceiptOperation::WriteTemporaryReceipt)
            .map_err(|error| map_io("write temporary receipt", error))?;
        temporary_file
            .write_all(&bytes[split_at..])
            .map_err(|error| map_io("write temporary receipt", error))?;
        temporary_file
            .flush()
            .map_err(|error| map_io("flush temporary receipt", error))?;
        faults
            .before(ReceiptOperation::SyncTemporaryReceipt)
            .map_err(|error| map_io("synchronize temporary receipt", error))?;
        temporary_file
            .sync_all()
            .map_err(|error| map_io("synchronize temporary receipt", error))?;
        faults
            .after(ReceiptOperation::SyncTemporaryReceipt)
            .map_err(|error| map_io("synchronize temporary receipt", error))?;
        faults
            .before(ReceiptOperation::VerifyTemporaryReceipt)
            .map_err(|error| map_io("verify temporary receipt", error))?;
        let written = read_receipt_file(&self.directory, &temporary_name)?;
        if written != self.receipt {
            return Err(ReceiptStoreError::TemporaryReceiptMismatch);
        }

        publish_receipt_no_replace(
            &self.directory,
            &self.lock,
            &self.receipt,
            &current,
            &temporary_name,
            temporary_identity,
            &mut temporary_file,
            faults,
        )?;
        temporary_guard.active = false;
        Ok(self.receipt.clone())
    }

    fn create_temporary_receipt(
        &self,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<(String, fs::File), ReceiptStoreError> {
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let name =
                random_temp_name().map_err(|error| map_io("name temporary receipt", error))?;
            faults
                .before(ReceiptOperation::CreateTemporaryReceipt)
                .map_err(|error| map_io("create temporary receipt", error))?;
            match self.directory.create_regular_file(&name) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(map_io("create temporary receipt", error)),
            }
        }
        Err(ReceiptStoreError::TemporaryNameExhausted)
    }

    fn release_inner(&mut self) -> Result<(), ReceiptStoreError> {
        if self.released {
            return Ok(());
        }
        self.lock.release()?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ReceiptReservation {
    fn drop(&mut self) {
        // WriterLock releases its OS lock when this reservation is dropped.
    }
}

/// The shared receipt publication boundary for installs and updates. It validates the exact
/// prepared bytes and uses the existing no-replace rename primitive; callers cannot overwrite a
/// deterministic receipt filename.
#[allow(clippy::too_many_arguments)]
pub(super) fn publish_receipt_no_replace(
    directory: &SecureDir,
    lock: &WriterLock,
    receipt: &InstallationReceipt,
    existing_claims: &[(String, InstallationReceipt)],
    temporary_name: &str,
    temporary_identity: FileIdentity,
    temporary_file: &mut fs::File,
    faults: &dyn ReceiptFaultInjector,
) -> Result<FileIdentity, ReceiptStoreError> {
    lock.validate()?;
    validate_new_claim(receipt, existing_claims)?;
    if SecureDir::file_identity(temporary_file)
        .map_err(|error| map_io("identify temporary receipt", error))?
        != temporary_identity
        || SecureDir::hard_link_count(temporary_file)
            .map_err(|error| map_io("inspect temporary receipt links", error))?
            != 1
        || read_receipt_file(directory, temporary_name)? != *receipt
    {
        return Err(ReceiptStoreError::TemporaryReceiptMismatch);
    }
    let final_name = receipt
        .receipt_filename()
        .map_err(|source| validation_error("committed receipt", source))?;
    faults
        .before(ReceiptOperation::PublishNoReplace)
        .map_err(|error| map_io("publish receipt without replacement", error))?;
    lock.validate()?;
    directory
        .rename_file_child_noreplace(temporary_name, &final_name, temporary_file)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                ReceiptStoreError::DuplicateIdentity
            } else {
                map_io("publish receipt without replacement", error)
            }
        })?;
    faults
        .after(ReceiptOperation::PublishNoReplace)
        .map_err(|error| map_io("publish receipt without replacement", error))?;
    faults
        .before(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    directory
        .sync_directory()
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    faults
        .after(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    let published = directory
        .open_regular_file(&final_name)
        .map_err(|error| map_io("verify published receipt", error))?;
    let identity = SecureDir::file_identity(&published)
        .map_err(|error| map_io("identify published receipt", error))?;
    if identity != temporary_identity || read_receipt_file(directory, &final_name)? != *receipt {
        return Err(ReceiptStoreError::TemporaryReceiptMismatch);
    }
    Ok(identity)
}

/// Write and verify a transition receipt temporary without publishing it. The returned name and
/// identity must be persisted in the update journal before the old receipt is moved.
pub(super) fn prepare_transition_receipt_temp(
    directory: &SecureDir,
    operation_id: &str,
    receipt: &InstallationReceipt,
    faults: &dyn ReceiptFaultInjector,
) -> Result<(String, FileIdentity), ReceiptStoreError> {
    let bytes = receipt
        .to_json()
        .map_err(|source| validation_error("replacement receipt", source))?;
    let name = format!(".pending-update-{operation_id}");
    faults
        .before(ReceiptOperation::CreateTemporaryReceipt)
        .map_err(|error| map_io("create update receipt temporary", error))?;
    let mut file = directory
        .create_regular_file(&name)
        .map_err(|error| map_io("create update receipt temporary", error))?;
    let identity = SecureDir::file_identity(&file)
        .map_err(|error| map_io("identify update receipt temporary", error))?;
    file.write_all(&bytes)
        .map_err(|error| map_io("write update receipt temporary", error))?;
    file.flush()
        .map_err(|error| map_io("flush update receipt temporary", error))?;
    faults
        .before(ReceiptOperation::SyncTemporaryReceipt)
        .map_err(|error| map_io("synchronize update receipt temporary", error))?;
    file.sync_all()
        .map_err(|error| map_io("synchronize update receipt temporary", error))?;
    faults
        .after(ReceiptOperation::SyncTemporaryReceipt)
        .map_err(|error| map_io("synchronize update receipt temporary", error))?;
    if read_receipt_file(directory, &name)? != *receipt
        || SecureDir::file_identity(&file)
            .map_err(|error| map_io("verify update receipt temporary", error))?
            != identity
    {
        return Err(ReceiptStoreError::TemporaryReceiptMismatch);
    }
    faults
        .before(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    directory
        .sync_directory()
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    faults
        .after(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    Ok((name, identity))
}

pub(super) struct WriterLock {
    directory: SecureDir,
    file: fs::File,
    identity: FileIdentity,
    released: bool,
}

impl WriterLock {
    fn acquire(
        directory: SecureDir,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<Self, ReceiptStoreError> {
        faults
            .before(ReceiptOperation::CreateWriterLock)
            .map_err(|error| map_io("reserve receipt storage", error))?;
        let file = directory
            .open_or_create_lock_file(WRITER_LOCK_NAME)
            .map_err(|error| map_io("reserve receipt storage", error))?;
        if file
            .metadata()
            .map_err(|error| map_io("inspect receipt-store lock", error))?
            .len()
            != 0
        {
            return Err(ReceiptStoreError::UnsafeObject {
                name: WRITER_LOCK_NAME.to_owned(),
            });
        }
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => ReceiptStoreError::BusyOrInterrupted,
            std::fs::TryLockError::Error(error) => map_io("lock receipt storage", error),
        })?;
        directory
            .sync_directory()
            .map_err(|error| map_io("synchronize receipt-store lock", error))?;
        let identity = SecureDir::file_identity(&file)
            .map_err(|error| map_io("identify receipt-store lock", error))?;
        let lock = Self {
            directory,
            file,
            identity,
            released: false,
        };
        lock.validate()?;
        Ok(lock)
    }

    fn try_acquire_existing(directory: SecureDir) -> Result<Option<Self>, ReceiptStoreError> {
        let entries = directory
            .entries()
            .map_err(|error| map_io("inspect receipt-store lock", error))?;
        let Some((_, kind)) = entries
            .iter()
            .find(|(name, _)| name.to_str() == Some(WRITER_LOCK_NAME))
        else {
            return Ok(None);
        };
        if *kind != NodeKind::RegularFile {
            return Err(ReceiptStoreError::UnsafeObject {
                name: WRITER_LOCK_NAME.to_owned(),
            });
        }
        let file = directory
            .open_lock_file(WRITER_LOCK_NAME)
            .map_err(|error| map_io("open receipt-store read reservation", error))?;
        if file
            .metadata()
            .map_err(|error| map_io("inspect receipt-store read reservation", error))?
            .len()
            != 0
        {
            return Err(ReceiptStoreError::UnsafeObject {
                name: WRITER_LOCK_NAME.to_owned(),
            });
        }
        match file.try_lock() {
            Ok(()) => {
                let identity = SecureDir::file_identity(&file)
                    .map_err(|error| map_io("identify receipt-store read reservation", error))?;
                let lock = Self {
                    directory,
                    file,
                    identity,
                    released: false,
                };
                lock.validate()?;
                Ok(Some(lock))
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(ReceiptStoreError::BusyOrInterrupted),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(map_io("reserve receipt-store listing", error))
            }
        }
    }

    pub(super) fn validate(&self) -> Result<(), ReceiptStoreError> {
        let current = self
            .directory
            .open_lock_file(WRITER_LOCK_NAME)
            .map_err(|error| map_io("verify receipt-store lock", error))?;
        if SecureDir::file_identity(&current)
            .map_err(|error| map_io("identify receipt-store lock", error))?
            != self.identity
            || SecureDir::file_identity(&self.file)
                .map_err(|error| map_io("identify receipt-store lock", error))?
                != self.identity
        {
            return Err(ReceiptStoreError::WriterLockChanged);
        }
        Ok(())
    }

    fn release(&mut self) -> Result<(), ReceiptStoreError> {
        if self.released {
            return Ok(());
        }
        self.validate()?;
        self.file
            .unlock()
            .map_err(|error| map_io("release receipt-store lock", error))?;
        self.released = true;
        Ok(())
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.file.unlock();
        }
    }
}

fn recover_operation(
    journal_store: &JournalStore,
    initial: &JournalRecord,
    receipt_directory: &SecureDir,
    install_faults: &dyn InstallFaultInjector,
    journal_faults: &dyn JournalFaultInjector,
    receipt_faults: &dyn ReceiptFaultInjector,
) -> Result<RecoveryOutcome, RecoveryError> {
    let operation_id = initial.operation_id().to_owned();
    let mut journal = journal_store
        .resume(initial.clone())
        .map_err(RecoveryError::Journal)?;
    match initial.state() {
        OperationState::Prepared => {
            journal
                .transition(OperationState::RolledBack, journal_faults)
                .map_err(RecoveryError::Journal)?;
            journal_store
                .cleanup_completed(&operation_id, journal_faults)
                .map_err(RecoveryError::Journal)?;
            return Ok(RecoveryOutcome::RolledBack);
        }
        OperationState::FilesystemCommitInProgress => {
            let (project, agents, skills) = recovery_parent_directories(initial)?;
            let Some(skills) = skills else {
                validate_and_remove_recovery_parents(
                    &mut journal,
                    &project,
                    agents,
                    None,
                    install_faults,
                    journal_faults,
                )?;
                journal
                    .transition(OperationState::RolledBack, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                journal_store
                    .cleanup_completed(&operation_id, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                return Ok(RecoveryOutcome::RolledBack);
            };
            let target = open_recovery_directory(initial, &skills, initial.receipt().skill_name())?;
            if let Some(target) = target {
                let identity = target.identity().map_err(|error| {
                    recovery_conflict(initial, format!("cannot identify destination: {error}"))
                })?;
                if initial.staging_identity() != Some(identity)
                    || verify_receipt_inventory_at(initial.receipt(), target)?
                        != ReceiptContentState::Verified
                {
                    return Err(recovery_conflict(
                        initial,
                        "destination identity or complete file inventory differs",
                    ));
                }
                skills.sync_directory().map_err(|error| {
                    recovery_conflict(
                        initial,
                        format!("cannot synchronize committed directory entry: {error}"),
                    )
                })?;
                journal
                    .set_destination_identity(identity, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                journal
                    .transition(OperationState::DirectoryCommitVerified, journal_faults)
                    .map_err(RecoveryError::Journal)?;
            } else {
                let stage = open_recovery_directory(initial, &skills, initial.staging_name())?;
                if let Some(stage) = stage {
                    let identity = stage.identity().map_err(|error| {
                        recovery_conflict(
                            initial,
                            format!("cannot identify staging directory: {error}"),
                        )
                    })?;
                    if initial.staging_identity() != Some(identity) {
                        return Err(recovery_conflict(
                            initial,
                            "staging directory identity differs",
                        ));
                    }
                    verify_partial_operation_tree(journal.record(), &stage)?;
                    remove_operation_stage(
                        &mut journal,
                        &skills,
                        &stage,
                        install_faults,
                        journal_faults,
                    )?;
                } else if initial.staging_identity().is_some()
                    && !initial.has_cleanup_intent("staging-root")
                {
                    return Err(recovery_conflict(
                        initial,
                        "recorded staging directory is missing without a durable cleanup intent",
                    ));
                }
                validate_and_remove_recovery_parents(
                    &mut journal,
                    &project,
                    agents,
                    Some(&skills),
                    install_faults,
                    journal_faults,
                )?;
                journal
                    .transition(OperationState::RolledBack, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                journal_store
                    .cleanup_completed(&operation_id, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                return Ok(RecoveryOutcome::RolledBack);
            }
        }
        OperationState::DirectoryCommitVerified
        | OperationState::ReceiptPublicationPending
        | OperationState::ReceiptPublished => {}
        OperationState::Complete => {
            verify_completed_operation(initial, receipt_directory)?;
            journal_store
                .cleanup_completed(&operation_id, journal_faults)
                .map_err(RecoveryError::Journal)?;
            return Ok(RecoveryOutcome::CleanedCompletedRecord);
        }
        OperationState::RolledBack => {
            verify_rolled_back_operation(initial)?;
            journal_store
                .cleanup_completed(&operation_id, journal_faults)
                .map_err(RecoveryError::Journal)?;
            return Ok(RecoveryOutcome::RolledBack);
        }
    }

    let current = journal.record().clone();
    let expected_identity = current.destination_identity().ok_or_else(|| {
        recovery_conflict(&current, "committed destination identity was not recorded")
    })?;
    if verify_receipt_tree(current.receipt(), Some(expected_identity)).map_err(|error| {
        recovery_conflict(
            &current,
            format!("cannot verify committed destination: {error}"),
        )
    })? != ReceiptContentState::Verified
    {
        return Err(recovery_conflict(
            &current,
            "committed destination is missing or does not match the recorded hashes",
        ));
    }
    sync_committed_parent(&current)?;
    if current.state() == OperationState::DirectoryCommitVerified {
        journal
            .transition(OperationState::ReceiptPublicationPending, journal_faults)
            .map_err(RecoveryError::Journal)?;
    }
    finalize_recovered_receipt(
        journal.record(),
        receipt_directory,
        expected_identity,
        receipt_faults,
    )?;
    if journal.record().state() == OperationState::ReceiptPublicationPending {
        journal
            .transition(OperationState::ReceiptPublished, journal_faults)
            .map_err(RecoveryError::Journal)?;
    }
    if journal.record().state() == OperationState::ReceiptPublished {
        journal
            .transition(OperationState::Complete, journal_faults)
            .map_err(RecoveryError::Journal)?;
    }
    journal_store
        .cleanup_completed(&operation_id, journal_faults)
        .map_err(RecoveryError::Journal)?;
    Ok(RecoveryOutcome::FinalizedReceipt)
}

fn sync_committed_parent(record: &JournalRecord) -> Result<(), RecoveryError> {
    let (_, _, skills) = recovery_parent_directories(record)?;
    let skills =
        skills.ok_or_else(|| recovery_conflict(record, "committed skills parent is missing"))?;
    skills.sync_directory().map_err(|error| {
        recovery_conflict(
            record,
            format!("cannot synchronize committed parent directory: {error}"),
        )
    })
}

fn verify_completed_operation(
    record: &JournalRecord,
    directory: &SecureDir,
) -> Result<(), RecoveryError> {
    let receipts = read_receipts(directory, true).map_err(|error| {
        recovery_conflict(
            record,
            format!("completed receipt store is invalid: {error}"),
        )
    })?;
    let expected_name = record.receipt().receipt_filename().map_err(|error| {
        recovery_conflict(record, format!("receipt identity is invalid: {error}"))
    })?;
    if !receipts
        .iter()
        .any(|(name, receipt)| name == &expected_name && receipt == record.receipt())
    {
        return Err(recovery_conflict(
            record,
            "completed operation has no matching durable receipt",
        ));
    }
    let expected_identity = record
        .destination_identity()
        .ok_or_else(|| recovery_conflict(record, "completed destination identity is missing"))?;
    if verify_receipt_tree(record.receipt(), Some(expected_identity)).map_err(|error| {
        recovery_conflict(
            record,
            format!("cannot verify completed destination: {error}"),
        )
    })? != ReceiptContentState::Verified
    {
        return Err(recovery_conflict(
            record,
            "completed destination identity or file hashes differ",
        ));
    }
    sync_committed_parent(record)?;
    directory.sync_directory().map_err(|error| {
        recovery_conflict(
            record,
            format!("cannot synchronize completed receipt storage: {error}"),
        )
    })?;
    Ok(())
}

fn verify_rolled_back_operation(record: &JournalRecord) -> Result<(), RecoveryError> {
    let (project, agents, skills) = recovery_parent_directories(record)?;
    if record.staging_identity().is_some()
        && (record
            .created_files()
            .iter()
            .any(|file| !record.has_cleanup_intent(&format!("file:{}", file.path())))
            || record.created_directories().iter().any(|directory| {
                !record.has_cleanup_intent(&format!("directory:{}", directory.path()))
            }))
    {
        return Err(recovery_conflict(
            record,
            "rolled-back staging inventory is missing durable per-object cleanup intents",
        ));
    }
    if let Some(skills) = &skills {
        if open_recovery_directory(record, skills, record.receipt().skill_name())?.is_some() {
            return Err(recovery_conflict(
                record,
                "rolled-back operation still has a destination directory",
            ));
        }
        if open_recovery_directory(record, skills, record.staging_name())?.is_some() {
            return Err(recovery_conflict(
                record,
                "rolled-back operation still has a staging directory",
            ));
        }
        if record.create_skills_directory() {
            return Err(recovery_conflict(
                record,
                "rolled-back operation-created skills parent remains",
            ));
        }
    }
    if record.staging_identity().is_some() && !record.has_cleanup_intent("staging-root") {
        return Err(recovery_conflict(
            record,
            "rolled-back staging directory is missing without a cleanup intent",
        ));
    }
    if record.create_skills_directory()
        && record.project_skills_identity().is_some()
        && skills.is_none()
        && !record.has_cleanup_intent("skills-parent")
    {
        return Err(recovery_conflict(
            record,
            "rolled-back skills parent is missing without a cleanup intent",
        ));
    }
    if record.create_agents_directory() {
        if agents.is_some() {
            return Err(recovery_conflict(
                record,
                "rolled-back operation-created agent parent remains",
            ));
        }
        if record.project_agents_identity().is_some() && !record.has_cleanup_intent("agents-parent")
        {
            return Err(recovery_conflict(
                record,
                "rolled-back agent parent is missing without a cleanup intent",
            ));
        }
    }
    let _ = project;
    Ok(())
}

fn recovery_parent_directories(
    record: &JournalRecord,
) -> Result<(SecureDir, Option<SecureDir>, Option<SecureDir>), RecoveryError> {
    let receipt = record.receipt();
    let agent_directory = receipt.agent_directory();
    let project = SecureDir::open_absolute(&receipt.root_path()).map_err(|error| {
        recovery_conflict(record, format!("project root is unavailable: {error}"))
    })?;
    if project.identity().ok() != Some(receipt.scope_root_identity()) {
        return Err(recovery_conflict(
            record,
            "project filesystem identity changed",
        ));
    }
    let agents = open_recovery_directory(record, &project, agent_directory)?;
    if let Some(agents) = &agents {
        if record.project_agents_identity() != agents.identity().ok() {
            return Err(recovery_conflict(
                record,
                "agent directory identity changed",
            ));
        }
    } else if record.project_agents_identity().is_some() && !record.create_agents_directory() {
        return Err(recovery_conflict(
            record,
            "pre-existing agent directory is missing",
        ));
    }
    let skills = match &agents {
        Some(agents) => open_recovery_directory(record, agents, "skills")?,
        None => None,
    };
    if let Some(skills) = &skills {
        if record.project_skills_identity() != skills.identity().ok() {
            return Err(recovery_conflict(
                record,
                "project skills directory identity changed",
            ));
        }
    } else if record.project_skills_identity().is_some() && !record.create_skills_directory() {
        return Err(recovery_conflict(
            record,
            "pre-existing project skills directory is missing",
        ));
    }
    Ok((project, agents, skills))
}

fn open_recovery_directory(
    record: &JournalRecord,
    parent: &SecureDir,
    name: &str,
) -> Result<Option<SecureDir>, RecoveryError> {
    match parent.open_directory(std::ffi::OsStr::new(name)) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(recovery_conflict(
            record,
            format!(
                "path component {name:?} is unsafe or unavailable ({})",
                error.kind()
            ),
        )),
    }
}

fn validate_and_remove_recovery_parents(
    journal: &mut super::journal::OperationJournal,
    project: &SecureDir,
    agents: Option<SecureDir>,
    skills: Option<&SecureDir>,
    install_faults: &dyn InstallFaultInjector,
    journal_faults: &dyn JournalFaultInjector,
) -> Result<(), RecoveryError> {
    let record = journal.record().clone();
    let agent_directory = record.receipt().agent_directory();
    if record.create_skills_directory() {
        if let Some(skills) = skills {
            if record.project_skills_identity() != skills.identity().ok()
                || !skills
                    .entries()
                    .map_err(|error| {
                        recovery_conflict(
                            &record,
                            format!("cannot inspect skills directory: {error}"),
                        )
                    })?
                    .is_empty()
            {
                return Err(recovery_conflict(
                    &record,
                    "operation-created skills directory changed or is not empty",
                ));
            }
            if let Some(agents) = &agents {
                journal
                    .record_cleanup_intent("skills-parent".to_owned(), journal_faults)
                    .map_err(RecoveryError::Journal)?;
                let current = agents
                    .open_directory(std::ffi::OsStr::new("skills"))
                    .map_err(|error| {
                        recovery_conflict(
                            &record,
                            format!("cannot recheck skills directory before cleanup: {error}"),
                        )
                    })?;
                if current.identity().ok() != record.project_skills_identity()
                    || !current
                        .entries()
                        .map_err(|error| {
                            recovery_conflict(
                                &record,
                                format!("cannot recheck skills directory before cleanup: {error}"),
                            )
                        })?
                        .is_empty()
                {
                    return Err(recovery_conflict(
                        &record,
                        "operation-created skills directory changed before cleanup",
                    ));
                }
                install_faults
                    .before(InstallOperation::RemoveDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            &record,
                            format!("injected or failed skills-parent cleanup: {error}"),
                        )
                    })?;
                agents.remove_directory("skills").map_err(|error| {
                    recovery_conflict(
                        &record,
                        format!("cannot remove empty operation-created skills directory: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::RemoveDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            &record,
                            format!(
                                "skills-parent cleanup completed but reported failure: {error}"
                            ),
                        )
                    })?;
                install_faults
                    .before(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(&record, format!("skills-parent sync failed: {error}"))
                    })?;
                agents.sync_directory().map_err(|error| {
                    recovery_conflict(
                        &record,
                        format!("cannot synchronize skills-parent cleanup: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            &record,
                            format!("skills-parent sync completed but reported failure: {error}"),
                        )
                    })?;
            }
        } else if record.project_skills_identity().is_some()
            && !record.has_cleanup_intent("skills-parent")
        {
            return Err(recovery_conflict(
                &record,
                "recorded skills parent is missing without a durable cleanup intent",
            ));
        }
    }
    if record.create_agents_directory()
        && let Some(agents) = agents
    {
        if record.project_agents_identity() != agents.identity().ok()
            || !agents
                .entries()
                .map_err(|error| {
                    recovery_conflict(&record, format!("cannot inspect agent directory: {error}"))
                })?
                .is_empty()
        {
            return Err(recovery_conflict(
                &record,
                "operation-created agent directory changed or is not empty",
            ));
        }
        journal
            .record_cleanup_intent("agents-parent".to_owned(), journal_faults)
            .map_err(RecoveryError::Journal)?;
        let current = project
            .open_directory(std::ffi::OsStr::new(agent_directory))
            .map_err(|error| {
                recovery_conflict(
                    &record,
                    format!("cannot recheck agent directory before cleanup: {error}"),
                )
            })?;
        if current.identity().ok() != record.project_agents_identity()
            || !current
                .entries()
                .map_err(|error| {
                    recovery_conflict(
                        &record,
                        format!("cannot recheck agent directory before cleanup: {error}"),
                    )
                })?
                .is_empty()
        {
            return Err(recovery_conflict(
                &record,
                "operation-created agent directory changed before cleanup",
            ));
        }
        install_faults
            .before(InstallOperation::RemoveDirectory)
            .map_err(|error| {
                recovery_conflict(
                    &record,
                    format!("injected or failed agent-directory cleanup: {error}"),
                )
            })?;
        project.remove_directory(agent_directory).map_err(|error| {
            recovery_conflict(
                &record,
                format!("cannot remove empty operation-created agent directory: {error}"),
            )
        })?;
        install_faults
            .after(InstallOperation::RemoveDirectory)
            .map_err(|error| {
                recovery_conflict(
                    &record,
                    format!("agent-directory cleanup completed but reported failure: {error}"),
                )
            })?;
        install_faults
            .before(InstallOperation::SyncParentDirectory)
            .map_err(|error| {
                recovery_conflict(&record, format!("project-parent sync failed: {error}"))
            })?;
        project.sync_directory().map_err(|error| {
            recovery_conflict(
                &record,
                format!("cannot synchronize project-parent cleanup: {error}"),
            )
        })?;
        install_faults
            .after(InstallOperation::SyncParentDirectory)
            .map_err(|error| {
                recovery_conflict(
                    &record,
                    format!("project-parent sync completed but reported failure: {error}"),
                )
            })?;
    } else if record.create_agents_directory()
        && record.project_agents_identity().is_some()
        && !record.has_cleanup_intent("agents-parent")
    {
        return Err(recovery_conflict(
            &record,
            "recorded agent parent is missing without a durable cleanup intent",
        ));
    }
    Ok(())
}

fn verify_partial_operation_tree(
    record: &JournalRecord,
    root: &SecureDir,
) -> Result<(), RecoveryError> {
    let expected_files = record
        .receipt()
        .files()
        .iter()
        .map(|file| file.path().to_owned())
        .collect::<BTreeSet<_>>();
    let expected_dirs = record
        .receipt()
        .directories()
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let recorded_files = record
        .created_files()
        .iter()
        .map(|file| file.path().to_owned())
        .collect::<BTreeSet<_>>();
    let mut observed_files = BTreeSet::new();
    let mut observed_directories = BTreeSet::new();
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for (name, kind) in directory.entries().map_err(|error| {
            recovery_conflict(record, format!("cannot inspect staging inventory: {error}"))
        })? {
            let Some(name) = name.to_str() else {
                return Err(recovery_conflict(
                    record,
                    "staging contains a non-UTF-8 path",
                ));
            };
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            match kind {
                NodeKind::Directory => {
                    let expected = record
                        .created_directories()
                        .iter()
                        .find(|entry| entry.path() == path);
                    if !expected_dirs.contains(&path) || expected.is_none() {
                        return Err(recovery_conflict(
                            record,
                            "staging contains an unrecorded directory",
                        ));
                    }
                    let child = directory
                        .open_directory(std::ffi::OsStr::new(name))
                        .map_err(|error| {
                            recovery_conflict(
                                record,
                                format!("cannot open staged directory: {error}"),
                            )
                        })?;
                    if Some(child.identity().map_err(|error| {
                        recovery_conflict(
                            record,
                            format!("cannot identify staged directory: {error}"),
                        )
                    })?) != expected.map(JournalDirectory::identity)
                    {
                        return Err(recovery_conflict(
                            record,
                            "staged directory identity changed",
                        ));
                    }
                    observed_directories.insert(path.clone());
                    stack.push((child, path));
                }
                NodeKind::RegularFile => {
                    if !expected_files.contains(&path) || !recorded_files.contains(&path) {
                        return Err(recovery_conflict(
                            record,
                            "staging contains an unrecorded file",
                        ));
                    }
                    let expected = record
                        .created_files()
                        .iter()
                        .find(|entry| entry.path() == path)
                        .ok_or_else(|| {
                            recovery_conflict(record, "staging file record is missing")
                        })?;
                    let mut file = directory.open_regular_file(name).map_err(|error| {
                        recovery_conflict(record, format!("cannot open staged file: {error}"))
                    })?;
                    if SecureDir::file_identity(&file).ok() != Some(expected.identity()) {
                        return Err(recovery_conflict(record, "staged file identity changed"));
                    }
                    observed_files.insert(path.clone());
                    let expected_receipt = record
                        .receipt()
                        .files()
                        .iter()
                        .find(|entry| entry.path() == path)
                        .ok_or_else(|| {
                            recovery_conflict(record, "expected staged file is not in the receipt")
                        })?;
                    if !receipt_file_matches(&mut file, expected_receipt).map_err(|error| {
                        recovery_conflict(record, format!("cannot hash staged file: {error}"))
                    })? {
                        let detail = if expected.verified() {
                            "a previously verified staged file no longer matches its SHA-256"
                        } else {
                            "staged file is incomplete or its SHA-256 differs"
                        };
                        return Err(recovery_conflict(record, detail));
                    }
                }
                NodeKind::Symlink | NodeKind::Other => {
                    return Err(recovery_conflict(
                        record,
                        "staging contains a symlink, reparse point, or unsupported object",
                    ));
                }
            }
        }
    }
    for entry in record.created_files() {
        if !observed_files.contains(entry.path())
            && !record.has_cleanup_intent(&format!("file:{}", entry.path()))
        {
            return Err(recovery_conflict(
                record,
                "a recorded staged file is missing without a durable cleanup intent",
            ));
        }
    }
    for entry in record.created_directories() {
        if !observed_directories.contains(entry.path())
            && !record.has_cleanup_intent(&format!("directory:{}", entry.path()))
        {
            return Err(recovery_conflict(
                record,
                "a recorded staged directory is missing without a durable cleanup intent",
            ));
        }
    }
    Ok(())
}

fn remove_operation_stage(
    journal: &mut super::journal::OperationJournal,
    skills: &SecureDir,
    root: &SecureDir,
    install_faults: &dyn InstallFaultInjector,
    journal_faults: &dyn JournalFaultInjector,
) -> Result<(), RecoveryError> {
    let files = journal.record().created_files().to_vec();
    for entry in files.iter().rev() {
        let intent = format!("file:{}", entry.path());
        let parent_and_name = open_relative_parent(root, entry.path()).map_err(|error| {
            recovery_conflict(
                journal.record(),
                format!("cannot open staged file parent: {error}"),
            )
        })?;
        let Some((parent, name)) = parent_and_name else {
            if journal.record().has_cleanup_intent(&intent) {
                continue;
            }
            return Err(recovery_conflict(
                journal.record(),
                "staged file parent is missing without a durable cleanup intent",
            ));
        };
        match parent.open_regular_file(&name) {
            Ok(mut file) => {
                if SecureDir::file_identity(&file).ok() != Some(entry.identity()) {
                    return Err(recovery_conflict(
                        journal.record(),
                        "staged file identity changed before cleanup",
                    ));
                }
                let expected = journal
                    .record()
                    .receipt()
                    .files()
                    .iter()
                    .find(|file| file.path() == entry.path())
                    .cloned()
                    .ok_or_else(|| {
                        recovery_conflict(journal.record(), "staged file is not expected")
                    })?;
                if !receipt_file_matches(&mut file, &expected).map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot recheck staged file: {error}"),
                    )
                })? {
                    return Err(recovery_conflict(
                        journal.record(),
                        "staged file hash changed before cleanup",
                    ));
                }
                journal
                    .record_cleanup_intent(intent, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                let mut current = parent.open_regular_file(&name).map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot recheck staged file before cleanup: {error}"),
                    )
                })?;
                if SecureDir::file_identity(&current).ok() != Some(entry.identity())
                    || !receipt_file_matches(&mut current, &expected).map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("cannot rehash staged file before cleanup: {error}"),
                        )
                    })?
                {
                    return Err(recovery_conflict(
                        journal.record(),
                        "staged file changed after its cleanup intent was recorded",
                    ));
                }
                install_faults
                    .before(InstallOperation::RemoveFile)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("injected or failed staged-file cleanup: {error}"),
                        )
                    })?;
                parent.remove_file(&name).map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot remove operation-owned staged file: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::RemoveFile)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("staged-file cleanup completed but reported failure: {error}"),
                        )
                    })?;
                install_faults
                    .before(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("staged-file parent sync failed: {error}"),
                        )
                    })?;
                parent.sync_directory().map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot synchronize staged file cleanup: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!(
                                "staged-file parent sync completed but reported failure: {error}"
                            ),
                        )
                    })?;
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && journal.record().has_cleanup_intent(&intent) => {}
            Err(error) => {
                return Err(recovery_conflict(
                    journal.record(),
                    format!("cannot inspect staged file before cleanup: {error}"),
                ));
            }
        }
    }
    let mut directories = journal.record().created_directories().to_vec();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.path().matches('/').count()));
    for entry in directories {
        let intent = format!("directory:{}", entry.path());
        let Some((parent, name)) = open_relative_parent(root, entry.path()).map_err(|error| {
            recovery_conflict(
                journal.record(),
                format!("cannot open staged directory parent: {error}"),
            )
        })?
        else {
            if journal.record().has_cleanup_intent(&intent) {
                continue;
            }
            return Err(recovery_conflict(
                journal.record(),
                "staged directory parent is missing without a durable cleanup intent",
            ));
        };
        match parent.open_directory(std::ffi::OsStr::new(&name)) {
            Ok(directory) => {
                if directory.identity().ok() != Some(entry.identity())
                    || !directory
                        .entries()
                        .map_err(|error| {
                            recovery_conflict(
                                journal.record(),
                                format!("cannot inspect staged directory: {error}"),
                            )
                        })?
                        .is_empty()
                {
                    return Err(recovery_conflict(
                        journal.record(),
                        "staged directory identity changed or it is not empty",
                    ));
                }
                journal
                    .record_cleanup_intent(intent, journal_faults)
                    .map_err(RecoveryError::Journal)?;
                let current =
                    parent
                        .open_directory(std::ffi::OsStr::new(&name))
                        .map_err(|error| {
                            recovery_conflict(
                                journal.record(),
                                format!("cannot recheck staged directory before cleanup: {error}"),
                            )
                        })?;
                if current.identity().ok() != Some(entry.identity())
                    || !current
                        .entries()
                        .map_err(|error| {
                            recovery_conflict(
                                journal.record(),
                                format!("cannot recheck staged directory before cleanup: {error}"),
                            )
                        })?
                        .is_empty()
                {
                    return Err(recovery_conflict(
                        journal.record(),
                        "staged directory changed after its cleanup intent was recorded",
                    ));
                }
                install_faults
                    .before(InstallOperation::RemoveDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("injected or failed staged-directory cleanup: {error}"),
                        )
                    })?;
                parent.remove_directory(&name).map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot remove staged directory: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::RemoveDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!(
                                "staged-directory cleanup completed but reported failure: {error}"
                            ),
                        )
                    })?;
                install_faults
                    .before(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("staged-directory parent sync failed: {error}"),
                        )
                    })?;
                parent.sync_directory().map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot synchronize staged directory cleanup: {error}"),
                    )
                })?;
                install_faults
                    .after(InstallOperation::SyncParentDirectory)
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("staged-directory parent sync completed but reported failure: {error}"),
                        )
                    })?;
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && journal.record().has_cleanup_intent(&intent) => {}
            Err(error) => {
                return Err(recovery_conflict(
                    journal.record(),
                    format!("cannot inspect staged directory before cleanup: {error}"),
                ));
            }
        }
    }
    let stage_name = journal.record().staging_name().to_owned();
    let stage_intent = "staging-root";
    let stage_identity = journal
        .record()
        .staging_identity()
        .ok_or_else(|| recovery_conflict(journal.record(), "staging identity was not recorded"))?;
    match skills.open_directory(std::ffi::OsStr::new(&stage_name)) {
        Ok(current) => {
            if current.identity().ok() != Some(stage_identity)
                || !current
                    .entries()
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("cannot inspect staging directory: {error}"),
                        )
                    })?
                    .is_empty()
            {
                return Err(recovery_conflict(
                    journal.record(),
                    "staging root identity changed or it is not empty",
                ));
            }
            journal
                .record_cleanup_intent(stage_intent.to_owned(), journal_faults)
                .map_err(RecoveryError::Journal)?;
            let current = skills
                .open_directory(std::ffi::OsStr::new(&stage_name))
                .map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("cannot recheck staging directory before cleanup: {error}"),
                    )
                })?;
            if current.identity().ok() != Some(stage_identity)
                || !current
                    .entries()
                    .map_err(|error| {
                        recovery_conflict(
                            journal.record(),
                            format!("cannot recheck staging directory before cleanup: {error}"),
                        )
                    })?
                    .is_empty()
            {
                return Err(recovery_conflict(
                    journal.record(),
                    "staging root changed after its cleanup intent was recorded",
                ));
            }
            install_faults
                .before(InstallOperation::RemoveDirectory)
                .map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("injected or failed staging-root cleanup: {error}"),
                    )
                })?;
            skills.remove_directory(&stage_name).map_err(|error| {
                recovery_conflict(
                    journal.record(),
                    format!("cannot remove empty operation staging directory: {error}"),
                )
            })?;
            install_faults
                .after(InstallOperation::RemoveDirectory)
                .map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("staging-root cleanup completed but reported failure: {error}"),
                    )
                })?;
            install_faults
                .before(InstallOperation::SyncParentDirectory)
                .map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("staging-root parent sync failed: {error}"),
                    )
                })?;
            skills.sync_directory().map_err(|error| {
                recovery_conflict(
                    journal.record(),
                    format!("cannot synchronize staging-root cleanup: {error}"),
                )
            })?;
            install_faults
                .after(InstallOperation::SyncParentDirectory)
                .map_err(|error| {
                    recovery_conflict(
                        journal.record(),
                        format!("staging-root parent sync completed but reported failure: {error}"),
                    )
                })?;
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && journal.record().has_cleanup_intent(stage_intent) => {}
        Err(error) => {
            return Err(recovery_conflict(
                journal.record(),
                format!("cannot inspect staging directory before cleanup: {error}"),
            ));
        }
    }
    Ok(())
}

fn open_relative_parent(root: &SecureDir, path: &str) -> io::Result<Option<(SecureDir, String)>> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = parts
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty relative path"))?
        .to_owned();
    let mut current = root.clone();
    for part in parts {
        match current.open_directory(std::ffi::OsStr::new(part)) {
            Ok(directory) => current = directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(Some((current, name)))
}

fn receipt_file_matches(file: &mut fs::File, expected: &ReceiptFile) -> io::Result<bool> {
    let expected_bytes = expected.byte_count();
    if file.metadata()?.len() != expected_bytes {
        return Ok(false);
    }
    let mut digest = Context::new(&SHA256);
    let mut count = 0_u64;
    let mut buffer = [0; IO_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .filter(|value| *value <= expected_bytes)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "file changed while hashing")
            })?;
        digest.update(&buffer[..read]);
    }
    let mut hash = String::from("sha256:");
    for byte in digest.finish().as_ref() {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    Ok(count == expected_bytes && hash == expected.sha256())
}

fn verify_receipt_inventory_at(
    receipt: &InstallationReceipt,
    root: SecureDir,
) -> Result<ReceiptContentState, RecoveryError> {
    let expected_files = receipt
        .files()
        .iter()
        .map(|file| {
            (
                file.path().to_owned(),
                (file.byte_count(), file.sha256().to_owned()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_dirs = receipt
        .directories()
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut actual_files = BTreeMap::new();
    let mut actual_dirs = BTreeSet::new();
    let mut stack = vec![(root, String::new(), 0_usize)];
    let mut total = 0_u64;
    while let Some((directory, prefix, depth)) = stack.pop() {
        if depth > 32 {
            return Ok(ReceiptContentState::Modified);
        }
        let mut entries = directory.entries().map_err(|error| {
            RecoveryError::Store(content_error("read recovery inventory", error))
        })?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, kind) in entries {
            let Some(name) = name.to_str() else {
                return Ok(ReceiptContentState::Modified);
            };
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if !valid_inventory_path(&path) {
                return Ok(ReceiptContentState::Modified);
            }
            match kind {
                NodeKind::Directory => {
                    actual_dirs.insert(path.clone());
                    stack.push((
                        directory
                            .open_directory(std::ffi::OsStr::new(name))
                            .map_err(|error| {
                                RecoveryError::Store(content_error(
                                    "open recovery directory",
                                    error,
                                ))
                            })?,
                        path,
                        depth + 1,
                    ));
                }
                NodeKind::RegularFile => {
                    let mut file = directory.open_regular_file(name).map_err(|error| {
                        RecoveryError::Store(content_error("open recovery file", error))
                    })?;
                    let length = file
                        .metadata()
                        .map_err(|error| {
                            RecoveryError::Store(content_error("inspect recovery file", error))
                        })?
                        .len();
                    if length > crate::sources::archive::MAX_FILE_UNCOMPRESSED_BYTES {
                        return Ok(ReceiptContentState::Modified);
                    }
                    let mut digest = Context::new(&SHA256);
                    let mut count = 0_u64;
                    let mut buffer = [0; IO_CHUNK_BYTES];
                    loop {
                        let read = file.read(&mut buffer).map_err(|error| {
                            RecoveryError::Store(content_error("hash recovery file", error))
                        })?;
                        if read == 0 {
                            break;
                        }
                        count = count
                            .checked_add(read as u64)
                            .filter(|value| *value <= length)
                            .ok_or_else(|| {
                                RecoveryError::Store(content_error(
                                    "bound recovery hash",
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "file changed while hashing",
                                    ),
                                ))
                            })?;
                        digest.update(&buffer[..read]);
                    }
                    let mut hash = String::from("sha256:");
                    for byte in digest.finish().as_ref() {
                        use std::fmt::Write as _;
                        let _ = write!(hash, "{byte:02x}");
                    }
                    total = total.saturating_add(count);
                    actual_files.insert(path, (count, hash));
                }
                NodeKind::Symlink | NodeKind::Other => return Ok(ReceiptContentState::Modified),
            }
            if actual_files.len() + actual_dirs.len() > 4096 {
                return Ok(ReceiptContentState::Modified);
            }
        }
    }
    let expected_total = receipt
        .files()
        .iter()
        .map(ReceiptFile::byte_count)
        .sum::<u64>();
    Ok(
        if total == expected_total && actual_files == expected_files && actual_dirs == expected_dirs
        {
            ReceiptContentState::Verified
        } else {
            ReceiptContentState::Modified
        },
    )
}

fn finalize_recovered_receipt(
    record: &JournalRecord,
    directory: &SecureDir,
    destination_identity: FileIdentity,
    faults: &dyn ReceiptFaultInjector,
) -> Result<(), RecoveryError> {
    recover_temporary_receipt(record, directory, destination_identity, faults)?;
    let receipts = read_receipts(directory, true).map_err(|error| RecoveryError::Conflict {
        operation_id: record.operation_id().to_owned(),
        detail: error.to_string(),
    })?;
    let final_name =
        record
            .receipt()
            .receipt_filename()
            .map_err(|error| RecoveryError::Conflict {
                operation_id: record.operation_id().to_owned(),
                detail: error.to_string(),
            })?;
    if let Some((_, existing)) = receipts.iter().find(|(name, _)| name == &final_name) {
        if existing != record.receipt() {
            return Err(recovery_conflict(
                record,
                "a conflicting receipt already exists",
            ));
        }
        faults
            .before(ReceiptOperation::SyncReceiptDirectory)
            .map_err(|error| {
                recovery_conflict(
                    record,
                    format!("cannot synchronize existing receipt publication: {error}"),
                )
            })?;
        directory.sync_directory().map_err(|error| {
            recovery_conflict(
                record,
                format!("cannot synchronize existing receipt publication: {error}"),
            )
        })?;
        faults
            .after(ReceiptOperation::SyncReceiptDirectory)
            .map_err(|error| {
                recovery_conflict(
                    record,
                    format!("cannot synchronize existing receipt publication: {error}"),
                )
            })?;
        return Ok(());
    }
    validate_new_claim(record.receipt(), &receipts).map_err(|error| RecoveryError::Conflict {
        operation_id: record.operation_id().to_owned(),
        detail: error.to_string(),
    })?;
    publish_recovered_receipt(directory, record.receipt(), faults).map_err(|error| {
        RecoveryError::Conflict {
            operation_id: record.operation_id().to_owned(),
            detail: error.to_string(),
        }
    })
}

fn recover_temporary_receipt(
    record: &JournalRecord,
    directory: &SecureDir,
    destination_identity: FileIdentity,
    faults: &dyn ReceiptFaultInjector,
) -> Result<(), RecoveryError> {
    let pending = directory
        .entries()
        .map_err(|error| RecoveryError::Store(map_io("inspect receipt storage", error)))?
        .into_iter()
        .filter_map(|(name, kind)| {
            let name = name.to_str()?.to_owned();
            name.starts_with(".pending-").then_some((name, kind))
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(());
    }
    if pending.len() != 1 {
        return Err(recovery_conflict(
            record,
            "multiple interrupted receipt temporary files exist",
        ));
    }
    let (name, kind) = &pending[0];
    if *kind != NodeKind::RegularFile {
        return Err(recovery_conflict(
            record,
            "receipt temporary is not a regular file",
        ));
    }
    let mut file = directory
        .open_regular_file_for_rename(name)
        .map_err(|error| {
            recovery_conflict(record, format!("cannot inspect receipt temporary: {error}"))
        })?;
    let len = file
        .metadata()
        .map_err(|error| {
            recovery_conflict(record, format!("cannot inspect receipt temporary: {error}"))
        })?
        .len();
    if len > MAX_RECEIPT_BYTES as u64 {
        return Err(recovery_conflict(
            record,
            "receipt temporary exceeds the size limit",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            recovery_conflict(record, format!("cannot read receipt temporary: {error}"))
        })?;
    let parsed = InstallationReceipt::from_json(&bytes)
        .map_err(|_| recovery_conflict(record, "receipt temporary is corrupt or incomplete"))?;
    if parsed != *record.receipt()
        || verify_receipt_tree(record.receipt(), Some(destination_identity)).map_err(|error| {
            recovery_conflict(
                record,
                format!("cannot verify destination for receipt recovery: {error}"),
            )
        })? != ReceiptContentState::Verified
    {
        return Err(recovery_conflict(
            record,
            "receipt temporary or committed content differs from the operation",
        ));
    }
    let final_name = record
        .receipt()
        .receipt_filename()
        .map_err(|error| recovery_conflict(record, format!("invalid receipt identity: {error}")))?;
    let existing = read_receipts_with_pending(directory, true, true).map_err(|error| {
        recovery_conflict(
            record,
            format!("existing receipt storage is conflicting: {error}"),
        )
    })?;
    validate_new_claim(record.receipt(), &existing).map_err(|error| {
        recovery_conflict(
            record,
            format!("existing receipt conflicts with recovery: {error}"),
        )
    })?;
    faults
        .before(ReceiptOperation::PublishNoReplace)
        .map_err(|error| RecoveryError::Store(map_io("recover receipt publication", error)))?;
    directory
        .rename_file_child_noreplace(name, &final_name, &file)
        .map_err(|error| {
            recovery_conflict(
                record,
                format!("cannot publish recovered receipt without replacement: {error}"),
            )
        })?;
    faults
        .after(ReceiptOperation::PublishNoReplace)
        .map_err(|error| RecoveryError::Store(map_io("recover receipt publication", error)))?;
    faults
        .before(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| RecoveryError::Store(map_io("synchronize recovered receipt", error)))?;
    directory
        .sync_directory()
        .map_err(|error| RecoveryError::Store(map_io("synchronize recovered receipt", error)))?;
    faults
        .after(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| RecoveryError::Store(map_io("synchronize recovered receipt", error)))
}

fn publish_recovered_receipt(
    directory: &SecureDir,
    receipt: &InstallationReceipt,
    faults: &dyn ReceiptFaultInjector,
) -> Result<(), ReceiptStoreError> {
    let bytes = receipt
        .to_json()
        .map_err(|source| validation_error("recovered receipt", source))?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err(validation_error(
            "recovered receipt",
            ReceiptValidationError::InputTooLarge {
                maximum: MAX_RECEIPT_BYTES,
            },
        ));
    }
    let final_name = receipt
        .receipt_filename()
        .map_err(|source| validation_error("recovered receipt", source))?;
    let mut temporary_name = None;
    let mut temporary_file = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = random_temp_name().map_err(|error| map_io("name recovered receipt", error))?;
        faults
            .before(ReceiptOperation::CreateTemporaryReceipt)
            .map_err(|error| map_io("create recovered receipt", error))?;
        match directory.create_regular_file(&name) {
            Ok(file) => {
                temporary_name = Some(name);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io("create recovered receipt", error)),
        }
    }
    let name = temporary_name.ok_or(ReceiptStoreError::TemporaryNameExhausted)?;
    let mut file = temporary_file.ok_or(ReceiptStoreError::TemporaryNameExhausted)?;
    let identity = SecureDir::file_identity(&file)
        .map_err(|error| map_io("identify recovered receipt", error))?;
    let mut guard = TempReceipt {
        directory: directory.clone(),
        name: name.clone(),
        identity,
        active: true,
    };
    faults
        .before(ReceiptOperation::WriteTemporaryReceipt)
        .map_err(|error| map_io("write recovered receipt", error))?;
    file.write_all(&bytes)
        .map_err(|error| map_io("write recovered receipt", error))?;
    file.flush()
        .map_err(|error| map_io("flush recovered receipt", error))?;
    faults
        .before(ReceiptOperation::SyncTemporaryReceipt)
        .map_err(|error| map_io("synchronize recovered receipt", error))?;
    file.sync_all()
        .map_err(|error| map_io("synchronize recovered receipt", error))?;
    faults
        .after(ReceiptOperation::SyncTemporaryReceipt)
        .map_err(|error| map_io("synchronize recovered receipt", error))?;
    let reread = read_receipt_file(directory, &name)?;
    if reread != *receipt {
        return Err(ReceiptStoreError::TemporaryReceiptMismatch);
    }
    faults
        .before(ReceiptOperation::PublishNoReplace)
        .map_err(|error| map_io("publish recovered receipt", error))?;
    directory
        .rename_file_child_noreplace(&name, &final_name, &file)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                ReceiptStoreError::DuplicateIdentity
            } else {
                map_io("publish recovered receipt", error)
            }
        })?;
    guard.active = false;
    faults
        .after(ReceiptOperation::PublishNoReplace)
        .map_err(|error| map_io("publish recovered receipt", error))?;
    faults
        .before(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    directory
        .sync_directory()
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })?;
    faults
        .after(ReceiptOperation::SyncReceiptDirectory)
        .map_err(|error| ReceiptStoreError::Durability {
            kind: error.kind(),
            os_error: error.raw_os_error(),
        })
}

fn recovery_conflict(record: &JournalRecord, detail: impl Into<String>) -> RecoveryError {
    RecoveryError::Conflict {
        operation_id: record.operation_id().to_owned(),
        detail: detail.into(),
    }
}

struct TempReceipt {
    directory: SecureDir,
    name: String,
    identity: FileIdentity,
    active: bool,
}

impl Drop for TempReceipt {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(file) = self.directory.open_regular_file(&self.name)
            && SecureDir::file_identity(&file).ok() == Some(self.identity)
        {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReceiptContentState {
    Verified,
    Missing,
    Modified,
}

#[derive(Debug, Error)]
pub enum ReceiptStoreError {
    #[error("no operating-system application-data directory is available")]
    DirectoryUnavailable,
    #[error("THRN_DATA_DIR must be an absolute path")]
    RelativeDirectoryOverride,
    #[error("could not resolve a THRN operating-system path: {0}")]
    PathResolution(String),
    #[error("could not {action} receipt storage ({kind}, os_error={os_error:?})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
    #[error("THRN receipt storage is not a real directory or contains a reparse point")]
    UnsafeDirectory,
    #[error("receipt storage contains an unknown, linked, or nonregular object: {name:?}")]
    UnsafeObject { name: String },
    #[error("receipt storage contains more than {maximum} entries")]
    TooManyReceipts { maximum: usize },
    #[error("receipt filename is not a supported deterministic identity key")]
    InvalidFilename,
    #[error("receipt filename does not match the identity recorded in its contents")]
    FilenameMismatch,
    #[error("receipt file {name:?} is invalid: {source}")]
    Validation {
        name: String,
        #[source]
        source: ReceiptValidationError,
    },
    #[error("receipt storage contains duplicate ownership claims for an installation identity")]
    DuplicateIdentity,
    #[error("receipt storage contains conflicting claims for the same {agent} {scope} destination")]
    DestinationConflict {
        agent: &'static str,
        scope: &'static str,
    },
    #[error(
        "receipt storage has managed records but no persistent writer lock for a coherent read"
    )]
    WriterLockMissing,
    #[error("receipt storage changed during read-only inspection; retry after writers are idle")]
    SnapshotChanged,
    #[error("selected project could not be inspected: {0}")]
    ProjectInspection(String),
    #[error(
        "installation recovery is required before project changes can proceed; {operation_count} journal operation(s) remain"
    )]
    RecoveryRequired { operation_count: usize },
    #[error("installation recovery is required before project changes can proceed: {detail}")]
    Journal { detail: String },
    #[error(
        "receipt storage has an existing writer lock; it may be busy or left by an interrupted write"
    )]
    BusyOrInterrupted,
    #[error("receipt storage contains an interrupted temporary receipt write")]
    InterruptedReceiptWrite,
    #[error("receipt temporary filename attempts were exhausted")]
    TemporaryNameExhausted,
    #[error("receipt content changed after the internal commit")]
    CommittedContentModified,
    #[error("receipt destination is missing after the internal commit")]
    CommittedContentMissing,
    #[error("committed installation identity does not match the prepared receipt")]
    CommittedIdentityMismatch,
    #[error("temporary receipt did not parse back to the complete prepared receipt")]
    TemporaryReceiptMismatch,
    #[error(
        "receipt persistence failed after atomic publication; parent-directory durability could not be confirmed ({kind}, os_error={os_error:?})"
    )]
    Durability {
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
    #[error("receipt was published but the writer lock could not be removed: {cleanup}")]
    CleanupAfterPublication { cleanup: String },
    #[error(
        "receipt write failed ({operation}); lock cleanup also failed ({cleanup}); published={published}"
    )]
    CleanupAfterFailure {
        operation: String,
        cleanup: String,
        published: bool,
    },
    #[error("receipt-store writer lock was replaced during publication")]
    WriterLockChanged,
    #[error("current installed content could not be checked ({action}, {kind})")]
    ContentInspection {
        action: &'static str,
        kind: io::ErrorKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryOutcome {
    CleanedCompletedRecord,
    RolledBack,
    FinalizedReceipt,
}

#[derive(Debug, Error)]
pub(crate) enum RecoveryError {
    #[error("receipt storage is not a safe absolute path")]
    UnsafeStore,
    #[error("receipt-store coordination failed: {0}")]
    Store(#[source] ReceiptStoreError),
    #[error("installation journal is invalid: {0}")]
    Journal(#[source] JournalError),
    #[error("removal recovery is incomplete or conflicted: {0}")]
    Removal(String),
    #[error("update recovery is incomplete or conflicted: {0}")]
    Update(String),
    #[error(
        "installation, removal, and update journals conflict; preserve them for manual recovery"
    )]
    LifecycleConflict,
    #[error(
        "a pending lifecycle operation belongs to a different agent or scope; recover that exact target first"
    )]
    ForeignOperation,
    #[error(
        "operation {operation_id} conflicts with current filesystem or receipt state: {detail}; preserve the content and inspect it manually"
    )]
    Conflict {
        operation_id: String,
        detail: String,
    },
}

fn receipt_matches_scope(
    receipt: &InstallationReceipt,
    scope: &super::scope::ResolvedScope,
) -> bool {
    receipt.agent() == scope.agent().as_str()
        && receipt.scope() == scope.scope().as_str()
        && receipt.root_path() == scope.root()
}

fn same_operation_scope(left: &InstallationReceipt, right: &InstallationReceipt) -> bool {
    left.agent() == right.agent()
        && left.scope() == right.scope()
        && left.root_path() == right.root_path()
}

fn read_receipts(
    directory: &SecureDir,
    allow_writer_lock: bool,
) -> Result<Vec<(String, InstallationReceipt)>, ReceiptStoreError> {
    read_receipts_with_pending(directory, allow_writer_lock, false)
}

fn read_receipts_with_pending(
    directory: &SecureDir,
    allow_writer_lock: bool,
    allow_pending: bool,
) -> Result<Vec<(String, InstallationReceipt)>, ReceiptStoreError> {
    read_receipts_excluding(directory, allow_writer_lock, allow_pending, &[])
}

fn read_receipts_excluding(
    directory: &SecureDir,
    allow_writer_lock: bool,
    allow_pending: bool,
    excluded_artifacts: &[&str],
) -> Result<Vec<(String, InstallationReceipt)>, ReceiptStoreError> {
    let mut entries = directory
        .entries()
        .map_err(|error| map_io("read receipt storage", error))?;
    if entries.len() > MAX_RECEIPTS + usize::from(allow_writer_lock) + usize::from(allow_pending) {
        return Err(ReceiptStoreError::TooManyReceipts {
            maximum: MAX_RECEIPTS,
        });
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut receipts = Vec::new();
    for (name, kind) in entries {
        let name = name
            .to_str()
            .ok_or_else(|| ReceiptStoreError::UnsafeObject {
                name: "non-UTF-8 entry".to_owned(),
            })?
            .to_owned();
        if name == WRITER_LOCK_NAME && allow_writer_lock {
            if kind != NodeKind::RegularFile {
                return Err(ReceiptStoreError::UnsafeObject { name });
            }
            let lock = directory
                .open_regular_file(&name)
                .map_err(|error| map_io("inspect receipt-store lock", error))?;
            if lock
                .metadata()
                .map_err(|error| map_io("inspect receipt-store lock", error))?
                .len()
                != 0
            {
                return Err(ReceiptStoreError::UnsafeObject { name });
            }
            continue;
        }
        if name == WRITER_LOCK_NAME {
            return Err(ReceiptStoreError::BusyOrInterrupted);
        }
        if excluded_artifacts.contains(&name.as_str()) {
            if kind != NodeKind::RegularFile {
                return Err(ReceiptStoreError::UnsafeObject { name });
            }
            continue;
        }
        if name.starts_with(".pending-") {
            if allow_pending {
                continue;
            }
            return Err(ReceiptStoreError::InterruptedReceiptWrite);
        }
        if kind != NodeKind::RegularFile {
            return Err(ReceiptStoreError::UnsafeObject { name });
        }
        if !valid_receipt_filename(&name) {
            return Err(ReceiptStoreError::InvalidFilename);
        }
        let receipt = read_receipt_file(directory, &name)?;
        let expected_name = receipt
            .receipt_filename()
            .map_err(|source| validation_error(&name, source))?;
        if expected_name != name {
            return Err(ReceiptStoreError::FilenameMismatch);
        }
        receipts.push((name, receipt));
    }
    if receipts.len() > MAX_RECEIPTS {
        return Err(ReceiptStoreError::TooManyReceipts {
            maximum: MAX_RECEIPTS,
        });
    }
    validate_unique_destinations(&receipts)?;
    Ok(receipts)
}

pub(super) fn read_update_receipt_claims(
    directory: &SecureDir,
    excluded_artifacts: &[&str],
) -> Result<Vec<(String, InstallationReceipt)>, ReceiptStoreError> {
    read_receipts_excluding(directory, true, false, excluded_artifacts)
}

pub(super) fn read_named_receipt(
    directory: &SecureDir,
    name: &str,
) -> Result<InstallationReceipt, ReceiptStoreError> {
    read_receipt_file(directory, name)
}

pub(super) fn receipt_file_identity(
    directory: &SecureDir,
    name: &str,
) -> Result<FileIdentity, ReceiptStoreError> {
    let file = directory
        .open_regular_file(name)
        .map_err(|error| map_io("open ownership receipt", error))?;
    if SecureDir::hard_link_count(&file)
        .map_err(|error| map_io("inspect ownership receipt links", error))?
        != 1
    {
        return Err(ReceiptStoreError::UnsafeObject {
            name: name.to_owned(),
        });
    }
    SecureDir::file_identity(&file).map_err(|error| map_io("identify ownership receipt", error))
}

pub(super) fn receipt_file_sha256(
    directory: &SecureDir,
    name: &str,
) -> Result<String, ReceiptStoreError> {
    let mut file = directory
        .open_regular_file(name)
        .map_err(|error| map_io("open ownership receipt for digest", error))?;
    let before = SecureDir::file_identity(&file)
        .map_err(|error| map_io("identify ownership receipt for digest", error))?;
    if SecureDir::hard_link_count(&file)
        .map_err(|error| map_io("inspect ownership receipt links", error))?
        != 1
    {
        return Err(ReceiptStoreError::UnsafeObject {
            name: name.to_owned(),
        });
    }
    let length = file
        .metadata()
        .map_err(|error| map_io("inspect ownership receipt for digest", error))?
        .len();
    if length > MAX_RECEIPT_BYTES as u64 {
        return Err(validation_error(
            name,
            ReceiptValidationError::InputTooLarge {
                maximum: MAX_RECEIPT_BYTES,
            },
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| map_io("read ownership receipt for digest", error))?;
    if bytes.len() as u64 != length
        || SecureDir::file_identity(&file)
            .map_err(|error| map_io("reidentify ownership receipt for digest", error))?
            != before
    {
        return Err(ReceiptStoreError::UnsafeObject {
            name: name.to_owned(),
        });
    }
    let digest = ring::digest::digest(&SHA256, &bytes);
    let mut value = String::from("sha256:");
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    Ok(value)
}

/// An unlocked persistent lock file is a stale reservation: OS locks, rather than age or PID,
/// decide whether a writer is still active. Listing only probes the lock and never removes it.
fn check_idle_writer_lock(directory: &SecureDir) -> Result<bool, ReceiptStoreError> {
    let entries = directory
        .entries()
        .map_err(|error| map_io("inspect receipt storage", error))?;
    let Some((_, kind)) = entries
        .iter()
        .find(|(name, _)| name.to_str() == Some(WRITER_LOCK_NAME))
    else {
        return Ok(false);
    };
    if *kind != NodeKind::RegularFile {
        return Err(ReceiptStoreError::UnsafeObject {
            name: WRITER_LOCK_NAME.to_owned(),
        });
    }
    let file = directory
        .open_lock_file(WRITER_LOCK_NAME)
        .map_err(|error| map_io("inspect receipt-store lock", error))?;
    if file
        .metadata()
        .map_err(|error| map_io("inspect receipt-store lock", error))?
        .len()
        != 0
    {
        return Err(ReceiptStoreError::UnsafeObject {
            name: WRITER_LOCK_NAME.to_owned(),
        });
    }
    match file.try_lock() {
        Ok(()) => {
            file.unlock()
                .map_err(|error| map_io("release receipt-store read probe", error))?;
            Ok(true)
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(ReceiptStoreError::BusyOrInterrupted),
        Err(std::fs::TryLockError::Error(error)) => Err(map_io("probe receipt-store lock", error)),
    }
}

fn read_receipt_file(
    directory: &SecureDir,
    name: &str,
) -> Result<InstallationReceipt, ReceiptStoreError> {
    let file = directory
        .open_regular_file(name)
        .map_err(|error| map_io("open receipt", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| map_io("inspect receipt", error))?;
    if !metadata.is_file() {
        return Err(ReceiptStoreError::UnsafeObject {
            name: name.to_owned(),
        });
    }
    if metadata.len() > MAX_RECEIPT_BYTES as u64 {
        return Err(validation_error(
            name,
            ReceiptValidationError::InputTooLarge {
                maximum: MAX_RECEIPT_BYTES,
            },
        ));
    }
    let mut bytes = Vec::new();
    file.take((MAX_RECEIPT_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| map_io("read receipt", error))?;
    InstallationReceipt::from_json(&bytes).map_err(|source| validation_error(name, source))
}

fn validate_new_claim(
    new_receipt: &InstallationReceipt,
    existing: &[(String, InstallationReceipt)],
) -> Result<(), ReceiptStoreError> {
    let new_name = new_receipt
        .receipt_filename()
        .map_err(|source| validation_error("new receipt", source))?;
    for (name, receipt) in existing {
        if *name == new_name {
            return Err(ReceiptStoreError::DuplicateIdentity);
        }
        if new_receipt.same_destination(receipt) {
            return Err(destination_conflict(new_receipt));
        }
    }
    Ok(())
}

fn validate_unique_destinations(
    receipts: &[(String, InstallationReceipt)],
) -> Result<(), ReceiptStoreError> {
    for (index, (_, receipt)) in receipts.iter().enumerate() {
        if receipts[index + 1..]
            .iter()
            .any(|(_, other)| receipt.same_destination(other))
        {
            return Err(destination_conflict(receipt));
        }
    }
    Ok(())
}

fn destination_conflict(receipt: &InstallationReceipt) -> ReceiptStoreError {
    ReceiptStoreError::DestinationConflict {
        agent: match receipt.agent() {
            "codex" => "Codex",
            "claude-code" => "Claude Code",
            "cursor" => "Cursor",
            "gemini" => "Gemini CLI",
            _ => "unknown agent",
        },
        scope: match receipt.scope() {
            "project" => "project",
            _ => "user",
        },
    }
}

fn valid_receipt_filename(name: &str) -> bool {
    let hex = name
        .strip_prefix("v1-")
        .or_else(|| name.strip_prefix("v2-"))
        .or_else(|| name.strip_prefix("v4-"))
        .or_else(|| name.strip_prefix("v5-"))
        .and_then(|name| name.strip_suffix(".json"));
    let Some(hex) = hex else { return false };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verify_receipt_tree(
    receipt: &InstallationReceipt,
    expected_destination_identity: Option<FileIdentity>,
) -> Result<ReceiptContentState, ReceiptStoreError> {
    let project = match SecureDir::open_absolute(&receipt.root_path()) {
        Ok(project) => project,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ReceiptContentState::Missing);
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(ReceiptContentState::Modified);
        }
        Err(error) => return Err(content_error("open project root", error)),
    };
    if project
        .identity()
        .map_err(|error| content_error("identify project root", error))?
        != receipt.scope_root_identity()
    {
        return Ok(ReceiptContentState::Modified);
    }
    let Some(agents) = open_directory_status(&project, receipt.agent_directory())? else {
        return Ok(ReceiptContentState::Missing);
    };
    let Some(skills) = open_directory_status(&agents, "skills")? else {
        return Ok(ReceiptContentState::Missing);
    };
    let Some(root) = open_directory_status(&skills, receipt.skill_name())? else {
        return Ok(ReceiptContentState::Missing);
    };
    let current_destination_identity = root
        .identity()
        .map_err(|error| content_error("identify installed directory", error))?;
    if receipt
        .installed_directory_identity()
        .is_some_and(|identity| identity != current_destination_identity)
        || expected_destination_identity
            .is_some_and(|identity| current_destination_identity != identity)
    {
        return Ok(ReceiptContentState::Modified);
    }

    let expected_files = receipt
        .files()
        .iter()
        .map(|file| {
            (
                file.path().to_owned(),
                (file.byte_count(), file.sha256().to_owned()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_directories = receipt
        .directories()
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut actual_files = BTreeMap::new();
    let mut actual_directories = BTreeSet::new();
    let mut stack = vec![(root, String::new(), 0_usize)];
    let mut total_bytes = 0_u64;
    while let Some((directory, prefix, depth)) = stack.pop() {
        if depth > 32 {
            return Ok(ReceiptContentState::Modified);
        }
        let mut entries = directory
            .entries()
            .map_err(|error| content_error("read installed directory", error))?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, kind) in entries {
            let Some(name) = name.to_str() else {
                return Ok(ReceiptContentState::Modified);
            };
            let relative = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if !valid_inventory_path(&relative) {
                return Ok(ReceiptContentState::Modified);
            }
            match kind {
                NodeKind::Directory => {
                    actual_directories.insert(relative.clone());
                    let child = directory
                        .open_directory(std::ffi::OsStr::new(name))
                        .map_err(|error| content_error("open installed subdirectory", error))?;
                    stack.push((child, relative, depth + 1));
                }
                NodeKind::RegularFile => {
                    let mut file = directory
                        .open_regular_file(name)
                        .map_err(|error| content_error("open installed file", error))?;
                    let expected_len = file
                        .metadata()
                        .map_err(|error| content_error("inspect installed file", error))?
                        .len();
                    if expected_len > crate::sources::archive::MAX_FILE_UNCOMPRESSED_BYTES {
                        return Ok(ReceiptContentState::Modified);
                    }
                    let mut digest = Context::new(&SHA256);
                    let mut count = 0_u64;
                    let mut buffer = [0; IO_CHUNK_BYTES];
                    loop {
                        let read = file
                            .read(&mut buffer)
                            .map_err(|error| content_error("read installed file", error))?;
                        if read == 0 {
                            break;
                        }
                        count = count
                            .checked_add(read as u64)
                            .filter(|bytes| *bytes <= expected_len)
                            .ok_or_else(|| {
                                content_error(
                                    "bound installed file read",
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "file changed while being checked",
                                    ),
                                )
                            })?;
                        digest.update(&buffer[..read]);
                    }
                    if count != expected_len {
                        return Ok(ReceiptContentState::Modified);
                    }
                    let mut hash = String::from("sha256:");
                    for byte in digest.finish().as_ref() {
                        use std::fmt::Write as _;
                        let _ = write!(hash, "{byte:02x}");
                    }
                    total_bytes = total_bytes.saturating_add(count);
                    actual_files.insert(relative, (count, hash));
                }
                NodeKind::Symlink | NodeKind::Other => {
                    return Ok(ReceiptContentState::Modified);
                }
            }
            if actual_files.len() + actual_directories.len() > 4_096 {
                return Ok(ReceiptContentState::Modified);
            }
        }
    }
    if total_bytes
        == receipt
            .files()
            .iter()
            .map(ReceiptFile::byte_count)
            .sum::<u64>()
        && actual_files == expected_files
        && actual_directories == expected_directories
    {
        Ok(ReceiptContentState::Verified)
    } else {
        Ok(ReceiptContentState::Modified)
    }
}

fn open_directory_status(
    parent: &SecureDir,
    name: &str,
) -> Result<Option<SecureDir>, ReceiptStoreError> {
    match parent.open_directory(std::ffi::OsStr::new(name)) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(content_error("open installed directory", error)),
    }
}

fn valid_inventory_path(path: &str) -> bool {
    crate::sources::archive::ValidatedArchivePath::parse(
        path,
        &crate::sources::archive::ArchiveLimits::DEFAULT,
    )
    .is_ok()
}

fn random_temp_name() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("operating-system random source unavailable"))?;
    let mut name = String::from(".pending-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(name)
}

fn map_directory_error(action: &'static str, error: io::Error) -> ReceiptStoreError {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotADirectory
    ) {
        ReceiptStoreError::UnsafeDirectory
    } else {
        map_io(action, error)
    }
}

fn map_io(action: &'static str, error: io::Error) -> ReceiptStoreError {
    ReceiptStoreError::Io {
        action,
        kind: error.kind(),
        os_error: error.raw_os_error(),
    }
}

fn content_error(action: &'static str, error: io::Error) -> ReceiptStoreError {
    ReceiptStoreError::ContentInspection {
        action,
        kind: error.kind(),
    }
}

fn validation_error(name: &str, source: ReceiptValidationError) -> ReceiptStoreError {
    ReceiptStoreError::Validation {
        name: name.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs, io,
        path::{Path, PathBuf},
        process::Command,
        sync::{Mutex, mpsc},
    };

    use std::io::{Cursor, Write};
    use tempfile::tempdir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        install::{
            command::{prepare_install_session, prepare_install_session_in_scope},
            commit::{InstallFaultInjector, InstallOperation, InstallationSession},
            destination::ComponentState,
            journal::{
                JournalError, JournalFaultInjector, JournalIntent, JournalOperation, JournalStore,
                MAX_JOURNAL_BYTES, OperationState,
            },
            plan::{InstallAgent, InstallScope},
            receipt::InstallationReceipt,
            scope::ResolvedScope,
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport},
            staging::TempDirFactory,
        },
    };

    use super::{
        ReceiptContentState, ReceiptFaultInjector, ReceiptOperation, ReceiptStore,
        ReceiptStoreError, resolve_data_directory,
    };

    struct FakeTransport(Vec<u8>);
    impl GithubTransport for FakeTransport {
        fn get(&self, url: &str) -> Result<GithubResponse, crate::sources::github::TransportError> {
            assert!(url.starts_with("https://github.com/Example-Org/catalog-test-fixtures/archive/2f68bee292a8c68346b4294fb31d9563eefe1ade.zip"));
            Ok(GithubResponse::new(
                200,
                Some("application/zip"),
                Some(self.0.len() as u64),
                None,
                Cursor::new(self.0.clone()),
            ))
        }
    }

    struct TestFactory(PathBuf);
    impl TempDirFactory for TestFactory {
        fn create_private(&self) -> io::Result<tempfile::TempDir> {
            tempfile::Builder::new()
                .prefix("m205-stage-")
                .tempdir_in(&self.0)
        }
    }

    fn archive() -> Vec<u8> {
        archive_with_guide(b"guide")
    }

    fn archive_with_guide(guide_contents: &[u8]) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
        let skill = format!("{root}skills/uncharted-ideation/");
        let mut zip = ZipWriter::new(&mut bytes);
        zip.add_directory(root, options).expect("root");
        zip.add_directory(format!("{root}skills/"), options)
            .expect("skills");
        zip.add_directory(&skill, options).expect("skill");
        zip.start_file(format!("{skill}SKILL.md"), options)
            .expect("skill metadata file");
        let mut metadata =
            b"---\nname: uncharted-ideation\ndescription: Receipt test skill.\n---\n".to_vec();
        if guide_contents.len() > 8 * 1024 {
            metadata.extend_from_slice(&[b'x'; 24 * 1024]);
        }
        zip.write_all(&metadata).expect("metadata");
        zip.start_file(format!("{skill}guide.md"), options)
            .expect("guide file");
        zip.write_all(guide_contents).expect("guide content");
        zip.finish().expect("archive");
        bytes.into_inner()
    }

    fn session(project: &Path, stage_parent: &Path) -> InstallationSession {
        session_with_archive(project, stage_parent, archive())
    }

    fn session_for_agent(
        project: &Path,
        stage_parent: &Path,
        agent: InstallAgent,
    ) -> InstallationSession {
        if agent == InstallAgent::Codex {
            return session(project, stage_parent);
        }
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog");
        let scope = ResolvedScope::from_agent_validated(
            agent,
            InstallScope::Project,
            project.to_path_buf(),
        );
        prepare_install_session_in_scope(
            &catalog,
            "uncharted",
            agent.as_str(),
            &scope,
            &FakeTransport(archive()),
            &TestFactory(stage_parent.to_path_buf()),
        )
        .expect("validated agent session")
    }

    fn session_with_archive(
        project: &Path,
        stage_parent: &Path,
        archive: Vec<u8>,
    ) -> InstallationSession {
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog");
        prepare_install_session(
            &catalog,
            "uncharted",
            "codex",
            None,
            project,
            &FakeTransport(archive),
            &TestFactory(stage_parent.to_path_buf()),
        )
        .expect("validated session")
    }

    fn prepared_journal_intent(
        session: &InstallationSession,
        installed_at_unix_seconds: u64,
    ) -> JournalIntent {
        let snapshot = session.plan().destination_snapshot();
        let project_identity = snapshot.project_identity().expect("project identity");
        JournalIntent {
            receipt: InstallationReceipt::from_committed_plan(
                session.plan(),
                session.inventory(),
                project_identity,
                installed_at_unix_seconds,
            )
            .expect("prepared receipt"),
            staging_name: crate::install::journal::new_staging_name().expect("staging name"),
            create_agents_directory: snapshot.agents_state() == ComponentState::Missing,
            create_skills_directory: snapshot.skills_state() == ComponentState::Missing,
            agents_identity: snapshot.agents_identity(),
            skills_identity: snapshot.skills_identity(),
        }
    }

    fn copy_directory_tree(source: &Path, destination: &Path) {
        fs::create_dir(destination).expect("create replacement directory");
        for entry in fs::read_dir(source).expect("read source directory") {
            let entry = entry.expect("directory entry");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let kind = entry.file_type().expect("entry type");
            if kind.is_dir() {
                copy_directory_tree(&source_path, &destination_path);
            } else {
                assert!(kind.is_file(), "fixture contains only regular files");
                fs::copy(source_path, destination_path).expect("copy replacement file");
            }
        }
    }

    #[derive(Default)]
    struct NoInstallFaults;
    impl InstallFaultInjector for NoInstallFaults {}

    #[derive(Default)]
    struct NoReceiptFaults;
    impl ReceiptFaultInjector for NoReceiptFaults {}

    #[derive(Default)]
    struct NoJournalFaultsForTest;
    impl JournalFaultInjector for NoJournalFaultsForTest {}

    struct FailReceiptAt {
        operation: ReceiptOperation,
        occurrence: usize,
        seen: Mutex<HashMap<ReceiptOperation, usize>>,
    }

    struct PauseBeforeWriterLock {
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl ReceiptFaultInjector for PauseBeforeWriterLock {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation == ReceiptOperation::CreateWriterLock {
                self.entered
                    .send(())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                self.resume
                    .lock()
                    .map_err(|_| io::Error::other("writer-lock barrier poisoned"))?
                    .recv_timeout(std::time::Duration::from_secs(15))
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            Ok(())
        }
    }

    struct PauseAfterPreparedJournal {
        journal_directory: PathBuf,
        prepared: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl JournalFaultInjector for PauseAfterPreparedJournal {
        fn after(&self, operation: JournalOperation) -> io::Result<()> {
            if operation != JournalOperation::SyncStore {
                return Ok(());
            }
            let records = JournalStore::new(self.journal_directory.clone())
                .read_all()
                .map_err(|error| io::Error::other(error.to_string()))?;
            if records
                .iter()
                .any(|record| record.state() == OperationState::Prepared)
            {
                self.prepared
                    .send(())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                self.resume
                    .lock()
                    .map_err(|_| io::Error::other("prepared-journal barrier poisoned"))?
                    .recv_timeout(std::time::Duration::from_secs(15))
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            Ok(())
        }
    }

    struct IntroduceJournalBeforeReservation {
        seen_writer_locks: Mutex<usize>,
        journal_store: JournalStore,
        intent: Mutex<Option<JournalIntent>>,
    }

    impl ReceiptFaultInjector for IntroduceJournalBeforeReservation {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation != ReceiptOperation::CreateWriterLock {
                return Ok(());
            }
            let mut seen = self
                .seen_writer_locks
                .lock()
                .map_err(|_| io::Error::other("writer-lock counter poisoned"))?;
            *seen += 1;
            if *seen == 2 {
                let intent = self
                    .intent
                    .lock()
                    .map_err(|_| io::Error::other("journal-intent lock poisoned"))?
                    .take()
                    .ok_or_else(|| io::Error::other("journal intent already used"))?;
                self.journal_store
                    .prepare(intent, &NoJournalFaultsForTest)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            Ok(())
        }
    }

    impl ReceiptFaultInjector for FailReceiptAt {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            let mut seen = self.seen.lock().map_err(|_| io::Error::other("counter"))?;
            let count = seen.entry(operation).or_default();
            *count += 1;
            if operation == self.operation && *count == self.occurrence {
                Err(io::Error::other("injected receipt failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FailInstallAt(InstallOperation);
    impl InstallFaultInjector for FailInstallAt {
        fn before(&self, operation: InstallOperation) -> io::Result<()> {
            if operation == self.0 {
                Err(io::Error::other("injected commit failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FailAfterInstallAt(InstallOperation);
    impl InstallFaultInjector for FailAfterInstallAt {
        fn after(&self, operation: InstallOperation) -> io::Result<()> {
            if operation == self.0 {
                Err(io::Error::other("injected post-filesystem failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FailJournalAt(JournalOperation);
    impl JournalFaultInjector for FailJournalAt {
        fn before(&self, operation: JournalOperation) -> io::Result<()> {
            if operation == self.0 {
                Err(io::Error::other("injected journal failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FailAfterJournalCleanupSync {
        cleanup_started: Mutex<bool>,
    }

    impl JournalFaultInjector for FailAfterJournalCleanupSync {
        fn after(&self, operation: JournalOperation) -> io::Result<()> {
            let mut cleanup_started = self
                .cleanup_started
                .lock()
                .map_err(|_| io::Error::other("journal cleanup test lock"))?;
            if operation == JournalOperation::CleanupRecord {
                *cleanup_started = true;
            } else if operation == JournalOperation::SyncStore && *cleanup_started {
                *cleanup_started = false;
                return Err(io::Error::other("injected journal cleanup sync failure"));
            }
            Ok(())
        }
    }

    struct ExitAtInstall(InstallOperation);
    impl InstallFaultInjector for ExitAtInstall {
        fn before(&self, operation: InstallOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(86);
            }
            Ok(())
        }
    }

    struct ExitAfterInstall(InstallOperation);
    impl InstallFaultInjector for ExitAfterInstall {
        fn after(&self, operation: InstallOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(87);
            }
            Ok(())
        }
    }

    struct ExitAfterJournal(JournalOperation);
    impl JournalFaultInjector for ExitAfterJournal {
        fn after(&self, operation: JournalOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(88);
            }
            Ok(())
        }
    }

    struct ExitAfterJournalState {
        directory: PathBuf,
        state: String,
    }

    impl JournalFaultInjector for ExitAfterJournalState {
        fn after(&self, operation: JournalOperation) -> io::Result<()> {
            if operation != JournalOperation::SyncStore {
                return Ok(());
            }
            let mut records = match fs::read_dir(&self.directory) {
                Ok(entries) => entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "json")
                    })
                    .collect::<Vec<_>>(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            records.sort();
            if let Some(path) = records.last() {
                let bytes = fs::read(path)?;
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                if value.get("state").and_then(serde_json::Value::as_str)
                    == Some(self.state.as_str())
                {
                    std::process::exit(90);
                }
            }
            Ok(())
        }
    }

    struct ExitAfterReceipt(ReceiptOperation);
    impl ReceiptFaultInjector for ExitAfterReceipt {
        fn after(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(89);
            }
            Ok(())
        }
    }

    struct ExitAtReceipt(ReceiptOperation);
    impl ReceiptFaultInjector for ExitAtReceipt {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(86);
            }
            Ok(())
        }
    }

    struct RecoverWhileWriting {
        receipt_directory: PathBuf,
        attempted: Mutex<bool>,
    }

    impl ReceiptFaultInjector for RecoverWhileWriting {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation == ReceiptOperation::CreateTemporaryReceipt {
                let store = ReceiptStore::new(self.receipt_directory.clone());
                let result = store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults);
                if !matches!(
                    result,
                    Err(super::RecoveryError::Store(
                        ReceiptStoreError::BusyOrInterrupted
                    ))
                ) {
                    return Err(io::Error::other("concurrent recovery was not excluded"));
                }
                *self
                    .attempted
                    .lock()
                    .map_err(|_| io::Error::other("recovery probe lock"))? = true;
            }
            Ok(())
        }
    }

    struct CreateReceiptBeforePublish {
        directory: PathBuf,
        wrote: Mutex<bool>,
    }

    impl ReceiptFaultInjector for CreateReceiptBeforePublish {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation != ReceiptOperation::PublishNoReplace {
                return Ok(());
            }
            let mut wrote = self
                .wrote
                .lock()
                .map_err(|_| io::Error::other("publication hook lock"))?;
            if *wrote {
                return Ok(());
            }
            let temporary = fs::read_dir(&self.directory)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".pending-"))
                })
                .ok_or_else(|| io::Error::other("temporary receipt was not present"))?;
            let bytes = fs::read(&temporary)?;
            let receipt = InstallationReceipt::from_json(&bytes)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let final_path = self.directory.join(
                receipt
                    .receipt_filename()
                    .map_err(|error| io::Error::other(error.to_string()))?,
            );
            fs::write(final_path, b"pre-existing bytes")?;
            *wrote = true;
            Ok(())
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let parent = tempdir().expect("parent");
        // macOS commonly exposes the temp directory through `/var`, a symlink to
        // `/private/var`. The secure dir adapter intentionally rejects symlinked
        // ancestors, so exercise it with the canonical temp root.
        let parent_path = parent.path().canonicalize().expect("canonical parent");
        let project = parent_path.join("project");
        let staging = parent_path.join("staging");
        let data = parent_path.join("data");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        (parent, project, staging, data)
    }

    #[test]
    fn missing_and_empty_storage_list_read_only() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(store.list().expect("missing is empty").is_empty());
        assert!(!data.exists());
        fs::create_dir_all(data.join("receipts")).expect("empty store");
        assert!(store.list().expect("empty is empty").is_empty());
        assert_eq!(
            fs::read_dir(data.join("receipts")).expect("read").count(),
            0
        );
        let _ = session(&project, &staging);
    }

    #[test]
    fn configured_data_root_resolves_existing_ancestor_aliases_without_creating_paths() {
        let parent = tempdir().expect("parent");
        let unresolved = parent.path().join("not-created/data");
        let resolved = resolve_data_directory(&unresolved).expect("resolve data root");
        assert_eq!(
            resolved,
            parent
                .path()
                .canonicalize()
                .expect("canonical parent")
                .join("not-created/data")
        );
        assert!(!parent.path().join("not-created").exists());
    }

    #[cfg(unix)]
    #[test]
    fn configured_data_root_itself_cannot_be_a_symlink() {
        use std::os::unix::fs::symlink;
        let parent = tempdir().expect("parent");
        let real = parent.path().join("real-data");
        let link = parent.path().join("linked-data");
        fs::create_dir(&real).expect("real data root");
        symlink(&real, &link).expect("data root link");
        assert!(matches!(
            resolve_data_directory(&link),
            Err(ReceiptStoreError::UnsafeDirectory)
        ));
    }

    #[test]
    fn commit_then_receipt_publishes_complete_pinned_install_record() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let session = session(&project, &staging);
        let receipt = session
            .commit_and_record_at(&store, 1_800_000_000, &NoInstallFaults, &NoReceiptFaults)
            .expect("commit and receipt");
        assert!(
            !receipt.is_legacy(),
            "new installations use receipt schema v2"
        );
        assert_eq!(
            receipt.installed_directory_identity(),
            Some(
                super::SecureDir::open_absolute(&receipt.destination_path())
                    .expect("installed destination handle")
                    .identity()
                    .expect("committed destination identity")
            )
        );
        assert_eq!(receipt.catalog_resource_id(), "uncharted");
        assert_eq!(receipt.skill_name(), "uncharted-ideation");
        assert_eq!(
            receipt.source_commit(),
            "2f68bee292a8c68346b4294fb31d9563eefe1ade"
        );
        assert_eq!(store.list().expect("list").len(), 1);
        assert_eq!(
            store.verify_current_content(&receipt).expect("verify"),
            ReceiptContentState::Verified
        );
    }

    #[test]
    fn identity_recording_failure_keeps_install_unmanaged_until_recovery_proves_it() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at_with_journal(
            &store,
            1,
            &NoInstallFaults,
            &NoReceiptFaults,
            &FailJournalAt(JournalOperation::SetDestinationIdentity),
        );
        assert!(matches!(
            result,
            Err(
                crate::install::commit::InstallationCompletionError::JournalAfterDirectoryCommit(_)
            )
        ));
        let destination = project.join(".agents/skills/uncharted-ideation");
        assert!(destination.join("SKILL.md").is_file());
        assert!(
            store
                .list()
                .expect("legacy receipt is not published")
                .is_empty()
        );
        let pending = store
            .journal_store()
            .expect("journal store")
            .read_all()
            .expect("journal");
        assert_eq!(pending.len(), 1);
        assert!(pending[0].receipt().is_legacy());
        assert_eq!(pending[0].destination_identity(), None);

        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recovery captures the committed installation identity"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        let receipts = store.list().expect("recovered schema-v2 receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].installed_directory_identity(),
            Some(
                super::SecureDir::open_absolute(&destination)
                    .expect("destination handle")
                    .identity()
                    .expect("destination identity")
            )
        );
    }

    #[test]
    fn subprocess_crash_after_identity_recording_recovers_a_v2_receipt() {
        if std::env::var_os("THRN_M2_OWNERSHIP_CRASH_AFTER_IDENTITY").is_some() {
            let project =
                PathBuf::from(std::env::var_os("THRN_M2_OWNERSHIP_PROJECT").expect("project"));
            let staging =
                PathBuf::from(std::env::var_os("THRN_M2_OWNERSHIP_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_OWNERSHIP_DATA").expect("data"));
            let _ = session(&project, &staging).commit_and_record_at_with_journal(
                &ReceiptStore::new(data.join("receipts")),
                1,
                &NoInstallFaults,
                &NoReceiptFaults,
                &ExitAfterJournal(JournalOperation::SetDestinationIdentity),
            );
            panic!("child should exit immediately after durable identity recording");
        }

        let (_parent, project, staging, data) = setup();
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_after_identity_recording_recovers_a_v2_receipt")
            .arg("--nocapture")
            .env("THRN_M2_OWNERSHIP_CRASH_AFTER_IDENTITY", "1")
            .env("THRN_M2_OWNERSHIP_PROJECT", &project)
            .env("THRN_M2_OWNERSHIP_STAGING", &staging)
            .env("THRN_M2_OWNERSHIP_DATA", &data)
            .status()
            .expect("launch identity-recording crash simulation");
        assert_eq!(status.code(), Some(88));

        let store = ReceiptStore::new(data.join("receipts"));
        assert!(store.list().expect("no premature publication").is_empty());
        let journals = store
            .journal_store()
            .expect("journal store")
            .read_all()
            .expect("journal");
        assert_eq!(journals.len(), 1);
        assert!(!journals[0].receipt().is_legacy());
        assert_eq!(
            journals[0].destination_identity(),
            journals[0].receipt().installed_directory_identity()
        );
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover interrupted publication"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        let receipts = store.list().expect("completed receipt");
        assert_eq!(receipts.len(), 1);
        assert!(!receipts[0].is_legacy());
    }

    #[test]
    fn recovery_reads_journal_only_after_the_active_install_releases_its_lock() {
        let (_parent, project, staging, data) = setup();
        let receipt_directory = data.join("receipts");
        let store = ReceiptStore::new(receipt_directory.clone());
        let install_session = session(&project, &staging);

        let (prepared_tx, prepared_rx) = mpsc::channel();
        let (resume_install_tx, resume_install_rx) = mpsc::channel();
        let journal_faults = std::sync::Arc::new(PauseAfterPreparedJournal {
            journal_directory: data.join("operations"),
            prepared: prepared_tx,
            resume: Mutex::new(resume_install_rx),
        });
        let install_faults_for_thread = journal_faults.clone();
        let install_receipt_directory = receipt_directory.clone();
        let installer = std::thread::spawn(move || {
            install_session.commit_and_record_at_with_journal(
                &ReceiptStore::new(install_receipt_directory),
                1,
                &NoInstallFaults,
                &NoReceiptFaults,
                install_faults_for_thread.as_ref(),
            )
        });
        prepared_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("installer paused with durable Prepared journal while holding the lock");

        let (recovery_entered_tx, recovery_entered_rx) = mpsc::channel();
        let (resume_recovery_tx, resume_recovery_rx) = mpsc::channel();
        let receipt_faults = std::sync::Arc::new(PauseBeforeWriterLock {
            entered: recovery_entered_tx,
            resume: Mutex::new(resume_recovery_rx),
        });
        let receipt_faults_for_thread = receipt_faults.clone();
        let recovery_receipt_directory = receipt_directory.clone();
        let recovery = std::thread::spawn(move || {
            ReceiptStore::new(recovery_receipt_directory)
                .recover_with_faults(&NoJournalFaultsForTest, receipt_faults_for_thread.as_ref())
        });
        recovery_entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("recovery reached the lock boundary before reading snapshots");

        resume_install_tx.send(()).expect("resume installation");
        let installed_receipt = installer
            .join()
            .expect("installer thread")
            .expect("installation completes and cleans its journal");
        let installed_skill = project.join(".agents/skills/uncharted-ideation");
        let installed_content = fs::read(installed_skill.join("SKILL.md")).expect("installed file");
        assert_eq!(
            store.list().expect("valid receipt remains"),
            vec![installed_receipt.clone()]
        );

        resume_recovery_tx.send(()).expect("resume recovery");
        assert!(
            recovery
                .join()
                .expect("recovery thread")
                .expect("recovery after install")
                .is_empty()
        );

        assert_eq!(
            store.list().expect("receipt remains unchanged"),
            vec![installed_receipt.clone()]
        );
        assert!(matches!(
            store.verify_current_content(&installed_receipt),
            Ok(ReceiptContentState::Verified)
        ));
        assert_eq!(
            fs::read(installed_skill.join("SKILL.md")).expect("installed file remains"),
            installed_content
        );
    }

    #[test]
    fn reservation_rejects_a_journal_operation_introduced_after_initial_recovery() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let install_session = session(&project, &staging);
        let fault = IntroduceJournalBeforeReservation {
            seen_writer_locks: Mutex::new(0),
            journal_store: JournalStore::new(data.join("operations")),
            intent: Mutex::new(Some(prepared_journal_intent(&install_session, 1))),
        };

        let error = install_session
            .commit_and_record_at(&store, 1, &NoInstallFaults, &fault)
            .expect_err("unresolved journal must block project mutation");
        assert!(matches!(
            error,
            crate::install::commit::InstallationCompletionError::ReceiptPreflight(
                ReceiptStoreError::RecoveryRequired { operation_count: 1 }
            )
        ));
        assert!(error.to_string().contains("recovery is required"));
        assert!(!project.join(".agents").exists());
        assert!(store.list().expect("no receipt published").is_empty());
        assert_eq!(
            store
                .journal_store()
                .expect("journal store")
                .read_all()
                .expect("unresolved journal retained")[0]
                .state(),
            OperationState::Prepared
        );
    }

    #[test]
    fn subprocess_recovery_observes_active_writer_before_reading_interrupted_journal() {
        let test_name = "install::receipt_store::tests::subprocess_recovery_observes_active_writer_before_reading_interrupted_journal";
        if let Some(data) = std::env::var_os("THRN_M2_06_ACTIVE_WRITER_DATA") {
            let store = ReceiptStore::new(PathBuf::from(data).join("receipts"));
            match store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults) {
                Err(super::RecoveryError::Store(ReceiptStoreError::BusyOrInterrupted)) => {
                    std::process::exit(0)
                }
                _ => std::process::exit(91),
            }
        }

        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let install_session = session(&project, &staging);
        let intent = prepared_journal_intent(&install_session, 1);
        let reservation = store
            .reserve(intent.receipt, &NoReceiptFaults)
            .expect("active writer reservation");
        let operations = data.join("operations");
        fs::create_dir(&operations).expect("operations directory");
        fs::write(operations.join(".pending-interrupted"), b"partial journal")
            .expect("interrupted journal publication");

        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Store(
                ReceiptStoreError::BusyOrInterrupted
            ))
        ));
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env("THRN_M2_06_ACTIVE_WRITER_DATA", &data)
            .status()
            .expect("launch active-writer recovery probe");
        assert_eq!(status.code(), Some(0));
        drop(reservation);
    }

    #[test]
    fn failed_commit_never_publishes_a_successful_receipt() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let session = session(&project, &staging);
        let result = session.commit_and_record_at(
            &store,
            1,
            &FailInstallAt(InstallOperation::CommitNoReplace),
            &NoReceiptFaults,
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::Commit(
                _
            ))
        ));
        assert!(store.list().expect("empty after failed commit").is_empty());
        assert!(!project.join(".agents/skills/uncharted-ideation").exists());
    }

    #[test]
    fn destination_directory_sync_failure_is_recovered_from_the_journal() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at(
            &store,
            1,
            &FailInstallAt(InstallOperation::SyncDestinationDirectory),
            &NoReceiptFaults,
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::Commit(
                _
            ))
        ));
        assert!(
            project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
        assert!(
            store
                .list()
                .expect("receipt is not yet published")
                .is_empty()
        );
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover after destination sync failure"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        assert_eq!(store.list().expect("recovered receipt").len(), 1);
    }

    #[test]
    fn journal_failure_before_publication_prevents_project_mutation() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at_with_journal(
            &store,
            1,
            &NoInstallFaults,
            &NoReceiptFaults,
            &FailJournalAt(JournalOperation::WriteRecord),
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::JournalBeforeCommit(_))
        ));
        assert!(!project.join(".agents").exists());
        assert!(store.list().expect("receipt listing").is_empty());
    }

    #[test]
    fn recovery_rolls_back_a_failed_pre_rename_commit_and_is_idempotent() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at(
            &store,
            1,
            &FailInstallAt(InstallOperation::CommitNoReplace),
            &NoReceiptFaults,
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::Commit(
                _
            ))
        ));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("rollback recovery"),
            vec![super::RecoveryOutcome::RolledBack]
        );
        assert!(!project.join(".agents/skills/uncharted-ideation").exists());
        assert!(store.list().expect("no successful receipt").is_empty());
        assert!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("repeat recovery")
                .is_empty()
        );
    }

    #[test]
    fn recovery_retries_after_cleanup_mutation_reports_failure() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(
            session(&project, &staging)
                .commit_and_record_at(
                    &store,
                    1,
                    &FailInstallAt(InstallOperation::CommitNoReplace),
                    &NoReceiptFaults,
                )
                .is_err()
        );
        assert!(matches!(
            store.recover_with_all_faults(
                &FailAfterInstallAt(InstallOperation::RemoveFile),
                &NoJournalFaultsForTest,
                &NoReceiptFaults,
            ),
            Err(super::RecoveryError::Conflict { .. })
        ));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("retry cleanup"),
            vec![super::RecoveryOutcome::RolledBack]
        );
        assert!(!project.join(".agents/skills/uncharted-ideation").exists());
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn recovery_finalizes_a_verified_directory_after_receipt_publication_failure() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(matches!(
            session(&project, &staging).commit_and_record_at(&store, 1, &NoInstallFaults, &fail),
            Err(crate::install::commit::InstallationCompletionError::ReceiptAfterCommit(_))
        ));
        assert!(store.list().expect("no receipt yet").is_empty());
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("finalize receipt"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        let receipts = store.list().expect("final receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            store
                .verify_current_content(&receipts[0])
                .expect("hash final content"),
            ReceiptContentState::Verified
        );
        assert!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("repeated finalization")
                .is_empty()
        );
    }

    #[test]
    fn recovery_retries_receipt_directory_sync_after_publication() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::SyncReceiptDirectory,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(matches!(
            session(&project, &staging).commit_and_record_at(&store, 1, &NoInstallFaults, &fail,),
            Err(
                crate::install::commit::InstallationCompletionError::ReceiptAfterCommit(
                    ReceiptStoreError::Durability { .. }
                )
            )
        ));
        assert_eq!(store.list().expect("visible published receipt").len(), 1);
        let journal_directory = data.join("operations");
        assert!(
            fs::read_dir(&journal_directory)
                .expect("pending journal")
                .filter_map(Result::ok)
                .any(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
        );
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("retry directory synchronization and complete"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        assert_eq!(store.list().expect("one receipt").len(), 1);
        assert!(
            fs::read_dir(journal_directory)
                .expect("cleaned journal")
                .next()
                .is_none()
        );
    }

    #[test]
    fn recovery_preserves_a_modified_committed_directory_and_reports_conflict() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        let result =
            session(&project, &staging).commit_and_record_at(&store, 1, &NoInstallFaults, &fail);
        assert!(result.is_err());
        let changed = project.join(".agents/skills/uncharted-ideation/guide.md");
        fs::write(&changed, b"user change").expect("modify committed content");
        let recovery = store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults);
        assert!(
            matches!(recovery, Err(super::RecoveryError::Conflict { .. })),
            "modified committed content recovery result: {recovery:?}"
        );
        assert_eq!(
            fs::read(changed).expect("preserved user content"),
            b"user change"
        );
        assert!(store.list().expect("no receipt was published").is_empty());
    }

    #[test]
    fn recovery_rejects_a_replaced_directory_with_matching_hashes() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(
            session(&project, &staging)
                .commit_and_record_at(&store, 1, &NoInstallFaults, &fail)
                .is_err()
        );
        let target = project.join(".agents/skills/uncharted-ideation");
        let preserved = project.join(".agents/skills/previous-object");
        fs::rename(&target, &preserved).expect("preserve original committed object");
        copy_directory_tree(&preserved, &target);
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Conflict { .. })
        ));
        assert_eq!(
            fs::read(target.join("guide.md")).expect("replacement content preserved"),
            b"guide"
        );
        assert_eq!(
            fs::read(preserved.join("guide.md")).expect("original content preserved"),
            b"guide"
        );
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn recovery_rejects_a_replaced_project_root() {
        let (parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(
            session(&project, &staging)
                .commit_and_record_at(&store, 1, &NoInstallFaults, &fail)
                .is_err()
        );
        let preserved = parent.path().join("project-original");
        fs::rename(&project, &preserved).expect("preserve original project root");
        fs::create_dir(&project).expect("replace project root");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Conflict { .. })
        ));
        assert!(
            preserved
                .join(".agents/skills/uncharted-ideation/guide.md")
                .is_file()
        );
        assert!(!project.join(".agents").exists());
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn recovery_rejects_missing_and_extra_destination_files() {
        for add_extra_file in [false, true] {
            let (_parent, project, staging, data) = setup();
            let store = ReceiptStore::new(data.join("receipts"));
            let fail = FailReceiptAt {
                operation: ReceiptOperation::PublishNoReplace,
                occurrence: 1,
                seen: Mutex::new(HashMap::new()),
            };
            assert!(
                session(&project, &staging)
                    .commit_and_record_at(&store, 1, &NoInstallFaults, &fail)
                    .is_err()
            );
            let destination = project.join(".agents/skills/uncharted-ideation");
            let changed = if add_extra_file {
                let extra = destination.join("user-added.txt");
                fs::write(&extra, b"user content").expect("add user file");
                extra
            } else {
                let missing = destination.join("guide.md");
                fs::remove_file(&missing).expect("remove expected file");
                missing
            };
            assert!(matches!(
                store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
                Err(super::RecoveryError::Conflict { .. })
            ));
            assert_eq!(
                changed.exists(),
                add_extra_file,
                "recovery must preserve user-created content and not recreate missing content"
            );
            assert!(store.list().expect("no receipt finalized").is_empty());
        }
    }

    #[test]
    fn recovery_preserves_a_conflicting_valid_receipt() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let install = session(&project, &staging);
        let project_identity = install
            .plan()
            .destination_snapshot()
            .project_identity()
            .expect("project identity");
        let expected = InstallationReceipt::from_committed_plan(
            install.plan(),
            install.inventory(),
            project_identity,
            1,
        )
        .expect("expected receipt");
        let mut conflicting_json: serde_json::Value =
            serde_json::from_slice(&expected.to_json().expect("serialize expected receipt"))
                .expect("receipt json");
        conflicting_json["installedAtUnixSeconds"] = serde_json::Value::from(2_u64);
        let conflicting = InstallationReceipt::from_json(
            &serde_json::to_vec(&conflicting_json).expect("serialize conflicting receipt"),
        )
        .expect("valid conflicting receipt");
        assert_ne!(conflicting, expected);

        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(
            install
                .commit_and_record_at(&store, 1, &NoInstallFaults, &fail)
                .is_err()
        );
        let receipt_path = data.join("receipts").join(
            conflicting
                .receipt_filename()
                .expect("conflicting filename"),
        );
        let bytes = conflicting.to_json().expect("conflicting bytes");
        fs::write(&receipt_path, &bytes).expect("install conflicting receipt fixture");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Conflict { .. })
        ));
        assert_eq!(fs::read(receipt_path).expect("preserved receipt"), bytes);
        assert!(
            project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlink_parent_substitution_and_preserves_external_content() {
        use std::os::unix::fs::symlink;

        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        assert!(
            session(&project, &staging)
                .commit_and_record_at(&store, 1, &NoInstallFaults, &fail)
                .is_err()
        );
        let skills = project.join(".agents/skills");
        let preserved = project.join(".agents/skills-original");
        let external = project.join("external");
        fs::rename(&skills, &preserved).expect("preserve original skills directory");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("user.txt"), b"keep outside link").expect("external sentinel");
        symlink(&external, &skills).expect("substitute skills symlink");
        let recovery = store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults);
        assert!(
            matches!(recovery, Err(super::RecoveryError::Conflict { .. })),
            "symlink substitution recovery result: {recovery:?}"
        );
        assert_eq!(
            fs::read(external.join("user.txt")).expect("external content preserved"),
            b"keep outside link"
        );
        assert!(preserved.join("uncharted-ideation/guide.md").is_file());
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn active_installation_excludes_simultaneous_recovery() {
        let (_parent, project, staging, data) = setup();
        let receipt_directory = data.join("receipts");
        let store = ReceiptStore::new(receipt_directory.clone());
        let probe = RecoverWhileWriting {
            receipt_directory,
            attempted: Mutex::new(false),
        };
        session(&project, &staging)
            .commit_and_record_at(&store, 1, &NoInstallFaults, &probe)
            .expect("install succeeds despite blocked concurrent recovery");
        assert!(*probe.attempted.lock().expect("probe result"));
    }

    #[test]
    fn failed_completed_journal_cleanup_is_retried_idempotently() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at_with_journal(
            &store,
            1,
            &NoInstallFaults,
            &NoReceiptFaults,
            &FailJournalAt(JournalOperation::CleanupRecord),
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::JournalAfterReceipt(_))
        ));
        assert_eq!(store.list().expect("published receipt remains").len(), 1);
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("cleanup complete operation record"),
            vec![super::RecoveryOutcome::CleanedCompletedRecord]
        );
    }

    #[test]
    fn journal_cleanup_sync_failure_keeps_terminal_record_for_retry() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at_with_journal(
            &store,
            1,
            &NoInstallFaults,
            &NoReceiptFaults,
            &FailAfterJournalCleanupSync {
                cleanup_started: Mutex::new(false),
            },
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::JournalAfterReceipt(_))
        ));
        assert_eq!(store.list().expect("published receipt remains").len(), 1);
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("retry journal cleanup"),
            vec![super::RecoveryOutcome::CleanedCompletedRecord]
        );
    }

    #[test]
    fn corrupt_or_interrupted_journal_is_preserved_and_reported() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(
            session(&project, &staging)
                .commit_and_record_at(
                    &store,
                    1,
                    &FailInstallAt(InstallOperation::CommitNoReplace),
                    &NoReceiptFaults,
                )
                .is_err()
        );
        let journal_path = fs::read_dir(data.join("operations"))
            .expect("journal store")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("journal record");
        let valid_record = fs::read(&journal_path).expect("valid journal bytes");
        fs::write(&journal_path, b"{").expect("truncate journal");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Journal(JournalError::Corrupt { .. }))
        ));
        assert_eq!(
            fs::read(&journal_path).expect("preserved corrupt journal"),
            b"{"
        );
        let mut unsupported: serde_json::Value =
            serde_json::from_slice(&valid_record).expect("valid journal json");
        unsupported["schemaVersion"] = serde_json::Value::from(99_u64);
        let unsupported_bytes =
            serde_json::to_vec(&unsupported).expect("unsupported journal bytes");
        fs::write(&journal_path, &unsupported_bytes).expect("write unsupported schema");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Journal(
                JournalError::UnsupportedSchema(99)
            ))
        ));
        assert_eq!(
            fs::read(&journal_path).expect("preserved unsupported journal"),
            unsupported_bytes
        );
        fs::remove_file(&journal_path).expect("remove corrupted fixture");
        fs::write(data.join("operations/.pending-interrupted"), b"partial")
            .expect("interrupted journal publication");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Journal(
                JournalError::InterruptedPublication { .. }
            ))
        ));
        assert_eq!(
            fs::read(data.join("operations/.pending-interrupted")).expect("preserved journal temp"),
            b"partial"
        );
    }

    #[test]
    fn changed_historical_journal_identity_breaks_the_sequence() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(
            session(&project, &staging)
                .commit_and_record_at(
                    &store,
                    1,
                    &FailInstallAt(InstallOperation::CommitNoReplace),
                    &NoReceiptFaults,
                )
                .is_err()
        );
        let mut records = fs::read_dir(data.join("operations"))
            .expect("journal store")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        records.sort();
        let changed = records
            .iter()
            .find_map(|path| {
                let bytes = fs::read(path).ok()?;
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                let identity = value.get_mut("stagingIdentity")?.as_object_mut()?;
                let object = identity.get_mut("object")?.as_u64()?;
                identity.insert("object".to_owned(), serde_json::Value::from(object + 1));
                Some((path.clone(), serde_json::to_vec(&value).ok()?))
            })
            .expect("a durable staging identity record");
        fs::write(&changed.0, changed.1).expect("tamper historical identity snapshot");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Journal(
                JournalError::InvalidTransition
            ))
        ));
    }

    #[test]
    fn oversized_journal_fails_closed_before_project_changes() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(
            session(&project, &staging)
                .commit_and_record_at(
                    &store,
                    1,
                    &FailInstallAt(InstallOperation::CommitNoReplace),
                    &NoReceiptFaults,
                )
                .is_err()
        );
        let journal_path = fs::read_dir(data.join("operations"))
            .expect("journal store")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("journal record");
        fs::write(&journal_path, vec![b'x'; MAX_JOURNAL_BYTES + 1]).expect("oversize journal");
        assert!(matches!(
            store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults),
            Err(super::RecoveryError::Journal(JournalError::Oversized))
        ));
        assert!(!project.join(".agents/skills/uncharted-ideation").exists());
    }

    #[test]
    fn subprocess_crash_immediately_before_directory_rename_is_rolled_back() {
        if std::env::var_os("THRN_M2_06_CRASH_AFTER_RECOVERY_REMOVE").is_some() {
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = store.recover_with_all_faults(
                &ExitAfterInstall(InstallOperation::RemoveFile),
                &NoJournalFaultsForTest,
                &NoReceiptFaults,
            );
            panic!("child should exit after a recovery file removal");
        }
        if std::env::var_os("THRN_M2_06_CRASH_BEFORE_RENAME").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session(&project, &staging).commit_and_record_at(
                &store,
                1,
                &ExitAtInstall(InstallOperation::CommitNoReplace),
                &NoReceiptFaults,
            );
            panic!("child should exit at the no-replace boundary");
        }

        let (_parent, project, staging, data) = setup();
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_immediately_before_directory_rename_is_rolled_back")
            .arg("--nocapture")
            .env("THRN_M2_06_CRASH_BEFORE_RENAME", "1")
            .env("THRN_M2_06_PROJECT", &project)
            .env("THRN_M2_06_STAGING", &staging)
            .env("THRN_M2_06_DATA", &data)
            .status()
            .expect("launch crash simulation");
        assert_eq!(status.code(), Some(86));
        let store = ReceiptStore::new(data.join("receipts"));
        let cleanup_status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_immediately_before_directory_rename_is_rolled_back")
            .arg("--nocapture")
            .env("THRN_M2_06_CRASH_AFTER_RECOVERY_REMOVE", "1")
            .env("THRN_M2_06_PROJECT", &project)
            .env("THRN_M2_06_STAGING", &staging)
            .env("THRN_M2_06_DATA", &data)
            .status()
            .expect("launch recovery crash simulation");
        assert_eq!(cleanup_status.code(), Some(87));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover process interruption"),
            vec![super::RecoveryOutcome::RolledBack]
        );
        assert!(!project.join(".agents/skills/uncharted-ideation").exists());
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn subprocess_crash_after_durable_preparation_is_rolled_back() {
        if std::env::var_os("THRN_M2_06_CRASH_AFTER_PREPARED").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session(&project, &staging).commit_and_record_at_with_journal(
                &store,
                1,
                &NoInstallFaults,
                &NoReceiptFaults,
                &ExitAfterJournal(JournalOperation::SyncStore),
            );
            panic!("child should exit after the prepared journal is synchronized");
        }

        let (_parent, project, staging, data) = setup();
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_after_durable_preparation_is_rolled_back")
            .arg("--nocapture")
            .env("THRN_M2_06_CRASH_AFTER_PREPARED", "1")
            .env("THRN_M2_06_PROJECT", &project)
            .env("THRN_M2_06_STAGING", &staging)
            .env("THRN_M2_06_DATA", &data)
            .status()
            .expect("launch crash simulation");
        assert_eq!(status.code(), Some(88));
        let store = ReceiptStore::new(data.join("receipts"));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover durable preparation"),
            vec![super::RecoveryOutcome::RolledBack]
        );
        assert!(!project.join(".agents").exists());
        assert!(store.list().expect("no receipt").is_empty());
    }

    #[test]
    fn subprocess_crashes_after_each_durable_operation_state() {
        let test_name =
            "install::receipt_store::tests::subprocess_crashes_after_each_durable_operation_state";
        if let Some(state) = std::env::var_os("THRN_M2_06_CRASH_AFTER_STATE") {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session(&project, &staging).commit_and_record_at_with_journal(
                &store,
                1,
                &NoInstallFaults,
                &NoReceiptFaults,
                &ExitAfterJournalState {
                    directory: data.join("operations"),
                    state: state.to_string_lossy().into_owned(),
                },
            );
            panic!("child should exit after the requested durable journal state");
        }

        for (state, expected_outcome) in [
            ("prepared", super::RecoveryOutcome::RolledBack),
            (
                "filesystemCommitInProgress",
                super::RecoveryOutcome::RolledBack,
            ),
            (
                "directoryCommitVerified",
                super::RecoveryOutcome::FinalizedReceipt,
            ),
            (
                "receiptPublicationPending",
                super::RecoveryOutcome::FinalizedReceipt,
            ),
            ("receiptPublished", super::RecoveryOutcome::FinalizedReceipt),
            ("complete", super::RecoveryOutcome::CleanedCompletedRecord),
        ] {
            let (_parent, project, staging, data) = setup();
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg(test_name)
                .arg("--nocapture")
                .env("THRN_M2_06_CRASH_AFTER_STATE", state)
                .env("THRN_M2_06_PROJECT", &project)
                .env("THRN_M2_06_STAGING", &staging)
                .env("THRN_M2_06_DATA", &data)
                .status()
                .expect("launch state crash simulation");
            assert_eq!(status.code(), Some(90), "state={state}");
            let store = ReceiptStore::new(data.join("receipts"));
            assert_eq!(
                store
                    .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                    .expect("recover durable operation state"),
                vec![expected_outcome],
                "state={state}"
            );
            let receipts = store.list().expect("consistent receipt listing");
            if expected_outcome == super::RecoveryOutcome::RolledBack {
                assert!(receipts.is_empty(), "state={state}");
                assert!(!project.join(".agents").exists(), "state={state}");
            } else {
                assert_eq!(receipts.len(), 1, "state={state}");
                assert_eq!(
                    store
                        .verify_current_content(&receipts[0])
                        .expect("verified recovered destination"),
                    ReceiptContentState::Verified,
                    "state={state}"
                );
            }
        }
    }

    #[test]
    fn subprocess_user_install_crash_after_commit_recovers_schema_v3_receipt() {
        if std::env::var_os("THRN_USER_INSTALL_CRASH_CHILD").is_some() {
            let home = PathBuf::from(std::env::var_os("THRN_USER_INSTALL_HOME").expect("home"));
            let staging =
                PathBuf::from(std::env::var_os("THRN_USER_INSTALL_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_USER_INSTALL_DATA").expect("data"));
            let catalog = parse_catalog_json(include_bytes!(
                "../../tests/fixtures/catalog/test-only-valid-v1.json"
            ))
            .expect("catalog");
            let scope = ResolvedScope::for_test(InstallScope::User, home);
            let install = prepare_install_session_in_scope(
                &catalog,
                "uncharted",
                "codex",
                &scope,
                &FakeTransport(archive()),
                &TestFactory(staging.clone()),
            )
            .expect("user installation session");
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = install.commit_and_record_at_with_journal(
                &store,
                1,
                &NoInstallFaults,
                &NoReceiptFaults,
                &ExitAfterJournalState {
                    directory: data.join("operations"),
                    state: "directoryCommitVerified".to_owned(),
                },
            );
            panic!("child should exit after user directory commit");
        }

        let (_parent, project, staging, data) = setup();
        let home = project.parent().expect("fixture root").join("home");
        fs::create_dir(&home).expect("isolated user home");
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_user_install_crash_after_commit_recovers_schema_v3_receipt")
            .arg("--nocapture")
            .env("THRN_USER_INSTALL_CRASH_CHILD", "1")
            .env("THRN_USER_INSTALL_HOME", &home)
            .env("THRN_USER_INSTALL_STAGING", &staging)
            .env("THRN_USER_INSTALL_DATA", &data)
            .status()
            .expect("launch user install crash simulation");
        assert_eq!(status.code(), Some(90));
        let store = ReceiptStore::new(data.join("receipts"));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover committed user install"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        let receipts = store.list().expect("user receipt after recovery");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].scope(), "user");
        assert_eq!(receipts[0].schema_version(), 3);
        assert_eq!(receipts[0].root_path(), home);
        assert_eq!(
            store
                .verify_current_content(&receipts[0])
                .expect("verified user tree"),
            ReceiptContentState::Verified
        );
        assert!(!project.join(".agents").exists());
    }

    #[test]
    fn subprocess_crash_after_directory_rename_finalizes_verified_receipt() {
        if std::env::var_os("THRN_M2_06_CRASH_AFTER_DIRECTORY_RENAME").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let agent = match std::env::var("THRN_M2_06_AGENT").as_deref() {
                Ok("claude-code") => InstallAgent::ClaudeCode,
                _ => InstallAgent::Codex,
            };
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session_for_agent(&project, &staging, agent).commit_and_record_at(
                &store,
                1,
                &ExitAfterInstall(InstallOperation::CommitNoReplace),
                &NoReceiptFaults,
            );
            panic!("child should exit after the no-replace rename");
        }

        for agent in [InstallAgent::Codex, InstallAgent::ClaudeCode] {
            let (_parent, project, staging, data) = setup();
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::receipt_store::tests::subprocess_crash_after_directory_rename_finalizes_verified_receipt")
                .arg("--nocapture")
                .env("THRN_M2_06_CRASH_AFTER_DIRECTORY_RENAME", "1")
                .env("THRN_M2_06_AGENT", agent.as_str())
                .env("THRN_M2_06_PROJECT", &project)
                .env("THRN_M2_06_STAGING", &staging)
                .env("THRN_M2_06_DATA", &data)
                .status()
                .expect("launch crash simulation");
            assert_eq!(status.code(), Some(87), "agent={}", agent.as_str());
            let store = ReceiptStore::new(data.join("receipts"));
            let scope =
                ResolvedScope::from_agent_validated(agent, InstallScope::Project, project.clone());
            assert_eq!(
                store
                    .recover_scope_with_faults(&scope, &NoJournalFaultsForTest, &NoReceiptFaults)
                    .expect("finalize verified committed directory"),
                vec![super::RecoveryOutcome::FinalizedReceipt]
            );
            let receipts = store.list().expect("published receipt");
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].agent(), agent.as_str());
            assert_eq!(
                receipts[0].schema_version(),
                if agent == InstallAgent::ClaudeCode {
                    4
                } else {
                    2
                }
            );
            assert_eq!(
                store
                    .verify_current_content(&receipts[0])
                    .expect("verify recovered install"),
                ReceiptContentState::Verified
            );
        }
    }

    #[test]
    fn subprocess_crash_during_partial_file_copy_preserves_unverified_staging() {
        if std::env::var_os("THRN_M2_06_CRASH_DURING_COPY").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let session = session_with_archive(
                &project,
                &staging,
                archive_with_guide(&vec![b'x'; 24 * 1024]),
            );
            let _ = session.commit_and_record_at(
                &store,
                1,
                &ExitAfterInstall(InstallOperation::WriteDestination),
                &NoReceiptFaults,
            );
            panic!("child should exit during the staged file copy");
        }

        let (_parent, project, staging, data) = setup();
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_during_partial_file_copy_preserves_unverified_staging")
            .arg("--nocapture")
            .env("THRN_M2_06_CRASH_DURING_COPY", "1")
            .env("THRN_M2_06_PROJECT", &project)
            .env("THRN_M2_06_STAGING", &staging)
            .env("THRN_M2_06_DATA", &data)
            .status()
            .expect("launch crash simulation");
        assert_eq!(status.code(), Some(87));
        let store = ReceiptStore::new(data.join("receipts"));
        let recovery = store.recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults);
        assert!(
            matches!(recovery, Err(super::RecoveryError::Conflict { .. })),
            "partial-copy recovery must report a preserved-content conflict: {recovery:?}"
        );
        let stage = fs::read_dir(project.join(".agents/skills"))
            .expect("skills parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".thrn-install-"))
            })
            .expect("partial operation staging is preserved");
        assert!(
            stage
                .join("SKILL.md")
                .metadata()
                .expect("partial file")
                .len()
                > 0
        );
        assert!(store.list().expect("no successful receipt").is_empty());
    }

    #[test]
    fn subprocess_crash_after_receipt_rename_is_completed_idempotently() {
        if std::env::var_os("THRN_M2_06_CRASH_AFTER_RECEIPT_RENAME").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let agent = match std::env::var("THRN_M2_06_AGENT").as_deref() {
                Ok("claude-code") => InstallAgent::ClaudeCode,
                _ => InstallAgent::Codex,
            };
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session_for_agent(&project, &staging, agent).commit_and_record_at(
                &store,
                1,
                &NoInstallFaults,
                &ExitAfterReceipt(ReceiptOperation::PublishNoReplace),
            );
            panic!("child should exit after the receipt rename");
        }

        for agent in [InstallAgent::Codex, InstallAgent::ClaudeCode] {
            let (_parent, project, staging, data) = setup();
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::receipt_store::tests::subprocess_crash_after_receipt_rename_is_completed_idempotently")
                .arg("--nocapture")
                .env("THRN_M2_06_CRASH_AFTER_RECEIPT_RENAME", "1")
                .env("THRN_M2_06_AGENT", agent.as_str())
                .env("THRN_M2_06_PROJECT", &project)
                .env("THRN_M2_06_STAGING", &staging)
                .env("THRN_M2_06_DATA", &data)
                .status()
                .expect("launch crash simulation");
            assert_eq!(status.code(), Some(89), "agent={}", agent.as_str());
            let store = ReceiptStore::new(data.join("receipts"));
            let scope = ResolvedScope::from_agent_validated(agent, InstallScope::Project, project);
            assert_eq!(
                store
                    .recover_scope_with_faults(&scope, &NoJournalFaultsForTest, &NoReceiptFaults)
                    .expect("complete already-published receipt"),
                vec![super::RecoveryOutcome::FinalizedReceipt]
            );
            let receipts = store.list().expect("one receipt");
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].agent(), agent.as_str());
            assert_eq!(
                receipts[0].schema_version(),
                if agent == InstallAgent::ClaudeCode {
                    4
                } else {
                    2
                }
            );
            assert!(
                store
                    .recover_scope_with_faults(&scope, &NoJournalFaultsForTest, &NoReceiptFaults)
                    .expect("repeat recovery")
                    .is_empty()
            );
        }
    }

    #[test]
    fn subprocess_crash_after_receipt_temp_sync_finalizes_without_overwrite() {
        if std::env::var_os("THRN_M2_06_CRASH_BEFORE_RECEIPT_RENAME").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M2_06_PROJECT").expect("project"));
            let staging = PathBuf::from(std::env::var_os("THRN_M2_06_STAGING").expect("staging"));
            let data = PathBuf::from(std::env::var_os("THRN_M2_06_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let _ = session(&project, &staging).commit_and_record_at(
                &store,
                1,
                &NoInstallFaults,
                &ExitAtReceipt(ReceiptOperation::PublishNoReplace),
            );
            panic!("child should exit at receipt publication boundary");
        }

        let (_parent, project, staging, data) = setup();
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::receipt_store::tests::subprocess_crash_after_receipt_temp_sync_finalizes_without_overwrite")
            .arg("--nocapture")
            .env("THRN_M2_06_CRASH_BEFORE_RECEIPT_RENAME", "1")
            .env("THRN_M2_06_PROJECT", &project)
            .env("THRN_M2_06_STAGING", &staging)
            .env("THRN_M2_06_DATA", &data)
            .status()
            .expect("launch crash simulation");
        assert_eq!(status.code(), Some(86));
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::InterruptedReceiptWrite)
        ));
        assert_eq!(
            store
                .recover_with_faults(&NoJournalFaultsForTest, &NoReceiptFaults)
                .expect("recover exact receipt temporary"),
            vec![super::RecoveryOutcome::FinalizedReceipt]
        );
        let receipts = store.list().expect("published receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            store
                .verify_current_content(&receipts[0])
                .expect("verified installed tree"),
            ReceiptContentState::Verified
        );
    }

    #[test]
    fn receipt_store_preflight_failures_happen_before_project_commit() {
        for operation in [
            ReceiptOperation::CreateStore,
            ReceiptOperation::CreateWriterLock,
        ] {
            let (_parent, project, staging, data) = setup();
            let store = ReceiptStore::new(data.join("receipts"));
            let fail = FailReceiptAt {
                operation,
                occurrence: if operation == ReceiptOperation::CreateWriterLock {
                    2 // Recovery acquires the lock once before reservation.
                } else {
                    1
                },
                seen: Mutex::new(HashMap::new()),
            };
            let result = session(&project, &staging).commit_and_record_at(
                &store,
                1,
                &NoInstallFaults,
                &fail,
            );
            assert!(matches!(
                result,
                Err(crate::install::commit::InstallationCompletionError::ReceiptPreflight(_))
            ));
            assert!(!project.join(".agents/skills/uncharted-ideation").exists());
            assert!(store.list().expect("no receipt").is_empty());
        }
    }

    #[test]
    fn no_replace_publication_preserves_a_receipt_created_at_the_publish_boundary() {
        let (_parent, project, staging, data) = setup();
        let receipt_directory = data.join("receipts");
        let store = ReceiptStore::new(receipt_directory.clone());
        let hook = CreateReceiptBeforePublish {
            directory: receipt_directory.clone(),
            wrote: Mutex::new(false),
        };
        let result =
            session(&project, &staging).commit_and_record_at(&store, 1, &NoInstallFaults, &hook);
        assert!(matches!(
            result,
            Err(
                crate::install::commit::InstallationCompletionError::ReceiptAfterCommit(
                    ReceiptStoreError::DuplicateIdentity
                )
            )
        ));
        let final_receipt = fs::read_dir(&receipt_directory)
            .expect("receipt directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("v1-"))
            })
            .expect("racing receipt remains");
        assert_eq!(
            fs::read(final_receipt).expect("existing receipt bytes"),
            b"pre-existing bytes"
        );
        assert!(
            project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn existing_unreceipted_skill_remains_untouched_and_never_gets_claimed() {
        let (_parent, project, staging, data) = setup();
        let destination = project.join(".agents/skills/uncharted-ideation");
        fs::create_dir_all(&destination).expect("manual installation");
        fs::write(destination.join("keep.txt"), b"externally managed").expect("manual content");
        let store = ReceiptStore::new(data.join("receipts"));
        let result = session(&project, &staging).commit_and_record_at(
            &store,
            1,
            &NoInstallFaults,
            &NoReceiptFaults,
        );
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::Commit(
                _
            ))
        ));
        assert_eq!(
            fs::read(destination.join("keep.txt")).expect("manual file preserved"),
            b"externally managed"
        );
        assert!(store.list().expect("no ownership receipt").is_empty());
    }

    #[test]
    fn receipt_failure_after_commit_is_reported_as_incomplete_and_keeps_directory() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let session = session(&project, &staging);
        let fail = FailReceiptAt {
            operation: ReceiptOperation::PublishNoReplace,
            occurrence: 1,
            seen: Mutex::new(HashMap::new()),
        };
        let result = session.commit_and_record_at(&store, 1, &NoInstallFaults, &fail);
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::ReceiptAfterCommit(_))
        ));
        assert!(
            project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
        assert!(store.list().expect("no receipt was published").is_empty());
    }

    #[test]
    fn partial_temporary_write_is_cleaned_and_never_published() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let fail = FailReceiptAt {
            operation: ReceiptOperation::WriteTemporaryReceipt,
            occurrence: 2,
            seen: Mutex::new(HashMap::new()),
        };
        let result =
            session(&project, &staging).commit_and_record_at(&store, 1, &NoInstallFaults, &fail);
        assert!(matches!(
            result,
            Err(crate::install::commit::InstallationCompletionError::ReceiptAfterCommit(_))
        ));
        let names = fs::read_dir(data.join("receipts"))
            .expect("storage exists")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| name != ".writer.lock")
            .collect::<Vec<_>>();
        assert!(names.is_empty());
    }

    #[test]
    fn duplicate_identity_is_rejected_before_project_commit() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let first = session(&project, &staging);
        first
            .commit_and_record_at(&store, 1, &NoInstallFaults, &NoReceiptFaults)
            .expect("first commit");
        let second = session(&project, &staging);
        let error = second
            .commit_and_record_at(&store, 2, &NoInstallFaults, &NoReceiptFaults)
            .expect_err("duplicate is blocked");
        assert!(matches!(
            error,
            crate::install::commit::InstallationCompletionError::ReceiptPreflight(
                ReceiptStoreError::DuplicateIdentity
            )
        ));
        assert_eq!(store.list().expect("one record").len(), 1);
    }

    #[test]
    fn verified_content_check_detects_modified_missing_and_extra_files() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        let receipt = session(&project, &staging)
            .commit_and_record_at(&store, 1, &NoInstallFaults, &NoReceiptFaults)
            .expect("commit");
        let destination = project.join(".agents/skills/uncharted-ideation");
        fs::write(destination.join("guide.md"), b"changed").expect("modify installed file");
        assert_eq!(
            store
                .verify_current_content(&receipt)
                .expect("verify changed"),
            ReceiptContentState::Modified
        );
        fs::write(destination.join("guide.md"), b"guide").expect("restore file");
        fs::write(destination.join("extra.txt"), b"extra").expect("add extra file");
        assert_eq!(
            store
                .verify_current_content(&receipt)
                .expect("verify extra"),
            ReceiptContentState::Modified
        );
        fs::remove_file(destination.join("extra.txt")).expect("remove extra");
        fs::remove_file(destination.join("guide.md")).expect("remove managed file");
        assert_eq!(
            store
                .verify_current_content(&receipt)
                .expect("verify missing"),
            ReceiptContentState::Modified
        );
        fs::remove_dir_all(project.join(".agents/skills/uncharted-ideation"))
            .expect("remove destination");
        assert_eq!(
            store
                .verify_current_content(&receipt)
                .expect("verify missing directory"),
            ReceiptContentState::Missing
        );
    }

    #[test]
    fn corrupt_unknown_duplicate_and_conflicting_storage_fails_closed() {
        let (_parent, project, staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        fs::create_dir_all(data.join("receipts")).expect("receipt storage");
        fs::write(data.join("receipts/unknown.json"), b"{}").expect("unknown object");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::InvalidFilename)
        ));
        fs::remove_file(data.join("receipts/unknown.json")).expect("remove fixture");
        fs::write(
            data.join(
                "receipts/v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
            ),
            b"{",
        )
        .expect("corrupt receipt");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::Validation { .. })
        ));
        fs::remove_dir_all(data.join("receipts")).expect("clear storage");
        let first = session(&project, &staging)
            .commit_and_record_at(&store, 1, &NoInstallFaults, &NoReceiptFaults)
            .expect("receipt");
        let mut conflicting_json: serde_json::Value =
            serde_json::from_slice(&first.to_json().expect("json")).expect("value");
        conflicting_json["catalogResourceId"] = serde_json::json!("creative-lab");
        let conflicting = InstallationReceipt::from_json(
            &serde_json::to_vec(&conflicting_json).expect("changed JSON"),
        )
        .expect("valid conflicting claim");
        let wrong_name = conflicting.receipt_filename().expect("new identity");
        fs::write(
            data.join("receipts").join(wrong_name),
            conflicting
                .to_json()
                .expect("serialize conflicting receipt"),
        )
        .expect("second conflicting claim");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::DestinationConflict { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_receipt_storage_and_symlink_receipt_files_are_rejected() {
        use std::os::unix::fs::symlink;
        let (_parent, _project, _staging, data) = setup();
        let real = data.join("real");
        fs::create_dir_all(&real).expect("real dir");
        symlink(&real, data.join("receipts")).expect("storage link");
        let store = ReceiptStore::new(data.join("receipts"));
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::UnsafeDirectory)
        ));

        fs::remove_file(data.join("receipts")).expect("remove link");
        fs::create_dir(data.join("receipts")).expect("receipt dir");
        symlink(
            "missing-target",
            data.join(
                "receipts/v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
            ),
        )
        .expect("receipt link");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::UnsafeObject { .. })
        ));
    }

    #[test]
    fn interrupted_temporary_or_writer_lock_objects_fail_closed() {
        let (_parent, _project, _staging, data) = setup();
        let store = ReceiptStore::new(data.join("receipts"));
        fs::create_dir_all(data.join("receipts")).expect("storage");
        fs::write(data.join("receipts/.pending-interrupted"), b"partial").expect("leftover temp");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::InterruptedReceiptWrite)
        ));
        fs::remove_file(data.join("receipts/.pending-interrupted")).expect("remove temp");
        fs::write(data.join("receipts/.writer.lock"), b"").expect("stale lock");
        assert!(
            store
                .list()
                .expect("idle stale lock is read-only")
                .is_empty()
        );
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data.join("receipts/.writer.lock"))
            .expect("open active lock");
        lock.try_lock().expect("hold active lock");
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::BusyOrInterrupted)
        ));
    }
}
