//! Safe replacement of an exactly verified THRN-managed agent skill.
//!
//! Update planning is read-only. A plan retains the validated pinned source stage and the
//! receipt-backed identities needed by the separately journaled replacement transaction.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    install::{
        commit::{NodeKind, SecureDir},
        destination::FileIdentity,
        inventory::{InventoryError, StagedInventory, inspect_staged_skill},
        plan::{InstallPlan, PlanError, build_update_install_plan_for_agent_scope},
        receipt::InstallationReceipt,
        removal::{RemovalError, RemovalIdentity, RemovalObject, RemovalObjectKind, RemovalPlan},
        scope::ResolvedScope,
    },
    models::catalog::{Catalog, Resource, SkillId},
    sources::{
        github::{FetchError, GithubTransport, fetch_and_stage_skill_with_factory},
        staging::{StagedSkill, TempDirFactory},
    },
};

use super::receipt_store::{ReceiptFaultInjector, ReceiptStore, WriterLock};

const PROJECT_UPDATE_JOURNAL_SCHEMA_VERSION: u64 = 1;
const USER_UPDATE_JOURNAL_SCHEMA_VERSION: u64 = 2;
const CLAUDE_UPDATE_JOURNAL_SCHEMA_VERSION: u64 = 3;
const ADAPTER_UPDATE_JOURNAL_SCHEMA_VERSION: u64 = 4;
const MAX_UPDATE_JOURNAL_BYTES: usize = 12 * 1024 * 1024;
const MAX_UPDATE_JOURNAL_FILES: usize = 16_384;
const MAX_UPDATE_JOURNAL_TOTAL_BYTES: u64 = 384 * 1024 * 1024;

/// One exact path-level difference between the installed receipt and the validated replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateFileChange {
    path: String,
    kind: UpdateFileChangeKind,
    old_bytes: Option<u64>,
    new_bytes: Option<u64>,
}

impl UpdateFileChange {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> UpdateFileChangeKind {
        self.kind
    }

    pub fn old_bytes(&self) -> Option<u64> {
        self.old_bytes
    }

    pub fn new_bytes(&self) -> Option<u64> {
        self.new_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateFileChangeKind {
    Added,
    Removed,
    Changed,
    Unchanged,
}

/// The old and replacement receipts claim one deterministic installation identity. The old
/// record stays in an operation-owned backup until the new tree and published receipt verify.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptTransitionPlan {
    active_receipt_name: String,
    old_receipt_file_identity: FileIdentity,
    old_receipt_sha256: String,
    publication: ReceiptPublicationMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptPublicationMode {
    BackupOldThenPublishNoReplace,
}

impl ReceiptTransitionPlan {
    pub fn active_receipt_name(&self) -> &str {
        &self.active_receipt_name
    }

    pub fn old_receipt_file_identity(&self) -> FileIdentity {
        self.old_receipt_file_identity
    }

    pub fn old_receipt_sha256(&self) -> &str {
        &self.old_receipt_sha256
    }

    pub fn publication(&self) -> ReceiptPublicationMode {
        self.publication
    }
}

/// Immutable update proposal. It owns the validated source stage until execution or dry-run ends.
#[derive(Debug)]
pub struct UpdatePlan {
    old: RemovalPlan,
    staged: StagedSkill,
    inventory: StagedInventory,
    replacement: InstallPlan,
    receipt_transition: ReceiptTransitionPlan,
    changes: Vec<UpdateFileChange>,
    operation_id: String,
    stage_name: String,
    backup_name: String,
    receipt_backup_name: String,
    assumptions: Vec<UpdateAssumption>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateAssumption {
    ReceiptFileIdentityUnchanged,
    ProjectAndDestinationIdentitiesUnchanged,
    InstalledObjectInventoryUnchanged,
    ReplacementStageDigestInventoryUnchanged,
    BackupAndDestinationNamesRemainAbsent,
    WriterLockRemainsExclusive,
}

impl UpdatePlan {
    pub fn receipt(&self) -> &InstallationReceipt {
        self.old.receipt()
    }

    pub fn resource_id(&self) -> &str {
        self.replacement.resource_id().as_str()
    }

    pub fn skill_name(&self) -> &str {
        self.replacement.skill_name().as_str()
    }

    pub fn destination(&self) -> &Path {
        self.replacement.destination()
    }

    pub fn previous_revision(&self) -> &str {
        self.old.receipt().source_commit()
    }

    pub fn new_revision(&self) -> &str {
        self.replacement.source_commit().as_str()
    }

    pub fn changes(&self) -> &[UpdateFileChange] {
        &self.changes
    }

    pub fn old_file_count(&self) -> u64 {
        self.old.file_count()
    }

    pub fn old_byte_count(&self) -> u64 {
        self.old.byte_count()
    }

    pub fn new_file_count(&self) -> u64 {
        self.inventory.files().len() as u64
    }

    pub fn new_byte_count(&self) -> u64 {
        self.inventory.total_bytes()
    }

    pub fn planned_backup_directory(&self) -> PathBuf {
        self.destination()
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(&self.backup_name)
    }

    pub fn planned_stage_directory(&self) -> PathBuf {
        self.destination()
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(&self.stage_name)
    }

    pub fn assumptions(&self) -> &[UpdateAssumption] {
        &self.assumptions
    }

    pub fn receipt_transition(&self) -> &ReceiptTransitionPlan {
        &self.receipt_transition
    }

    pub(crate) fn old_plan(&self) -> &RemovalPlan {
        &self.old
    }

    pub(crate) fn staged(&self) -> &StagedSkill {
        &self.staged
    }

    pub(crate) fn inventory(&self) -> &StagedInventory {
        &self.inventory
    }

    pub(crate) fn replacement(&self) -> &InstallPlan {
        &self.replacement
    }
}

#[derive(Debug)]
pub(crate) enum UpdatePreparation {
    Ready(Box<UpdatePlan>),
    AlreadyUpToDate(Box<InstallationReceipt>),
}

#[derive(Debug, Error)]
pub(crate) enum UpdateError {
    #[error("the selected agent does not match the resolved scope or receipt")]
    UnsupportedAgent,
    #[error("update requires an exact canonical catalog resource ID")]
    NonCanonicalId,
    #[error("no validated local catalog resource has canonical ID {0:?}")]
    UnknownResource(String),
    #[error("no valid schema-v2 THRN ownership receipt matches this resource and project")]
    MissingReceipt,
    #[error("update planning conflict: {0}")]
    Conflict(String),
    #[error(
        "update cannot continue because an installation, removal, or update recovery is pending: {0}"
    )]
    RecoveryRequired(String),
    #[error(transparent)]
    Removal(#[from] RemovalError),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Inventory(#[from] InventoryError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Destination(#[from] super::destination::DestinationError),
    #[error("receipt store failed: {0}")]
    Store(String),
    #[error(
        "update transaction {operation_id} may be incomplete; preserve the directory, receipt, backup, and journal for recovery: {detail}"
    )]
    Incomplete {
        operation_id: String,
        detail: String,
    },
}

/// Resolve only an exact canonical ID, verify the installed schema-v2 ownership claim, and stage
/// the local catalog's exact pinned GitHub revision. No catalog refresh is performed.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_update<T: GithubTransport + ?Sized>(
    catalog: &Catalog,
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    current_directory: &Path,
    store: &ReceiptStore,
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<UpdatePreparation, UpdateError> {
    let root = super::removal::resolve_project_root(project, current_directory)?;
    let scope = ResolvedScope::from_validated(super::plan::InstallScope::Project, root);
    prepare_update_in_scope(
        catalog,
        identifier,
        agent,
        &scope,
        store,
        transport,
        tempdir_factory,
    )
}

/// Prepare an update against one explicit, already validated scope root.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_update_in_scope<T: GithubTransport + ?Sized>(
    catalog: &Catalog,
    identifier: &str,
    agent: &str,
    scope: &ResolvedScope,
    store: &ReceiptStore,
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<UpdatePreparation, UpdateError> {
    let selected_agent = match agent {
        "codex" => crate::agents::AgentId::Codex,
        "claude-code" => crate::agents::AgentId::ClaudeCode,
        "cursor" => crate::agents::AgentId::Cursor,
        "gemini" => crate::agents::AgentId::Gemini,
        _ => return Err(UpdateError::UnsupportedAgent),
    };
    if selected_agent != scope.agent() {
        return Err(UpdateError::UnsupportedAgent);
    }
    let requested = SkillId::from_str(identifier).map_err(|_| UpdateError::NonCanonicalId)?;
    let catalog_identity = catalog
        .resources()
        .iter()
        .find(|resource| resource.id().lookup_key() == requested.lookup_key());
    if catalog_identity.is_some_and(|resource| resource.id().as_str() != identifier) {
        return Err(UpdateError::NonCanonicalId);
    }
    let resource = catalog
        .resources()
        .iter()
        .find(|resource| resource.id().as_str() == identifier)
        .ok_or_else(|| UpdateError::UnknownResource(identifier.to_owned()))?;
    let old = store
        .plan_update_target_in_scope(identifier, scope)
        .map_err(|error| match error {
            RemovalError::MissingReceipt => UpdateError::MissingReceipt,
            other => UpdateError::Removal(other),
        })?;

    validate_resource_identity(resource, old.receipt())?;
    if resource.source().commit().as_str() == old.receipt().source_commit() {
        return Ok(UpdatePreparation::AlreadyUpToDate(Box::new(
            old.receipt().clone(),
        )));
    }

    let old_receipt_sha256 = store
        .update_receipt_digest(&old)
        .map_err(|error| UpdateError::Store(error.to_string()))?;
    let receipt_transition = ReceiptTransitionPlan {
        active_receipt_name: old.receipt_name().to_owned(),
        old_receipt_file_identity: old.receipt_file_identity(),
        old_receipt_sha256,
        publication: ReceiptPublicationMode::BackupOldThenPublishNoReplace,
    };

    let staged = fetch_and_stage_skill_with_factory(resource, transport, tempdir_factory)?;
    let inventory = inspect_staged_skill(&staged)?;
    let destination = super::destination::inspect_destination_for_agent(
        scope.agent(),
        Some(&old.receipt().root_path()),
        scope.root(),
        staged.skill_name().as_str(),
    )?;
    if destination.project_root() != old.receipt().root_path()
        || destination.destination() != old.receipt().destination_path()
    {
        return Err(UpdateError::Conflict(
            "the selected project, agent, scope, or destination differs from the recorded installation".to_owned(),
        ));
    }
    let mut replacement = build_update_install_plan_for_agent_scope(
        &staged,
        &inventory,
        destination,
        scope.agent(),
        scope.scope(),
    )?;
    if scope.agent().adapter().discovers_shared_agents_skills() {
        let shared = super::destination::inspect_destination_for_agent(
            crate::agents::AgentId::Codex,
            Some(scope.root()),
            scope.root(),
            staged.skill_name().as_str(),
        )?;
        replacement.set_shared_path_snapshot(shared);
        if !replacement.shared_path_conflicts().is_empty() {
            let status = store
                .inspect_shared_skill(
                    scope.scope(),
                    scope.root(),
                    staged.skill_name().as_str(),
                    replacement.shared_path_snapshot().ok_or_else(|| {
                        UpdateError::Conflict("shared-path evidence is missing".to_owned())
                    })?,
                )
                .map_err(|error| UpdateError::Store(error.to_string()))?;
            let ownership = match status {
                super::receipt_store::SharedSkillStatus::ManagedByCodex => {
                    "A verified THRN-managed Codex skill may already be discoverable there; update/removal remain Codex-owned."
                }
                super::receipt_store::SharedSkillStatus::UnverifiedCodexReceipt => {
                    "A Codex receipt exists but the shared tree does not verify; preserve it and resolve through Codex."
                }
                super::receipt_store::SharedSkillStatus::Unmanaged => {
                    "The shared skill is unmanaged external content and remains untouched."
                }
                super::receipt_store::SharedSkillStatus::Absent
                | super::receipt_store::SharedSkillStatus::ChangedSincePlanning => {
                    "The shared path changed during inspection; it remains untouched."
                }
            };
            let precedence = if scope.agent() == crate::agents::AgentId::Gemini {
                "Gemini gives .agents/skills precedence over .gemini/skills."
            } else {
                "Cursor documents both paths but does not establish their same-name precedence."
            };
            return Err(UpdateError::Conflict(format!(
                "update is blocked by same-named skill in shared .agents/skills; {precedence} {ownership}"
            )));
        }
    }
    if !replacement.conflicts().is_empty() {
        return Err(UpdateError::Conflict(
            "destination parents contain an unexpected or ambiguous object".to_owned(),
        ));
    }
    let changes = compare_files(old.receipt(), &inventory);
    let operation_id = random_update_id().map_err(|error| {
        UpdateError::Conflict(format!(
            "could not allocate update identity ({})",
            error.kind()
        ))
    })?;
    Ok(UpdatePreparation::Ready(Box::new(UpdatePlan {
        old,
        staged,
        inventory,
        replacement,
        receipt_transition,
        changes,
        stage_name: format!(".thrn-update-{operation_id}-new"),
        backup_name: format!(".thrn-update-{operation_id}-old"),
        receipt_backup_name: format!(".thrn-update-{operation_id}-receipt-old"),
        operation_id,
        assumptions: vec![
            UpdateAssumption::ReceiptFileIdentityUnchanged,
            UpdateAssumption::ProjectAndDestinationIdentitiesUnchanged,
            UpdateAssumption::InstalledObjectInventoryUnchanged,
            UpdateAssumption::ReplacementStageDigestInventoryUnchanged,
            UpdateAssumption::BackupAndDestinationNamesRemainAbsent,
            UpdateAssumption::WriterLockRemainsExclusive,
        ],
    })))
}

fn validate_resource_identity(
    resource: &Resource,
    receipt: &InstallationReceipt,
) -> Result<(), UpdateError> {
    let source = resource.source();
    if resource.kind() != crate::models::catalog::ResourceKind::Skill
        || source.provider() != crate::models::catalog::SourceProvider::Github
        || resource.id().as_str() != receipt.catalog_resource_id()
        || resource.skill_name().as_str() != receipt.skill_name()
        || source.owner().as_str() != receipt.source_owner()
        || source.repository().as_str() != receipt.source_repository()
        || source.path().as_str() != receipt.source_directory()
    {
        return Err(UpdateError::Conflict(
            "the validated catalog resource changed its identity, GitHub owner/repository, source directory, or installed skill name".to_owned(),
        ));
    }
    Ok(())
}

fn compare_files(
    receipt: &InstallationReceipt,
    replacement: &StagedInventory,
) -> Vec<UpdateFileChange> {
    let mut old = receipt
        .files()
        .iter()
        .map(|file| {
            (
                file.path().to_owned(),
                (file.byte_count(), file.sha256().to_owned()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut changes = Vec::new();
    for file in replacement.files() {
        let path = file.relative_path().to_string_lossy().replace('\\', "/");
        match old.remove(&path) {
            None => changes.push(UpdateFileChange {
                path,
                kind: UpdateFileChangeKind::Added,
                old_bytes: None,
                new_bytes: Some(file.expected_bytes()),
            }),
            Some((old_bytes, old_hash)) => {
                let same = old_bytes == file.expected_bytes()
                    && old_hash == sha256_string(file.expected_sha256());
                changes.push(UpdateFileChange {
                    path,
                    kind: if same {
                        UpdateFileChangeKind::Unchanged
                    } else {
                        UpdateFileChangeKind::Changed
                    },
                    old_bytes: Some(old_bytes),
                    new_bytes: Some(file.expected_bytes()),
                });
            }
        }
    }
    for (path, (old_bytes, _)) in old {
        changes.push(UpdateFileChange {
            path,
            kind: UpdateFileChangeKind::Removed,
            old_bytes: Some(old_bytes),
            new_bytes: None,
        });
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

fn sha256_string(digest: &[u8]) -> String {
    let mut value = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

pub(crate) fn format_dry_run(plan: &UpdatePlan) -> String {
    let changes = plan.changes();
    let added = changes
        .iter()
        .filter(|item| item.kind == UpdateFileChangeKind::Added)
        .count();
    let removed = changes
        .iter()
        .filter(|item| item.kind == UpdateFileChangeKind::Removed)
        .count();
    let changed = changes
        .iter()
        .filter(|item| item.kind == UpdateFileChangeKind::Changed)
        .count();
    let unchanged = changes
        .iter()
        .filter(|item| item.kind == UpdateFileChangeKind::Unchanged)
        .count();
    let mut output = format!(
        "DRY RUN — no project or receipt-store files were modified\nResource: {}\nSkill name: {}\nAgent: {}\nScope: {}\nDestination: {}\nPrevious revision: {}\nNew revision: {}\nVerified ownership: versioned receipt and complete installed tree match\nFile changes:\n",
        plan.resource_id(),
        plan.skill_name(),
        plan.replacement().agent().as_str(),
        plan.replacement().scope().as_str(),
        safe_path(plan.destination()),
        plan.previous_revision(),
        plan.new_revision(),
    );
    for change in changes {
        let label = match change.kind {
            UpdateFileChangeKind::Added => "added",
            UpdateFileChangeKind::Removed => "removed",
            UpdateFileChangeKind::Changed => "changed",
            UpdateFileChangeKind::Unchanged => "unchanged",
        };
        let old_size = change
            .old_bytes
            .map(|bytes| format!("{bytes} bytes"))
            .unwrap_or_else(|| "absent".to_owned());
        let new_size = change
            .new_bytes
            .map(|bytes| format!("{bytes} bytes"))
            .unwrap_or_else(|| "absent".to_owned());
        output.push_str(&format!(
            "  {label}: {} ({old_size} -> {new_size})\n",
            change.path
        ));
    }
    output.push_str(&format!(
        "Totals: {added} added, {removed} removed, {changed} changed, {unchanged} unchanged\nPrevious: {} files, {} bytes\nReplacement: {} files, {} bytes\nExact pinned source may be fetched for this read-only comparison.\n",
        plan.old_file_count(),
        plan.old_byte_count(),
        plan.new_file_count(),
        plan.new_byte_count(),
    ));
    output
}

fn safe_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .flat_map(char::escape_debug)
        .collect()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum UpdateState {
    Prepared,
    ReplacementStaged,
    BackupRenamePending,
    OldBackedUp,
    NewCommitPending,
    NewCommitted,
    ReceiptTransitionPending,
    OldReceiptBackedUp,
    NewReceiptPublished,
    OldBackupCleanupInProgress,
    OldBackupRemoved,
    ReceiptBackupCleanupPending,
    Complete,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum UpdateOperationType {
    Replacement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct UpdateRecord {
    schema_version: u64,
    operation_type: UpdateOperationType,
    operation_id: String,
    sequence: u64,
    state: UpdateState,
    old_receipt: InstallationReceipt,
    new_receipt_intent: InstallationReceipt,
    new_receipt: Option<InstallationReceipt>,
    old_objects: Vec<RemovalObject>,
    receipt_directory_identity: RemovalIdentity,
    old_receipt_file_identity: RemovalIdentity,
    old_receipt_sha256: String,
    agents_identity: RemovalIdentity,
    skills_identity: RemovalIdentity,
    old_destination_identity: RemovalIdentity,
    stage_name: String,
    stage_identity: Option<RemovalIdentity>,
    stage_objects: Vec<RemovalObject>,
    backup_name: String,
    receipt_backup_name: String,
    receipt_temp_name: Option<String>,
    receipt_temp_identity: Option<RemovalIdentity>,
    new_receipt_file_identity: Option<RemovalIdentity>,
    cleanup_intents: BTreeSet<String>,
    receipt_backup_cleanup_intent: bool,
}

impl UpdateRecord {
    pub(super) fn receipt(&self) -> &InstallationReceipt {
        &self.old_receipt
    }

    fn validate(&self) -> Result<(), UpdateJournalError> {
        let user_scope = self.old_receipt.scope() == "user";
        let claude = self.old_receipt.agent() == "claude-code";
        let cursor_or_gemini = matches!(self.old_receipt.agent(), "cursor" | "gemini");
        let expected_journal_schema = if cursor_or_gemini {
            ADAPTER_UPDATE_JOURNAL_SCHEMA_VERSION
        } else if claude {
            CLAUDE_UPDATE_JOURNAL_SCHEMA_VERSION
        } else if user_scope {
            USER_UPDATE_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_UPDATE_JOURNAL_SCHEMA_VERSION
        };
        let expected_receipt_schema = if cursor_or_gemini {
            5
        } else if claude {
            4
        } else if user_scope {
            3
        } else {
            2
        };
        let expected_intent_schema = if cursor_or_gemini {
            5
        } else if claude {
            4
        } else if user_scope {
            3
        } else {
            1
        };
        if self.schema_version != expected_journal_schema
            || self.operation_type != UpdateOperationType::Replacement
            || !valid_hex(&self.operation_id, 32)
            || self.stage_name != format!(".thrn-update-{}-new", self.operation_id)
            || self.backup_name != format!(".thrn-update-{}-old", self.operation_id)
            || self.receipt_backup_name != format!(".thrn-update-{}-receipt-old", self.operation_id)
            || self.old_receipt.schema_version() != expected_receipt_schema
            || self.new_receipt_intent.schema_version() != expected_intent_schema
            || self.old_receipt.catalog_resource_id()
                != self.new_receipt_intent.catalog_resource_id()
            || self.old_receipt.skill_name() != self.new_receipt_intent.skill_name()
            || self.old_receipt.agent() != self.new_receipt_intent.agent()
            || self.old_receipt.scope() != self.new_receipt_intent.scope()
            || self.old_receipt.destination_path() != self.new_receipt_intent.destination_path()
            || self.old_receipt.root_path() != self.new_receipt_intent.root_path()
            || self.old_receipt.scope_root_identity()
                != self.new_receipt_intent.scope_root_identity()
            || self.old_receipt.source_owner() != self.new_receipt_intent.source_owner()
            || self.old_receipt.source_repository() != self.new_receipt_intent.source_repository()
            || self.old_receipt.source_directory() != self.new_receipt_intent.source_directory()
            || self.old_receipt.source_commit() == self.new_receipt_intent.source_commit()
            || self.old_receipt.receipt_filename().ok()
                != self.new_receipt_intent.receipt_filename().ok()
            || self.old_receipt.installed_directory_identity()
                != Some(FileIdentity::from(self.old_destination_identity))
            || !is_sha256_string(&self.old_receipt_sha256)
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        self.old_receipt
            .validate()
            .map_err(|_| UpdateJournalError::InvalidRecord)?;
        if !objects_match_receipt(&self.old_receipt, &self.old_objects) {
            return Err(UpdateJournalError::InvalidRecord);
        }
        self.new_receipt_intent
            .validate_install_intent()
            .map_err(|_| UpdateJournalError::InvalidRecord)?;
        if let Some(receipt) = &self.new_receipt {
            receipt
                .validate()
                .map_err(|_| UpdateJournalError::InvalidRecord)?;
            let Some(identity) = self.stage_identity else {
                return Err(UpdateJournalError::InvalidRecord);
            };
            if receipt.schema_version() != expected_receipt_schema
                || self
                    .new_receipt_intent
                    .clone()
                    .with_installed_directory_identity(FileIdentity::from(identity))
                    .map_or(true, |expected| expected != *receipt)
            {
                return Err(UpdateJournalError::InvalidRecord);
            }
        }
        let expects_stage = !matches!(self.state, UpdateState::Prepared | UpdateState::RolledBack);
        if expects_stage && (self.stage_identity.is_none() || self.stage_objects.is_empty()) {
            return Err(UpdateJournalError::InvalidRecord);
        }
        if self.stage_identity.is_some() != !self.stage_objects.is_empty() {
            return Err(UpdateJournalError::InvalidRecord);
        }
        if !self.stage_objects.is_empty()
            && !objects_match_receipt(&self.new_receipt_intent, &self.stage_objects)
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        let transition_state = matches!(
            self.state,
            UpdateState::ReceiptTransitionPending
                | UpdateState::OldReceiptBackedUp
                | UpdateState::NewReceiptPublished
                | UpdateState::OldBackupCleanupInProgress
                | UpdateState::OldBackupRemoved
                | UpdateState::ReceiptBackupCleanupPending
                | UpdateState::Complete
        );
        if transition_state
            && (self.new_receipt.is_none()
                || self.receipt_temp_name.as_deref()
                    != Some(format!(".pending-update-{}", self.operation_id).as_str())
                || self.receipt_temp_identity.is_none()
                || self.new_receipt_file_identity != self.receipt_temp_identity)
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        if self.receipt_backup_cleanup_intent
            && !matches!(
                self.state,
                UpdateState::ReceiptBackupCleanupPending | UpdateState::Complete
            )
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        let old_keys = self
            .old_receipt
            .files()
            .iter()
            .map(|file| format!("file:{}", file.path()))
            .chain(
                self.old_receipt
                    .directories()
                    .iter()
                    .map(|path| format!("directory:{path}")),
            )
            .chain(std::iter::once("root".to_owned()))
            .collect::<BTreeSet<_>>();
        if self
            .cleanup_intents
            .iter()
            .any(|key| !old_keys.contains(key))
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        if matches!(
            self.state,
            UpdateState::OldBackupRemoved
                | UpdateState::ReceiptBackupCleanupPending
                | UpdateState::Complete
        ) && self.cleanup_intents != old_keys
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        if self.state == UpdateState::RolledBack
            && (!self.cleanup_intents.is_empty() || self.receipt_backup_cleanup_intent)
        {
            return Err(UpdateJournalError::InvalidRecord);
        }
        Ok(())
    }
}

fn objects_match_receipt(receipt: &InstallationReceipt, objects: &[RemovalObject]) -> bool {
    let mut expected = BTreeMap::<String, (RemovalObjectKind, u64, Option<String>)>::new();
    expected.insert("root".to_owned(), (RemovalObjectKind::Root, 0, None));
    for path in receipt.directories() {
        expected.insert(
            format!("directory:{path}"),
            (RemovalObjectKind::Directory, 0, None),
        );
    }
    for file in receipt.files() {
        expected.insert(
            format!("file:{}", file.path()),
            (
                RemovalObjectKind::File,
                file.byte_count(),
                Some(file.sha256().to_owned()),
            ),
        );
    }
    if expected.len() != objects.len() {
        return false;
    }
    let mut actual = BTreeSet::new();
    objects.iter().all(|object| {
        let key = object.key();
        expected.get(&key).is_some_and(|(kind, bytes, digest)| {
            object.kind == *kind
                && object.byte_count == *bytes
                && object.sha256 == *digest
                && actual.insert(key)
        })
    })
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256_string(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| valid_hex(hex, 64))
}

#[derive(Debug, Error)]
pub(super) enum UpdateJournalError {
    #[error("update journal directory is unsafe")]
    UnsafeDirectory,
    #[error("update journal contains an unsupported or malformed object")]
    UnsafeObject,
    #[error("update journal is corrupt: {0}")]
    Corrupt(String),
    #[error("update journal exceeds a configured size or entry bound")]
    Oversized,
    #[error("update journal record is invalid")]
    InvalidRecord,
    #[error("update journal sequence is incomplete or duplicated")]
    Sequence,
    #[error("update journal transition is invalid")]
    Transition,
    #[error("update journal write failed during {action} ({kind})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
    },
    #[error("update journal temporary-name attempts were exhausted")]
    TempNameExhausted,
}

fn update_journal_io(action: &'static str, error: io::Error) -> UpdateJournalError {
    UpdateJournalError::Io {
        action,
        kind: error.kind(),
    }
}

fn update_journal_filename(operation_id: &str, sequence: u64) -> String {
    format!("{operation_id}-{sequence:08}.json")
}

fn parse_update_journal_filename(name: &str) -> Option<(&str, u64)> {
    let (operation_id, tail) = name.split_once('-')?;
    let sequence = tail.strip_suffix(".json")?;
    (valid_hex(operation_id, 32) && sequence.len() == 8)
        .then(|| sequence.parse::<u64>().ok())
        .flatten()
        .map(|sequence| (operation_id, sequence))
}

fn random_update_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("operating-system random source unavailable"))?;
    let mut value = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(value)
}

#[derive(Clone)]
pub(crate) struct UpdateJournalStore {
    directory: PathBuf,
}

impl UpdateJournalStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn prepare(
        &self,
        plan: &UpdatePlan,
        new_receipt_intent: InstallationReceipt,
    ) -> Result<UpdateJournal, UpdateJournalError> {
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| update_journal_io("create update journal storage", error))?;
        let operation_id = plan.operation_id.clone();
        let schema_version = if matches!(plan.old.receipt().agent(), "cursor" | "gemini") {
            ADAPTER_UPDATE_JOURNAL_SCHEMA_VERSION
        } else if plan.old.receipt().agent() == "claude-code" {
            CLAUDE_UPDATE_JOURNAL_SCHEMA_VERSION
        } else if plan.old.receipt().scope() == "user" {
            USER_UPDATE_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_UPDATE_JOURNAL_SCHEMA_VERSION
        };
        let record = UpdateRecord {
            schema_version,
            operation_type: UpdateOperationType::Replacement,
            operation_id: operation_id.clone(),
            sequence: 0,
            state: UpdateState::Prepared,
            old_receipt: plan.receipt().clone(),
            new_receipt_intent,
            new_receipt: None,
            old_objects: plan.old.objects().to_vec(),
            receipt_directory_identity: plan.old.receipt_directory_identity().into(),
            old_receipt_file_identity: plan.old.receipt_file_identity().into(),
            old_receipt_sha256: plan.receipt_transition.old_receipt_sha256.clone(),
            agents_identity: plan.old.agents_identity().into(),
            skills_identity: plan.old.skills_identity().into(),
            old_destination_identity: plan.old.destination_identity().into(),
            stage_name: plan.stage_name.clone(),
            stage_identity: None,
            stage_objects: Vec::new(),
            backup_name: plan.backup_name.clone(),
            receipt_backup_name: plan.receipt_backup_name.clone(),
            receipt_temp_name: None,
            receipt_temp_identity: None,
            new_receipt_file_identity: None,
            cleanup_intents: BTreeSet::new(),
            receipt_backup_cleanup_intent: false,
        };
        record.validate()?;
        self.persist(&directory, &record)?;
        Ok(UpdateJournal {
            store: self.clone(),
            directory,
            record,
        })
    }

    fn resume(&self, record: UpdateRecord) -> Result<UpdateJournal, UpdateJournalError> {
        record.validate()?;
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| update_journal_io("open update journal storage", error))?;
        Ok(UpdateJournal {
            store: self.clone(),
            directory,
            record,
        })
    }

    pub(super) fn pending_count(&self) -> Result<usize, UpdateJournalError> {
        Ok(self.read_all()?.len())
    }

    pub(super) fn read_all(&self) -> Result<Vec<UpdateRecord>, UpdateJournalError> {
        if !self.directory.is_absolute() {
            return Err(UpdateJournalError::UnsafeDirectory);
        }
        let directory = match SecureDir::open_absolute(&self.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(update_journal_io("open update journals", error)),
        };
        let entries = directory
            .entries()
            .map_err(|error| update_journal_io("read update journals", error))?;
        if entries.len() > MAX_UPDATE_JOURNAL_FILES {
            return Err(UpdateJournalError::Oversized);
        }
        let mut groups = BTreeMap::<String, BTreeMap<u64, UpdateRecord>>::new();
        let mut total = 0_u64;
        for (name, kind) in entries {
            let name = name.to_str().ok_or(UpdateJournalError::UnsafeObject)?;
            if name.starts_with(".pending-") {
                return Err(UpdateJournalError::UnsafeObject);
            }
            let (operation_id, sequence) =
                parse_update_journal_filename(name).ok_or(UpdateJournalError::UnsafeObject)?;
            if kind != NodeKind::RegularFile {
                return Err(UpdateJournalError::UnsafeObject);
            }
            let mut file = directory
                .open_regular_file(name)
                .map_err(|error| update_journal_io("open update journal record", error))?;
            let length = file
                .metadata()
                .map_err(|error| update_journal_io("inspect update journal record", error))?
                .len();
            if length > MAX_UPDATE_JOURNAL_BYTES as u64 {
                return Err(UpdateJournalError::Oversized);
            }
            total = total
                .checked_add(length)
                .filter(|value| *value <= MAX_UPDATE_JOURNAL_TOTAL_BYTES)
                .ok_or(UpdateJournalError::Oversized)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_UPDATE_JOURNAL_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| update_journal_io("read update journal record", error))?;
            let record: UpdateRecord = serde_json::from_slice(&bytes)
                .map_err(|_| UpdateJournalError::Corrupt(name.to_owned()))?;
            record.validate()?;
            if record.operation_id != operation_id || record.sequence != sequence {
                return Err(UpdateJournalError::Corrupt(name.to_owned()));
            }
            if groups
                .entry(operation_id.to_owned())
                .or_default()
                .insert(sequence, record)
                .is_some()
            {
                return Err(UpdateJournalError::Sequence);
            }
        }
        let mut latest = Vec::new();
        for (_, records) in groups {
            let mut previous: Option<(u64, &UpdateRecord)> = None;
            for (sequence, record) in &records {
                if let Some((prior_sequence, prior)) = previous {
                    if *sequence == prior_sequence + 1 {
                        if !valid_update_successor(prior, record) {
                            return Err(UpdateJournalError::Transition);
                        }
                    } else if !is_update_terminal(records.last_key_value().map(|(_, r)| r)) {
                        return Err(UpdateJournalError::Sequence);
                    } else if !same_update_payload(prior, record) {
                        return Err(UpdateJournalError::Transition);
                    }
                } else if !is_update_terminal(records.last_key_value().map(|(_, r)| r))
                    && (*sequence != 0 || record.state != UpdateState::Prepared)
                {
                    return Err(UpdateJournalError::Sequence);
                }
                previous = Some((*sequence, record));
            }
            latest.push(
                records
                    .into_iter()
                    .next_back()
                    .map(|(_, record)| record)
                    .ok_or(UpdateJournalError::Sequence)?,
            );
        }
        latest.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        Ok(latest)
    }

    fn persist(
        &self,
        directory: &SecureDir,
        record: &UpdateRecord,
    ) -> Result<(), UpdateJournalError> {
        record.validate()?;
        let bytes = serde_json::to_vec(record).map_err(|_| UpdateJournalError::InvalidRecord)?;
        if bytes.len() > MAX_UPDATE_JOURNAL_BYTES {
            return Err(UpdateJournalError::Oversized);
        }
        let mut name = String::new();
        let mut file = None;
        for _ in 0..32 {
            let candidate = format!(".pending-{}-{}", record.operation_id, record.sequence);
            match directory.create_regular_file(&candidate) {
                Ok(created) => {
                    name = candidate;
                    file = Some(created);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(update_journal_io("create update journal temporary", error));
                }
            }
        }
        let mut file = file.ok_or(UpdateJournalError::TempNameExhausted)?;
        let identity = SecureDir::file_identity(&file)
            .map_err(|error| update_journal_io("identify update journal temporary", error))?;
        file.write_all(&bytes)
            .map_err(|error| update_journal_io("write update journal", error))?;
        file.flush()
            .map_err(|error| update_journal_io("flush update journal", error))?;
        file.sync_all()
            .map_err(|error| update_journal_io("synchronize update journal", error))?;
        let mut verify = directory
            .open_regular_file(&name)
            .map_err(|error| update_journal_io("verify update journal", error))?;
        let mut actual = Vec::new();
        Read::by_ref(&mut verify)
            .take(MAX_UPDATE_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| update_journal_io("verify update journal", error))?;
        let parsed: UpdateRecord = serde_json::from_slice(&actual)
            .map_err(|_| UpdateJournalError::Corrupt(name.clone()))?;
        parsed.validate()?;
        if parsed != *record
            || SecureDir::file_identity(&verify)
                .map_err(|error| update_journal_io("identify verified update journal", error))?
                != identity
        {
            return Err(UpdateJournalError::Corrupt(name));
        }
        let final_name = update_journal_filename(&record.operation_id, record.sequence);
        directory
            .rename_file_child_noreplace(&name, &final_name, &file)
            .map_err(|error| update_journal_io("publish update journal", error))?;
        directory
            .sync_directory()
            .map_err(|error| update_journal_io("synchronize update journal directory", error))
    }

    fn cleanup_completed(&self, operation_id: &str) -> Result<(), UpdateJournalError> {
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| update_journal_io("open completed update journal", error))?;
        let mut entries = directory
            .entries()
            .map_err(|error| update_journal_io("read completed update journal", error))?
            .into_iter()
            .filter_map(|(name, kind)| {
                let name = name.to_str()?.to_owned();
                let (id, sequence) = parse_update_journal_filename(&name)?;
                (id == operation_id).then_some((sequence, name, kind))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|item| item.0);
        let Some((sequence, name, kind)) = entries.last().cloned() else {
            return Ok(());
        };
        if kind != NodeKind::RegularFile {
            return Err(UpdateJournalError::UnsafeObject);
        }
        let mut file = directory
            .open_regular_file(&name)
            .map_err(|error| update_journal_io("open terminal update journal", error))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_UPDATE_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| update_journal_io("read terminal update journal", error))?;
        let record: UpdateRecord = serde_json::from_slice(&bytes)
            .map_err(|_| UpdateJournalError::Corrupt(name.clone()))?;
        record.validate()?;
        if record.operation_id != operation_id
            || record.sequence != sequence
            || !is_update_terminal(Some(&record))
        {
            return Err(UpdateJournalError::Transition);
        }
        for (_, prior_name, prior_kind) in entries.iter().filter(|entry| entry.1 != name) {
            if *prior_kind != NodeKind::RegularFile {
                return Err(UpdateJournalError::UnsafeObject);
            }
            directory
                .remove_file(prior_name)
                .map_err(|error| update_journal_io("remove completed update journal", error))?;
        }
        if entries.len() > 1 {
            directory.sync_directory().map_err(|error| {
                update_journal_io("synchronize completed update journal", error)
            })?;
        }
        directory
            .remove_file(&name)
            .map_err(|error| update_journal_io("remove terminal update journal", error))?;
        directory
            .sync_directory()
            .map_err(|error| update_journal_io("synchronize terminal update journal", error))
    }
}

fn is_update_terminal(record: Option<&UpdateRecord>) -> bool {
    record.is_some_and(|record| {
        matches!(
            record.state,
            UpdateState::Complete | UpdateState::RolledBack
        )
    })
}

fn same_update_payload(previous: &UpdateRecord, current: &UpdateRecord) -> bool {
    previous.schema_version == current.schema_version
        && previous.operation_type == current.operation_type
        && previous.operation_id == current.operation_id
        && previous.old_receipt == current.old_receipt
        && previous.new_receipt_intent == current.new_receipt_intent
        && previous.old_objects == current.old_objects
        && previous.receipt_directory_identity == current.receipt_directory_identity
        && previous.old_receipt_file_identity == current.old_receipt_file_identity
        && previous.old_receipt_sha256 == current.old_receipt_sha256
        && previous.agents_identity == current.agents_identity
        && previous.skills_identity == current.skills_identity
        && previous.old_destination_identity == current.old_destination_identity
        && previous.stage_name == current.stage_name
        && previous.backup_name == current.backup_name
        && previous.receipt_backup_name == current.receipt_backup_name
}

fn valid_update_successor(previous: &UpdateRecord, current: &UpdateRecord) -> bool {
    previous.sequence.checked_add(1) == Some(current.sequence)
        && valid_update_transition(previous.state, current.state)
        && same_update_payload(previous, current)
        && previous
            .stage_identity
            .is_none_or(|identity| current.stage_identity == Some(identity))
        && (previous.stage_objects.is_empty() || previous.stage_objects == current.stage_objects)
        && previous
            .new_receipt
            .as_ref()
            .is_none_or(|receipt| current.new_receipt.as_ref() == Some(receipt))
        && previous
            .receipt_temp_name
            .as_ref()
            .is_none_or(|name| current.receipt_temp_name.as_ref() == Some(name))
        && previous
            .receipt_temp_identity
            .is_none_or(|identity| current.receipt_temp_identity == Some(identity))
        && previous
            .new_receipt_file_identity
            .is_none_or(|identity| current.new_receipt_file_identity == Some(identity))
        && previous
            .cleanup_intents
            .iter()
            .all(|key| current.cleanup_intents.contains(key))
        && (!previous.receipt_backup_cleanup_intent || current.receipt_backup_cleanup_intent)
}

fn valid_update_transition(from: UpdateState, to: UpdateState) -> bool {
    from == to
        || matches!(
            (from, to),
            (UpdateState::Prepared, UpdateState::ReplacementStaged)
                | (UpdateState::Prepared, UpdateState::RolledBack)
                | (
                    UpdateState::ReplacementStaged,
                    UpdateState::BackupRenamePending
                )
                | (UpdateState::BackupRenamePending, UpdateState::OldBackedUp)
                | (UpdateState::OldBackedUp, UpdateState::NewCommitPending)
                | (UpdateState::NewCommitPending, UpdateState::NewCommitted)
                | (
                    UpdateState::NewCommitted,
                    UpdateState::ReceiptTransitionPending
                )
                | (
                    UpdateState::ReceiptTransitionPending,
                    UpdateState::OldReceiptBackedUp
                )
                | (
                    UpdateState::ReceiptTransitionPending,
                    UpdateState::NewReceiptPublished
                )
                | (
                    UpdateState::OldReceiptBackedUp,
                    UpdateState::NewReceiptPublished
                )
                | (
                    UpdateState::NewReceiptPublished,
                    UpdateState::OldBackupCleanupInProgress
                )
                | (
                    UpdateState::OldBackupCleanupInProgress,
                    UpdateState::OldBackupRemoved
                )
                | (
                    UpdateState::OldBackupRemoved,
                    UpdateState::ReceiptBackupCleanupPending
                )
                | (
                    UpdateState::ReceiptBackupCleanupPending,
                    UpdateState::Complete
                )
        )
}

struct UpdateJournal {
    store: UpdateJournalStore,
    directory: SecureDir,
    record: UpdateRecord,
}

impl UpdateJournal {
    fn record(&self) -> &UpdateRecord {
        &self.record
    }

    fn set_stage(
        &mut self,
        identity: FileIdentity,
        objects: Vec<RemovalObject>,
    ) -> Result<(), UpdateJournalError> {
        self.record.stage_identity = Some(identity.into());
        self.record.stage_objects = objects;
        self.set_state(UpdateState::ReplacementStaged)
    }

    fn set_new_receipt(&mut self, receipt: InstallationReceipt) -> Result<(), UpdateJournalError> {
        self.record.new_receipt = Some(receipt);
        self.set_state(UpdateState::NewCommitted)
    }

    fn set_receipt_temporary(
        &mut self,
        name: String,
        identity: FileIdentity,
        receipt: InstallationReceipt,
    ) -> Result<(), UpdateJournalError> {
        self.record.receipt_temp_name = Some(name);
        self.record.receipt_temp_identity = Some(identity.into());
        self.record.new_receipt_file_identity = Some(identity.into());
        self.record.new_receipt = Some(receipt);
        self.set_state(UpdateState::ReceiptTransitionPending)
    }

    fn record_cleanup_intent(&mut self, key: String) -> Result<(), UpdateJournalError> {
        self.record.cleanup_intents.insert(key);
        self.persist_update()
    }

    fn record_receipt_backup_cleanup_intent(&mut self) -> Result<(), UpdateJournalError> {
        self.record.receipt_backup_cleanup_intent = true;
        self.persist_update()
    }

    fn set_state(&mut self, state: UpdateState) -> Result<(), UpdateJournalError> {
        if !valid_update_transition(self.record.state, state) {
            return Err(UpdateJournalError::Transition);
        }
        self.record.state = state;
        self.persist_update()
    }

    fn persist_update(&mut self) -> Result<(), UpdateJournalError> {
        self.record.sequence = self
            .record
            .sequence
            .checked_add(1)
            .ok_or(UpdateJournalError::InvalidRecord)?;
        self.record.validate()?;
        self.store.persist(&self.directory, &self.record)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum UpdateOperation {
    CreateStage,
    CreateDirectory,
    CopyFile,
    SyncStagedContent,
    BackupRename,
    CommitReplacement,
    PrepareReceipt,
    ReceiptBackupRename,
    PublishReceipt,
    CleanupFile,
    CleanupDirectory,
    CleanupReceiptBackup,
}

pub(crate) trait UpdateFaultInjector: Send + Sync {
    fn before(&self, _operation: UpdateOperation) -> io::Result<()> {
        Ok(())
    }

    fn after(&self, _operation: UpdateOperation) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct NoUpdateFaults;
impl UpdateFaultInjector for NoUpdateFaults {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdateSummary {
    resource_id: String,
    skill_name: String,
    destination: PathBuf,
    previous_revision: String,
    new_revision: String,
    file_count: u64,
    byte_count: u64,
    receipt: InstallationReceipt,
}

impl UpdateSummary {
    pub(crate) fn agent(&self) -> &str {
        self.receipt.agent()
    }

    pub(crate) fn scope(&self) -> &str {
        self.receipt.scope()
    }

    pub(crate) fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub(crate) fn skill_name(&self) -> &str {
        &self.skill_name
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn previous_revision(&self) -> &str {
        &self.previous_revision
    }

    pub(crate) fn new_revision(&self) -> &str {
        &self.new_revision
    }

    pub(crate) fn file_count(&self) -> u64 {
        self.file_count
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

pub(super) fn execute_locked(
    plan: &UpdatePlan,
    receipt_directory: &SecureDir,
    writer_lock: &WriterLock,
    journal_store: &UpdateJournalStore,
    receipt_faults: &dyn ReceiptFaultInjector,
    faults: &dyn UpdateFaultInjector,
) -> Result<UpdateSummary, UpdateError> {
    revalidate_plan(plan, receipt_directory, writer_lock)?;
    let new_receipt_intent = InstallationReceipt::from_committed_plan(
        plan.replacement(),
        plan.inventory(),
        plan.receipt().scope_root_identity(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| UpdateError::Conflict("system clock is before the Unix epoch".to_owned()))?
            .as_secs(),
    )
    .map_err(|error| UpdateError::Conflict(format!("replacement receipt is invalid: {error}")))?;
    let mut journal = journal_store
        .prepare(plan, new_receipt_intent)
        .map_err(|error| {
            UpdateError::Store(format!("could not prepare update journal: {error}"))
        })?;
    let operation_id = journal.record().operation_id.clone();
    let stage_result = materialize_replacement(
        plan,
        &journal.record().stage_name,
        &journal.record().new_receipt_intent,
        faults,
    );
    let (stage_identity, stage_objects) = match stage_result {
        Ok(value) => value,
        Err(detail) => {
            return Err(UpdateError::Incomplete {
                operation_id,
                detail,
            });
        }
    };
    if let Err(error) = journal.set_stage(stage_identity, stage_objects) {
        return Err(UpdateError::Incomplete {
            operation_id,
            detail: format!(
                "replacement is staged but its identities could not be journaled: {error}"
            ),
        });
    }
    if let Err(detail) = drive_forward(
        &mut journal,
        receipt_directory,
        writer_lock,
        receipt_faults,
        faults,
    ) {
        return Err(UpdateError::Incomplete {
            operation_id,
            detail,
        });
    }
    let receipt = journal
        .record()
        .new_receipt
        .clone()
        .ok_or_else(|| UpdateError::Incomplete {
            operation_id: journal.record().operation_id.clone(),
            detail: "update completed without a recorded replacement receipt".to_owned(),
        })?;
    let summary = UpdateSummary {
        resource_id: plan.resource_id().to_owned(),
        skill_name: plan.skill_name().to_owned(),
        destination: plan.destination().to_path_buf(),
        previous_revision: plan.previous_revision().to_owned(),
        new_revision: plan.new_revision().to_owned(),
        file_count: plan.new_file_count(),
        byte_count: plan.new_byte_count(),
        receipt,
    };
    journal_store
        .cleanup_completed(&journal.record().operation_id)
        .map_err(|error| UpdateError::Incomplete {
            operation_id: journal.record().operation_id.clone(),
            detail: format!("replacement is recorded but update journal cleanup failed: {error}"),
        })?;
    Ok(summary)
}

fn revalidate_plan(
    plan: &UpdatePlan,
    receipt_directory: &SecureDir,
    writer_lock: &WriterLock,
) -> Result<(), UpdateError> {
    writer_lock
        .validate()
        .map_err(|error| UpdateError::RecoveryRequired(error.to_string()))?;
    if receipt_directory.identity().map_err(|error| {
        UpdateError::Conflict(format!(
            "could not identify receipt store: {}",
            error.kind()
        ))
    })? != plan.old.receipt_directory_identity()
    {
        return Err(UpdateError::Conflict(
            "receipt store identity changed after update planning".to_owned(),
        ));
    }
    let name = plan.old.receipt_name();
    let receipt = super::receipt_store::read_named_receipt(receipt_directory, name)
        .map_err(|error| UpdateError::Conflict(format!("ownership receipt changed: {error}")))?;
    let identity = super::receipt_store::receipt_file_identity(receipt_directory, name)
        .map_err(|error| UpdateError::Conflict(format!("ownership receipt changed: {error}")))?;
    let receipt_digest = super::receipt_store::receipt_file_sha256(receipt_directory, name)
        .map_err(|error| UpdateError::Conflict(format!("ownership receipt changed: {error}")))?;
    if receipt != *plan.receipt()
        || identity != plan.old.receipt_file_identity()
        || identity != plan.receipt_transition.old_receipt_file_identity
        || receipt_digest != plan.receipt_transition.old_receipt_sha256
        || plan.receipt_transition.active_receipt_name != name
    {
        return Err(UpdateError::Conflict(
            "ownership receipt changed after update planning".to_owned(),
        ));
    }
    let current = RemovalPlan::verify(
        receipt,
        name.to_owned(),
        plan.old.receipt_directory_identity(),
        plan.old.receipt_file_identity(),
    )?;
    if current != *plan.old_plan() {
        return Err(UpdateError::Conflict(
            "installed filesystem identities or content changed after update planning".to_owned(),
        ));
    }
    let current_inventory = inspect_staged_skill(plan.staged())?;
    if current_inventory != *plan.inventory() {
        return Err(UpdateError::Conflict(
            "validated replacement staging content changed after planning".to_owned(),
        ));
    }
    Ok(())
}

fn materialize_replacement(
    plan: &UpdatePlan,
    stage_name: &str,
    receipt_intent: &InstallationReceipt,
    faults: &dyn UpdateFaultInjector,
) -> Result<(FileIdentity, Vec<RemovalObject>), String> {
    let (_, _, skills) = open_plan_parents(plan.old_plan()).map_err(|error| error.to_string())?;
    faults
        .before(UpdateOperation::CreateStage)
        .map_err(|error| format!("could not create replacement stage ({})", error.kind()))?;
    skills
        .create_directory(stage_name)
        .map_err(|error| format!("could not create replacement stage ({})", error.kind()))?;
    faults
        .after(UpdateOperation::CreateStage)
        .map_err(|error| {
            format!(
                "replacement stage was created but setup reported failure ({})",
                error.kind()
            )
        })?;
    let root = skills
        .open_directory(std::ffi::OsStr::new(stage_name))
        .map_err(|error| format!("could not open replacement stage ({})", error.kind()))?;
    let root_identity = root
        .identity()
        .map_err(|error| format!("could not identify replacement stage ({})", error.kind()))?;
    let source_root = SecureDir::open_absolute(plan.staged().content_directory())
        .map_err(|error| format!("validated source stage became unsafe ({})", error.kind()))?;

    let mut directories = plan.inventory().directories().to_vec();
    directories.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    for relative in directories {
        let path = relative
            .to_str()
            .ok_or_else(|| "replacement directory path is not portable".to_owned())?;
        let (parent, name) = open_relative_parent(&root, path, true).map_err(|error| {
            format!("could not create replacement directory ({})", error.kind())
        })?;
        faults
            .before(UpdateOperation::CreateDirectory)
            .map_err(|error| {
                format!("could not create replacement directory ({})", error.kind())
            })?;
        parent.create_directory(&name).map_err(|error| {
            format!("could not create replacement directory ({})", error.kind())
        })?;
        faults
            .after(UpdateOperation::CreateDirectory)
            .map_err(|error| {
                format!(
                    "replacement directory creation reported failure ({})",
                    error.kind()
                )
            })?;
    }

    for expected in plan.inventory().files() {
        let relative = expected
            .relative_path()
            .to_str()
            .ok_or_else(|| "replacement file path is not portable".to_owned())?;
        let (destination_parent, name) =
            open_relative_parent(&root, relative, false).map_err(|error| {
                format!("could not open replacement file parent ({})", error.kind())
            })?;
        let (source_parent, source_name) = open_relative_parent(&source_root, relative, false)
            .map_err(|error| {
                format!("could not open validated source parent ({})", error.kind())
            })?;
        let mut source = source_parent
            .open_regular_file(&source_name)
            .map_err(|error| {
                format!(
                    "could not open validated source file without following links ({})",
                    error.kind()
                )
            })?;
        if SecureDir::hard_link_count(&source).map_err(|error| {
            format!(
                "could not inspect validated source link count ({})",
                error.kind()
            )
        })? != 1
        {
            return Err("validated replacement source file is hard-linked".to_owned());
        }
        faults.before(UpdateOperation::CopyFile).map_err(|error| {
            format!(
                "replacement file copy failed before writing ({})",
                error.kind()
            )
        })?;
        let mut target = destination_parent
            .create_regular_file(&name)
            .map_err(|error| format!("could not create replacement file ({})", error.kind()))?;
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut count = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(|error| {
                format!("could not read validated source file ({})", error.kind())
            })?;
            if read == 0 {
                break;
            }
            count = count
                .checked_add(read as u64)
                .filter(|value| *value <= expected.expected_bytes())
                .ok_or_else(|| "replacement source file changed while copying".to_owned())?;
            target
                .write_all(&buffer[..read])
                .map_err(|error| format!("could not write replacement file ({})", error.kind()))?;
            digest.update(&buffer[..read]);
        }
        if count != expected.expected_bytes()
            || digest.finish().as_ref() != expected.expected_sha256()
        {
            return Err("replacement file bytes differ from the validated inventory".to_owned());
        }
        target.sync_all().map_err(|error| {
            format!("could not synchronize replacement file ({})", error.kind())
        })?;
        faults.after(UpdateOperation::CopyFile).map_err(|error| {
            format!(
                "replacement file was written but copy reported failure ({})",
                error.kind()
            )
        })?;
    }

    sync_tree_directories(&root, faults)?;
    faults
        .before(UpdateOperation::SyncStagedContent)
        .map_err(|error| {
            format!(
                "could not synchronize replacement parent ({})",
                error.kind()
            )
        })?;
    skills.sync_directory().map_err(|error| {
        format!(
            "could not synchronize replacement parent ({})",
            error.kind()
        )
    })?;
    faults
        .after(UpdateOperation::SyncStagedContent)
        .map_err(|error| {
            format!(
                "replacement parent synchronization reported failure ({})",
                error.kind()
            )
        })?;
    let objects = super::removal::verify_inventory(receipt_intent, root).map_err(|error| {
        format!("replacement stage failed complete inventory verification: {error}")
    })?;
    Ok((root_identity, objects))
}

fn sync_tree_directories(root: &SecureDir, faults: &dyn UpdateFaultInjector) -> Result<(), String> {
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for (name, kind) in directory
            .entries()
            .map_err(|error| format!("could not inspect staged directory ({})", error.kind()))?
        {
            if kind == NodeKind::Directory {
                let child = directory.open_directory(&name).map_err(|error| {
                    format!("could not open staged directory ({})", error.kind())
                })?;
                stack.push(child);
            }
        }
        faults
            .before(UpdateOperation::SyncStagedContent)
            .map_err(|error| {
                format!("could not synchronize staged directory ({})", error.kind())
            })?;
        directory.sync_directory().map_err(|error| {
            format!("could not synchronize staged directory ({})", error.kind())
        })?;
        faults
            .after(UpdateOperation::SyncStagedContent)
            .map_err(|error| {
                format!("staged directory sync reported failure ({})", error.kind())
            })?;
    }
    Ok(())
}

fn open_relative_parent(
    root: &SecureDir,
    relative: &str,
    create_parents: bool,
) -> io::Result<(SecureDir, String)> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split(['/', '\\'])
            .any(|part| part.is_empty() || part == "." || part == "..")
        || Path::new(relative).is_absolute()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe relative path",
        ));
    }
    let mut components = relative.split('/').collect::<Vec<_>>();
    let name = components
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty relative path"))?
        .to_owned();
    let mut current = root.clone();
    for component in components {
        current = match current.open_directory(std::ffi::OsStr::new(component)) {
            Ok(child) => child,
            Err(error) if create_parents && error.kind() == io::ErrorKind::NotFound => {
                current.create_directory(component)?;
                current.open_directory(std::ffi::OsStr::new(component))?
            }
            Err(error) => return Err(error),
        };
    }
    Ok((current, name))
}

fn drive_forward(
    journal: &mut UpdateJournal,
    receipt_directory: &SecureDir,
    writer_lock: &WriterLock,
    receipt_faults: &dyn ReceiptFaultInjector,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    loop {
        writer_lock
            .validate()
            .map_err(|error| format!("receipt-store writer lock changed ({error})"))?;
        match journal.record().state {
            UpdateState::Prepared => {
                let record = journal.record().clone();
                let (_, _, skills) = open_update_parents(&record)?;
                verify_old_receipt_file_matches(
                    receipt_directory,
                    &record,
                    &record
                        .old_receipt
                        .receipt_filename()
                        .map_err(|error| error.to_string())?,
                )?;
                verify_old_at(&skills, &record, record.old_receipt.skill_name())?.ok_or_else(
                    || "original installation is missing before update preparation".to_owned(),
                )?;
                if open_directory_child(&skills, &record.stage_name)?.is_some()
                    || open_directory_child(&skills, &record.backup_name)?.is_some()
                    || open_file_child(receipt_directory, &record.receipt_backup_name)?.is_some()
                    || open_file_child(
                        receipt_directory,
                        &format!(".pending-update-{}", record.operation_id),
                    )?
                    .is_some()
                {
                    return Err(
                        "an unrecorded update stage, backup, or receipt artifact already exists"
                            .to_owned(),
                    );
                }
                journal
                    .set_state(UpdateState::RolledBack)
                    .map_err(|error| format!("could not retire unused update intent: {error}"))?;
            }
            UpdateState::ReplacementStaged => {
                journal
                    .set_state(UpdateState::BackupRenamePending)
                    .map_err(|error| {
                        format!("could not record the old-directory backup intent: {error}")
                    })?;
            }
            UpdateState::BackupRenamePending => {
                let record = journal.record().clone();
                ensure_adapter_shared_name_absent(&record.old_receipt)?;
                let (_, _, skills) = open_update_parents(&record)?;
                let stage = verify_stage_at(&skills, &record, &record.stage_name)?
                    .ok_or_else(|| "verified replacement stage is missing".to_owned())?;
                drop(stage);
                let original = verify_old_at(&skills, &record, record.old_receipt.skill_name())?;
                let backup = verify_old_at(&skills, &record, &record.backup_name)?;
                match (original, backup) {
                    (Some(_), None) => {
                        verify_old_receipt_file_matches(
                            receipt_directory,
                            &record,
                            &record
                                .old_receipt
                                .receipt_filename()
                                .map_err(|error| error.to_string())?,
                        )?;
                        faults
                            .before(UpdateOperation::BackupRename)
                            .map_err(|error| {
                                format!(
                                    "old installation backup rename failed before mutation ({})",
                                    error.kind()
                                )
                            })?;
                        skills
                            .rename_directory_child_noreplace(
                                record.old_receipt.skill_name(),
                                &record.backup_name,
                                FileIdentity::from(record.old_destination_identity),
                            )
                            .map_err(|error| {
                                format!(
                                    "could not move verified old installation to its backup ({})",
                                    error.kind()
                                )
                            })?;
                        faults
                            .after(UpdateOperation::BackupRename)
                            .map_err(|error| format!("old installation moved to backup but rename reported failure ({})", error.kind()))?;
                        skills.sync_directory().map_err(|error| {
                            format!(
                                "could not synchronize the skill parent after backup rename ({})",
                                error.kind()
                            )
                        })?;
                        verify_old_at(&skills, &record, &record.backup_name)?.ok_or_else(|| {
                            "old installation backup disappeared after rename".to_owned()
                        })?;
                        if open_directory_child(&skills, record.old_receipt.skill_name())?.is_some()
                        {
                            return Err("original destination remains present after backup rename"
                                .to_owned());
                        }
                    }
                    (None, Some(_)) => {}
                    _ => return Err(
                        "original destination and old backup are missing, duplicated, or ambiguous"
                            .to_owned(),
                    ),
                }
                journal
                    .set_state(UpdateState::OldBackedUp)
                    .map_err(|error| {
                        format!("old installation is backed up but journal update failed: {error}")
                    })?;
            }
            UpdateState::OldBackedUp => {
                journal
                    .set_state(UpdateState::NewCommitPending)
                    .map_err(|error| {
                        format!("could not record replacement commit intent: {error}")
                    })?;
            }
            UpdateState::NewCommitPending => {
                let record = journal.record().clone();
                ensure_adapter_shared_name_absent(&record.old_receipt)?;
                let (_, _, skills) = open_update_parents(&record)?;
                verify_old_at(&skills, &record, &record.backup_name)?
                    .ok_or_else(|| "verified old installation backup is missing".to_owned())?;
                let stage = verify_stage_at(&skills, &record, &record.stage_name)?;
                let destination =
                    verify_stage_at(&skills, &record, record.old_receipt.skill_name())?;
                match (stage, destination) {
                    (Some(stage), None) => {
                        let stage_identity = stage.identity().map_err(|error| {
                            format!("could not identify replacement stage ({})", error.kind())
                        })?;
                        drop(stage);
                        if stage_identity
                            != FileIdentity::from(record.stage_identity.ok_or_else(|| {
                                "replacement stage identity is missing".to_owned()
                            })?)
                        {
                            return Err(
                                "replacement stage identity changed before commitment".to_owned()
                            );
                        }
                        faults
                            .before(UpdateOperation::CommitReplacement)
                            .map_err(|error| {
                                format!(
                                    "replacement directory commit failed before mutation ({})",
                                    error.kind()
                                )
                            })?;
                        skills
                            .rename_directory_child_noreplace(
                                &record.stage_name,
                                record.old_receipt.skill_name(),
                                stage_identity,
                            )
                            .map_err(|error| format!("replacement directory could not be committed without replacement ({})", error.kind()))?;
                        faults
                            .after(UpdateOperation::CommitReplacement)
                            .map_err(|error| format!("replacement directory committed but rename reported failure ({})", error.kind()))?;
                        skills
                            .sync_directory()
                            .map_err(|error| format!("could not synchronize the skill parent after replacement commit ({})", error.kind()))?;
                    }
                    (None, Some(_)) => {}
                    _ => {
                        return Err(
                            "replacement stage and destination state is missing or ambiguous"
                                .to_owned(),
                        );
                    }
                }
                let destination =
                    verify_stage_at(&skills, &record, record.old_receipt.skill_name())?
                        .ok_or_else(|| {
                            "replacement destination is missing after commitment".to_owned()
                        })?;
                let identity = destination.identity().map_err(|error| {
                    format!(
                        "could not identify committed replacement ({})",
                        error.kind()
                    )
                })?;
                let expected = FileIdentity::from(
                    record
                        .stage_identity
                        .ok_or_else(|| "replacement stage identity is missing".to_owned())?,
                );
                if identity != expected {
                    return Err(
                        "committed replacement identity differs from its verified stage".to_owned(),
                    );
                }
                let receipt = record
                    .new_receipt_intent
                    .clone()
                    .with_installed_directory_identity(identity)
                    .map_err(|error| {
                        format!("could not construct schema-v2 replacement receipt: {error}")
                    })?;
                journal
                    .set_new_receipt(receipt)
                    .map_err(|error| format!("replacement is committed but receipt intent could not be journaled: {error}"))?;
            }
            UpdateState::NewCommitted => {
                let record = journal.record().clone();
                verify_new_destination(&record)?;
                verify_old_receipt_file_matches(
                    receipt_directory,
                    &record,
                    &record
                        .old_receipt
                        .receipt_filename()
                        .map_err(|error| error.to_string())?,
                )?;
                let temp_name = format!(".pending-update-{}", record.operation_id);
                if open_file_child(receipt_directory, &temp_name)?.is_some() {
                    return Err("an unrecorded update receipt temporary exists; preserving it for inspection".to_owned());
                }
                faults
                    .before(UpdateOperation::PrepareReceipt)
                    .map_err(|error| {
                        format!("could not prepare replacement receipt ({})", error.kind())
                    })?;
                let new_receipt = record.new_receipt.as_ref().ok_or_else(|| {
                    "replacement receipt is missing from the update journal".to_owned()
                })?;
                let (name, identity) = super::receipt_store::prepare_transition_receipt_temp(
                    receipt_directory,
                    &record.operation_id,
                    new_receipt,
                    receipt_faults,
                )
                .map_err(|error| {
                    format!("could not prepare durable replacement receipt: {error}")
                })?;
                faults
                    .after(UpdateOperation::PrepareReceipt)
                    .map_err(|error| {
                        format!(
                            "replacement receipt is durable but preparation reported failure ({})",
                            error.kind()
                        )
                    })?;
                journal
                    .set_receipt_temporary(name, identity, new_receipt.clone())
                    .map_err(|error| format!("replacement receipt is durable but transition identities could not be journaled: {error}"))?;
            }
            UpdateState::ReceiptTransitionPending | UpdateState::OldReceiptBackedUp => {
                complete_receipt_transition(
                    journal,
                    receipt_directory,
                    writer_lock,
                    receipt_faults,
                    faults,
                )?;
            }
            UpdateState::NewReceiptPublished => {
                let record = journal.record().clone();
                verify_active_new_installation(&record, receipt_directory)?;
                verify_old_at(
                    &open_update_parents(&record)?.2,
                    &record,
                    &record.backup_name,
                )?
                .ok_or_else(|| "verified old backup is missing before cleanup".to_owned())?;
                journal
                    .set_state(UpdateState::OldBackupCleanupInProgress)
                    .map_err(|error| {
                        format!("could not record old-backup cleanup phase: {error}")
                    })?;
            }
            UpdateState::OldBackupCleanupInProgress => {
                let record = journal.record().clone();
                verify_active_new_installation(&record, receipt_directory)?;
                cleanup_old_backup(journal, faults)?;
                journal
                    .set_state(UpdateState::OldBackupRemoved)
                    .map_err(|error| {
                        format!("old backup was cleaned but journal update failed: {error}")
                    })?;
            }
            UpdateState::OldBackupRemoved => {
                let record = journal.record().clone();
                verify_active_new_installation(&record, receipt_directory)?;
                if open_directory_child(&open_update_parents(&record)?.2, &record.backup_name)?
                    .is_some()
                {
                    return Err("old backup remains after cleanup was recorded".to_owned());
                }
                journal
                    .set_state(UpdateState::ReceiptBackupCleanupPending)
                    .map_err(|error| {
                        format!("could not record old receipt cleanup phase: {error}")
                    })?;
            }
            UpdateState::ReceiptBackupCleanupPending => {
                let record = journal.record().clone();
                verify_active_new_installation(&record, receipt_directory)?;
                cleanup_old_receipt_backup(journal, receipt_directory, faults)?;
                verify_active_new_installation(journal.record(), receipt_directory)?;
                journal
                    .set_state(UpdateState::Complete)
                    .map_err(|error| format!("replacement and receipt are committed but completion record failed: {error}"))?;
            }
            UpdateState::Complete => {
                let record = journal.record().clone();
                verify_active_new_installation(&record, receipt_directory)?;
                let (_, _, skills) = open_update_parents(&record)?;
                if open_directory_child(&skills, &record.backup_name)?.is_some()
                    || open_directory_child(&skills, &record.stage_name)?.is_some()
                    || open_file_child(receipt_directory, &record.receipt_backup_name)?.is_some()
                    || open_file_child(
                        receipt_directory,
                        record
                            .receipt_temp_name
                            .as_deref()
                            .ok_or_else(|| "receipt temp name missing".to_owned())?,
                    )?
                    .is_some()
                    || !record.receipt_backup_cleanup_intent
                    || record.cleanup_intents.len() != record.old_objects.len()
                {
                    return Err("completed update still has a backup, staging object, or incomplete cleanup evidence".to_owned());
                }
                return Ok(());
            }
            UpdateState::RolledBack => {
                let record = journal.record().clone();
                let (_, _, skills) = open_update_parents(&record)?;
                verify_old_at(&skills, &record, record.old_receipt.skill_name())?
                    .ok_or_else(|| "rolled-back installation is missing".to_owned())?;
                verify_old_receipt_file_matches(
                    receipt_directory,
                    &record,
                    &record
                        .old_receipt
                        .receipt_filename()
                        .map_err(|error| error.to_string())?,
                )?;
                if open_directory_child(&skills, &record.stage_name)?.is_some()
                    || open_directory_child(&skills, &record.backup_name)?.is_some()
                    || open_file_child(receipt_directory, &record.receipt_backup_name)?.is_some()
                    || open_file_child(
                        receipt_directory,
                        &format!(".pending-update-{}", record.operation_id),
                    )?
                    .is_some()
                {
                    return Err(
                        "rolled-back update still has an operation-owned artifact".to_owned()
                    );
                }
                return Ok(());
            }
        }
    }
}

fn complete_receipt_transition(
    journal: &mut UpdateJournal,
    receipt_directory: &SecureDir,
    writer_lock: &WriterLock,
    receipt_faults: &dyn ReceiptFaultInjector,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let record = journal.record().clone();
    let receipt_name = record
        .new_receipt
        .as_ref()
        .ok_or_else(|| "replacement receipt is missing".to_owned())?
        .receipt_filename()
        .map_err(|error| error.to_string())?;
    let old_identity = FileIdentity::from(record.old_receipt_file_identity);
    let new_identity = FileIdentity::from(
        record
            .receipt_temp_identity
            .ok_or_else(|| "replacement receipt temporary identity is missing".to_owned())?,
    );
    let old_backup =
        verify_old_receipt_file_optional(receipt_directory, &record, &record.receipt_backup_name)?;
    let active_file = open_file_child(receipt_directory, &receipt_name)?;
    if let Some(file) = active_file.as_ref() {
        let identity = SecureDir::file_identity(file).map_err(|error| {
            format!(
                "could not identify active ownership receipt ({})",
                error.kind()
            )
        })?;
        let active = super::receipt_store::read_named_receipt(receipt_directory, &receipt_name)
            .map_err(|error| format!("active ownership receipt is invalid: {error}"))?;
        if active
            == *record
                .new_receipt
                .as_ref()
                .ok_or_else(|| "replacement receipt missing".to_owned())?
            && identity == new_identity
        {
            if !old_backup {
                return Err(
                    "new receipt is active without the verified old receipt backup".to_owned(),
                );
            }
            if journal.record().state != UpdateState::NewReceiptPublished {
                journal
                    .set_state(UpdateState::NewReceiptPublished)
                    .map_err(|error| {
                        format!("new receipt is active but journal update failed: {error}")
                    })?;
            }
            return Ok(());
        }
        if active != record.old_receipt
            || identity != old_identity
            || super::receipt_store::receipt_file_sha256(receipt_directory, &receipt_name)
                .map_err(|error| format!("could not verify old receipt bytes: {error}"))?
                != record.old_receipt_sha256
            || old_backup
        {
            return Err(
                "active receipt conflicts with the journaled old/new receipt transition".to_owned(),
            );
        }
    } else if !old_backup {
        return Err(
            "both the active receipt and its verified transition backup are missing".to_owned(),
        );
    }
    drop(active_file);

    if !old_backup {
        let active_identity =
            super::receipt_store::receipt_file_identity(receipt_directory, &receipt_name).map_err(
                |error| format!("could not verify old receipt before transition: {error}"),
            )?;
        if active_identity != old_identity
            || super::receipt_store::read_named_receipt(receipt_directory, &receipt_name)
                .map_err(|error| format!("could not read old receipt before transition: {error}"))?
                != record.old_receipt
            || super::receipt_store::receipt_file_sha256(receipt_directory, &receipt_name)
                .map_err(|error| format!("could not verify old receipt bytes: {error}"))?
                != record.old_receipt_sha256
        {
            return Err("old receipt changed before transition".to_owned());
        }
        faults
            .before(UpdateOperation::ReceiptBackupRename)
            .map_err(|error| {
                format!(
                    "old receipt backup rename failed before mutation ({})",
                    error.kind()
                )
            })?;
        let active = receipt_directory
            .open_regular_file_for_rename(&receipt_name)
            .map_err(|error| {
                format!(
                    "could not open old receipt for no-replace transition ({})",
                    error.kind()
                )
            })?;
        if SecureDir::file_identity(&active)
            .map_err(|error| format!("could not identify old receipt ({})", error.kind()))?
            != old_identity
            || SecureDir::hard_link_count(&active).map_err(|error| {
                format!("could not inspect old receipt links ({})", error.kind())
            })? != 1
        {
            return Err("old receipt identity or link count changed before transition".to_owned());
        }
        receipt_directory
            .rename_file_child_noreplace(&receipt_name, &record.receipt_backup_name, &active)
            .map_err(|error| {
                format!(
                    "old receipt could not be moved to its operation-owned backup ({})",
                    error.kind()
                )
            })?;
        faults
            .after(UpdateOperation::ReceiptBackupRename)
            .map_err(|error| {
                format!(
                    "old receipt was backed up but rename reported failure ({})",
                    error.kind()
                )
            })?;
        receipt_directory.sync_directory().map_err(|error| {
            format!(
                "could not synchronize old receipt backup ({})",
                error.kind()
            )
        })?;
        if !verify_old_receipt_file_optional(
            receipt_directory,
            &record,
            &record.receipt_backup_name,
        )? {
            return Err("old receipt backup did not verify after rename".to_owned());
        }
        journal
            .set_state(UpdateState::OldReceiptBackedUp)
            .map_err(|error| {
                format!("old receipt is backed up but journal update failed: {error}")
            })?;
    } else if journal.record().state == UpdateState::ReceiptTransitionPending {
        journal
            .set_state(UpdateState::OldReceiptBackedUp)
            .map_err(|error| {
                format!("old receipt backup exists but journal update failed: {error}")
            })?;
    }

    let temp_name = record
        .receipt_temp_name
        .as_deref()
        .ok_or_else(|| "replacement receipt temporary name is missing".to_owned())?;
    let mut temp_file = receipt_directory
        .open_regular_file_for_rename(temp_name)
        .map_err(|error| {
            format!(
                "replacement receipt temporary is missing or unsafe ({})",
                error.kind()
            )
        })?;
    if SecureDir::file_identity(&temp_file).map_err(|error| {
        format!(
            "could not identify replacement receipt temporary ({})",
            error.kind()
        )
    })? != new_identity
        || super::receipt_store::read_named_receipt(receipt_directory, temp_name)
            .map_err(|error| format!("replacement receipt temporary is invalid: {error}"))?
            != *record
                .new_receipt
                .as_ref()
                .ok_or_else(|| "replacement receipt missing".to_owned())?
    {
        return Err("replacement receipt temporary differs from the durable journal".to_owned());
    }
    verify_old_receipt_file_optional(receipt_directory, &record, &record.receipt_backup_name)?
        .then_some(())
        .ok_or_else(|| "old receipt backup is missing or altered".to_owned())?;
    let existing = super::receipt_store::read_update_receipt_claims(
        receipt_directory,
        &[&record.receipt_backup_name, temp_name],
    )
    .map_err(|error| format!("other ownership receipts are invalid: {error}"))?;
    writer_lock
        .validate()
        .map_err(|error| format!("receipt-store writer lock changed ({error})"))?;
    faults
        .before(UpdateOperation::PublishReceipt)
        .map_err(|error| {
            format!(
                "new receipt publication failed before mutation ({})",
                error.kind()
            )
        })?;
    super::receipt_store::publish_receipt_no_replace(
        receipt_directory,
        writer_lock,
        record
            .new_receipt
            .as_ref()
            .ok_or_else(|| "replacement receipt missing".to_owned())?,
        &existing,
        temp_name,
        new_identity,
        &mut temp_file,
        receipt_faults,
    )
    .map_err(|error| {
        format!("new schema-v2 receipt could not be published without replacement: {error}")
    })?;
    faults
        .after(UpdateOperation::PublishReceipt)
        .map_err(|error| {
            format!(
                "new receipt was published but publication reported failure ({})",
                error.kind()
            )
        })?;
    journal
        .set_state(UpdateState::NewReceiptPublished)
        .map_err(|error| format!("new receipt is published but journal update failed: {error}"))
}

fn ensure_adapter_shared_name_absent(receipt: &InstallationReceipt) -> Result<(), String> {
    let agent = crate::agents::AgentId::from_identifier(receipt.agent())
        .ok_or_else(|| "update journal has an unsupported agent".to_owned())?;
    if !agent.adapter().discovers_shared_agents_skills() {
        return Ok(());
    }
    let scope = match receipt.scope() {
        "project" => super::plan::InstallScope::Project,
        "user" => super::plan::InstallScope::User,
        _ => return Err("update receipt has an unsupported scope".to_owned()),
    };
    let root = receipt.root_path();
    let shared = super::destination::inspect_destination_for_agent(
        crate::agents::AgentId::Codex,
        Some(&root),
        &root,
        receipt.skill_name(),
    )
    .map_err(|error| format!("could not recheck shared skill visibility: {error}"))?;
    if shared.skill_state() != super::destination::ComponentState::Missing
        || !shared.conflicts().is_empty()
    {
        let precedence = if agent == crate::agents::AgentId::Gemini {
            "Gemini would load the shared .agents/skills copy before its native .gemini/skills copy"
        } else {
            "Cursor's documented precedence between shared and native copies is unclear"
        };
        return Err(format!(
            "same-named shared skill appeared before update recovery; {precedence}; preserve the pending update and shared content"
        ));
    }
    let _ = scope;
    Ok(())
}

fn verify_receipt_file_matches(
    directory: &SecureDir,
    name: &str,
    receipt: &InstallationReceipt,
    identity: FileIdentity,
) -> Result<(), String> {
    if !verify_receipt_file_optional(directory, name, receipt, identity)? {
        return Err(format!("receipt {name:?} is missing"));
    }
    Ok(())
}

fn verify_receipt_file_optional(
    directory: &SecureDir,
    name: &str,
    receipt: &InstallationReceipt,
    identity: FileIdentity,
) -> Result<bool, String> {
    let Some(file) = open_file_child(directory, name)? else {
        return Ok(false);
    };
    if SecureDir::file_identity(&file)
        .map_err(|error| format!("could not identify receipt {name:?} ({})", error.kind()))?
        != identity
        || SecureDir::hard_link_count(&file)
            .map_err(|error| format!("could not inspect receipt links ({})", error.kind()))?
            != 1
    {
        return Err(format!("receipt {name:?} identity or link count changed"));
    }
    let actual = super::receipt_store::read_named_receipt(directory, name)
        .map_err(|error| format!("receipt {name:?} is invalid: {error}"))?;
    Ok(actual == *receipt)
}

fn verify_old_receipt_file_matches(
    directory: &SecureDir,
    record: &UpdateRecord,
    name: &str,
) -> Result<(), String> {
    if !verify_old_receipt_file_optional(directory, record, name)? {
        return Err(format!("old ownership receipt {name:?} is missing"));
    }
    Ok(())
}

fn verify_old_receipt_file_optional(
    directory: &SecureDir,
    record: &UpdateRecord,
    name: &str,
) -> Result<bool, String> {
    let identity = FileIdentity::from(record.old_receipt_file_identity);
    if !verify_receipt_file_optional(directory, name, &record.old_receipt, identity)? {
        return Ok(false);
    }
    let digest = super::receipt_store::receipt_file_sha256(directory, name)
        .map_err(|error| format!("could not verify old receipt bytes: {error}"))?;
    if digest != record.old_receipt_sha256 {
        return Err(format!("old ownership receipt {name:?} bytes changed"));
    }
    Ok(true)
}

fn verify_active_new_installation(
    record: &UpdateRecord,
    receipt_directory: &SecureDir,
) -> Result<(), String> {
    let receipt = record
        .new_receipt
        .as_ref()
        .ok_or_else(|| "replacement ownership receipt is missing".to_owned())?;
    let name = receipt
        .receipt_filename()
        .map_err(|error| error.to_string())?;
    let identity = FileIdentity::from(
        record
            .new_receipt_file_identity
            .ok_or_else(|| "replacement receipt file identity is missing".to_owned())?,
    );
    verify_receipt_file_matches(receipt_directory, &name, receipt, identity)?;
    let (_, _, skills) = open_update_parents(record)?;
    verify_named_tree(
        &skills,
        receipt.skill_name(),
        receipt,
        FileIdentity::from(
            record
                .stage_identity
                .ok_or_else(|| "replacement directory identity is missing".to_owned())?,
        ),
        &record.stage_objects,
    )?
    .ok_or_else(|| "replacement destination is missing while its receipt is active".to_owned())?;
    Ok(())
}

fn verify_new_destination(record: &UpdateRecord) -> Result<(), String> {
    let receipt = record
        .new_receipt
        .as_ref()
        .ok_or_else(|| "replacement ownership receipt is missing".to_owned())?;
    let (_, _, skills) = open_update_parents(record)?;
    verify_named_tree(
        &skills,
        receipt.skill_name(),
        receipt,
        FileIdentity::from(
            record
                .stage_identity
                .ok_or_else(|| "replacement directory identity is missing".to_owned())?,
        ),
        &record.stage_objects,
    )?
    .ok_or_else(|| "replacement destination is missing".to_owned())?;
    Ok(())
}

fn cleanup_old_backup(
    journal: &mut UpdateJournal,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let record = journal.record().clone();
    let (_, _, skills) = open_update_parents(&record)?;
    let Some(root) = open_directory_child(&skills, &record.backup_name)? else {
        let all_intents = record
            .old_objects
            .iter()
            .all(|object| record.cleanup_intents.contains(&object.key()));
        return if all_intents {
            Ok(())
        } else {
            Err("old backup disappeared without complete per-object cleanup evidence".to_owned())
        };
    };
    let root_identity = FileIdentity::from(record.old_destination_identity);
    if root
        .identity()
        .map_err(|error| format!("could not identify old backup ({})", error.kind()))?
        != root_identity
    {
        return Err("old backup directory identity differs from the journal".to_owned());
    }

    let mut files = record
        .old_objects
        .iter()
        .filter(|object| object.kind == RemovalObjectKind::File)
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut directories = record
        .old_objects
        .iter()
        .filter(|object| object.kind == RemovalObjectKind::Directory)
        .cloned()
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .path
            .split('/')
            .count()
            .cmp(&left.path.split('/').count())
            .then_with(|| right.path.cmp(&left.path))
    });
    for object in files.into_iter().chain(directories) {
        cleanup_backup_object(journal, &root, &object, faults)?;
    }
    cleanup_backup_root(journal, &skills, &record.backup_name, root_identity, faults)?;
    if open_directory_child(&skills, &record.backup_name)?.is_some() {
        return Err("old backup root remains after verified object cleanup".to_owned());
    }
    Ok(())
}

fn cleanup_backup_object(
    journal: &mut UpdateJournal,
    root: &SecureDir,
    object: &RemovalObject,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let key = object.key();
    let had_intent = journal.record().cleanup_intents.contains(&key);
    match object.kind {
        RemovalObjectKind::File => {
            let (parent, name) = match open_relative_parent(root, &object.path, false) {
                Ok(parent) => parent,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && had_intent
                        && missing_parent_cleanup_is_proven(
                            root,
                            journal.record(),
                            &object.path,
                        )? =>
                {
                    return Ok(());
                }
                Err(error) => {
                    return Err(format!(
                        "could not open old backup file parent ({})",
                        error.kind()
                    ));
                }
            };
            let Some(file) = open_file_child(&parent, &name)? else {
                return if had_intent {
                    Ok(())
                } else {
                    Err(format!(
                        "old backup file {:?} is missing without a cleanup intent",
                        object.path
                    ))
                };
            };
            verify_backup_file(&file, object)?;
            drop(file);
            if !had_intent {
                journal
                    .record_cleanup_intent(key.clone())
                    .map_err(|error| {
                        format!("could not persist old-file cleanup intent: {error}")
                    })?;
            }
            faults
                .before(UpdateOperation::CleanupFile)
                .map_err(|error| {
                    format!(
                        "old backup file cleanup failed before unlink ({})",
                        error.kind()
                    )
                })?;
            let file = parent
                .open_regular_file_for_rename(&name)
                .map_err(|error| {
                    format!("old backup file changed before unlink ({})", error.kind())
                })?;
            verify_backup_file(&file, object)?;
            parent
                .remove_regular_file_if_identity(&name, FileIdentity::from(object.identity))
                .map_err(|error| {
                    format!(
                        "could not remove verified old backup file ({})",
                        error.kind()
                    )
                })?;
            parent.sync_directory().map_err(|error| {
                format!(
                    "could not synchronize old backup file removal ({})",
                    error.kind()
                )
            })?;
            faults
                .after(UpdateOperation::CleanupFile)
                .map_err(|error| {
                    format!(
                        "old backup file was removed but cleanup reported failure ({})",
                        error.kind()
                    )
                })?;
        }
        RemovalObjectKind::Directory => {
            let (parent, name) = match open_relative_parent(root, &object.path, false) {
                Ok(parent) => parent,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && had_intent
                        && missing_parent_cleanup_is_proven(
                            root,
                            journal.record(),
                            &object.path,
                        )? =>
                {
                    return Ok(());
                }
                Err(error) => {
                    return Err(format!(
                        "could not open old backup directory parent ({})",
                        error.kind()
                    ));
                }
            };
            let Some(directory) = open_directory_child(&parent, &name)? else {
                return if had_intent {
                    Ok(())
                } else {
                    Err(format!(
                        "old backup directory {:?} is missing without a cleanup intent",
                        object.path
                    ))
                };
            };
            verify_empty_directory_identity(&directory, FileIdentity::from(object.identity))?;
            drop(directory);
            if !had_intent {
                journal
                    .record_cleanup_intent(key.clone())
                    .map_err(|error| {
                        format!("could not persist old-directory cleanup intent: {error}")
                    })?;
            }
            faults
                .before(UpdateOperation::CleanupDirectory)
                .map_err(|error| {
                    format!(
                        "old backup directory cleanup failed before removal ({})",
                        error.kind()
                    )
                })?;
            let directory = open_directory_child(&parent, &name)?
                .ok_or_else(|| "old backup directory disappeared before removal".to_owned())?;
            verify_empty_directory_identity(&directory, FileIdentity::from(object.identity))?;
            parent.remove_directory(&name).map_err(|error| {
                format!(
                    "could not remove verified empty old backup directory ({})",
                    error.kind()
                )
            })?;
            parent.sync_directory().map_err(|error| {
                format!(
                    "could not synchronize old backup directory removal ({})",
                    error.kind()
                )
            })?;
            faults
                .after(UpdateOperation::CleanupDirectory)
                .map_err(|error| {
                    format!(
                        "old backup directory was removed but cleanup reported failure ({})",
                        error.kind()
                    )
                })?;
        }
        RemovalObjectKind::Root => {
            return Err("root cleanup reached through a child object".to_owned());
        }
    }
    Ok(())
}

fn missing_parent_cleanup_is_proven(
    root: &SecureDir,
    record: &UpdateRecord,
    relative_path: &str,
) -> Result<bool, String> {
    let components = relative_path.split('/').collect::<Vec<_>>();
    let mut current = root.clone();
    let mut prefix = Vec::new();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        prefix.push(*component);
        match open_directory_child(&current, component)? {
            Some(directory) => current = directory,
            None => {
                let key = format!("directory:{}", prefix.join("/"));
                return Ok(record.cleanup_intents.contains(&key));
            }
        }
    }
    Ok(false)
}

fn verify_backup_file(file: &fs::File, object: &RemovalObject) -> Result<(), String> {
    if SecureDir::file_identity(file)
        .map_err(|error| format!("could not identify old backup file ({})", error.kind()))?
        != FileIdentity::from(object.identity)
        || SecureDir::hard_link_count(file).map_err(|error| {
            format!("could not inspect old backup file links ({})", error.kind())
        })? != 1
    {
        return Err(format!(
            "old backup file {:?} identity or link count changed",
            object.path
        ));
    }
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect old backup file ({})", error.kind()))?;
    if metadata.len() != object.byte_count {
        return Err(format!("old backup file {:?} length changed", object.path));
    }
    let expected = object
        .sha256
        .as_deref()
        .ok_or_else(|| "old backup file digest is missing from the journal".to_owned())?;
    let mut file = file
        .try_clone()
        .map_err(|error| format!("could not clone old backup file handle ({})", error.kind()))?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut count = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash old backup file ({})", error.kind()))?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .filter(|value| *value <= object.byte_count)
            .ok_or_else(|| "old backup file changed while hashing".to_owned())?;
        digest.update(&buffer[..read]);
    }
    if count != object.byte_count || sha256_string(digest.finish().as_ref()) != expected {
        return Err(format!("old backup file {:?} digest changed", object.path));
    }
    Ok(())
}

fn verify_empty_directory_identity(
    directory: &SecureDir,
    expected: FileIdentity,
) -> Result<(), String> {
    if directory
        .identity()
        .map_err(|error| format!("could not identify old backup directory ({})", error.kind()))?
        != expected
        || !directory
            .entries()
            .map_err(|error| format!("could not inspect old backup directory ({})", error.kind()))?
            .is_empty()
    {
        return Err(
            "old backup directory identity changed or unexpected content remains".to_owned(),
        );
    }
    Ok(())
}

fn cleanup_backup_root(
    journal: &mut UpdateJournal,
    skills: &SecureDir,
    backup_name: &str,
    expected_identity: FileIdentity,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let key = "root".to_owned();
    let had_intent = journal.record().cleanup_intents.contains(&key);
    let Some(root) = open_directory_child(skills, backup_name)? else {
        return if had_intent {
            Ok(())
        } else {
            Err("old backup root is missing without a cleanup intent".to_owned())
        };
    };
    verify_empty_directory_identity(&root, expected_identity)?;
    drop(root);
    if !had_intent {
        journal.record_cleanup_intent(key).map_err(|error| {
            format!("could not persist old-backup root cleanup intent: {error}")
        })?;
    }
    faults
        .before(UpdateOperation::CleanupDirectory)
        .map_err(|error| {
            format!(
                "old backup root cleanup failed before removal ({})",
                error.kind()
            )
        })?;
    let root = open_directory_child(skills, backup_name)?
        .ok_or_else(|| "old backup root disappeared before removal".to_owned())?;
    verify_empty_directory_identity(&root, expected_identity)?;
    drop(root);
    skills.remove_directory(backup_name).map_err(|error| {
        format!(
            "could not remove verified empty old backup root ({})",
            error.kind()
        )
    })?;
    skills.sync_directory().map_err(|error| {
        format!(
            "could not synchronize old backup root removal ({})",
            error.kind()
        )
    })?;
    faults
        .after(UpdateOperation::CleanupDirectory)
        .map_err(|error| {
            format!(
                "old backup root was removed but cleanup reported failure ({})",
                error.kind()
            )
        })?;
    Ok(())
}

fn cleanup_old_receipt_backup(
    journal: &mut UpdateJournal,
    receipt_directory: &SecureDir,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let record = journal.record().clone();
    let old_identity = FileIdentity::from(record.old_receipt_file_identity);
    let had_intent = journal.record().receipt_backup_cleanup_intent;
    let backup_file = open_file_child(receipt_directory, &record.receipt_backup_name)?;
    if backup_file.is_none() && !had_intent {
        return Err("old receipt backup is missing without a durable cleanup intent".to_owned());
    }
    if backup_file.is_some() {
        verify_old_receipt_file_matches(receipt_directory, &record, &record.receipt_backup_name)?;
        drop(backup_file);
        if !had_intent {
            journal
                .record_receipt_backup_cleanup_intent()
                .map_err(|error| {
                    format!("could not persist old receipt cleanup intent: {error}")
                })?;
        }
        faults
            .before(UpdateOperation::CleanupReceiptBackup)
            .map_err(|error| {
                format!(
                    "old receipt backup cleanup failed before unlink ({})",
                    error.kind()
                )
            })?;
        verify_old_receipt_file_matches(receipt_directory, &record, &record.receipt_backup_name)?;
        receipt_directory
            .remove_regular_file_if_identity(&record.receipt_backup_name, old_identity)
            .map_err(|error| {
                format!(
                    "could not remove verified old receipt backup ({})",
                    error.kind()
                )
            })?;
        receipt_directory.sync_directory().map_err(|error| {
            format!(
                "could not synchronize old receipt backup removal ({})",
                error.kind()
            )
        })?;
        faults
            .after(UpdateOperation::CleanupReceiptBackup)
            .map_err(|error| {
                format!(
                    "old receipt backup was removed but cleanup reported failure ({})",
                    error.kind()
                )
            })?;
    }
    if open_file_child(receipt_directory, &record.receipt_backup_name)?.is_some() {
        return Err("old receipt backup remains after cleanup intent".to_owned());
    }
    Ok(())
}

fn verify_old_at(
    skills: &SecureDir,
    record: &UpdateRecord,
    name: &str,
) -> Result<Option<SecureDir>, String> {
    verify_named_tree(
        skills,
        name,
        &record.old_receipt,
        FileIdentity::from(record.old_destination_identity),
        &record.old_objects,
    )
}

fn verify_stage_at(
    skills: &SecureDir,
    record: &UpdateRecord,
    name: &str,
) -> Result<Option<SecureDir>, String> {
    let Some(identity) = record.stage_identity else {
        return Err("replacement directory identity was never recorded".to_owned());
    };
    verify_named_tree(
        skills,
        name,
        &record.new_receipt_intent,
        FileIdentity::from(identity),
        &record.stage_objects,
    )
}

pub(super) fn recover_all(
    store: &UpdateJournalStore,
    receipt_directory: &SecureDir,
    writer_lock: &WriterLock,
    receipt_faults: &dyn ReceiptFaultInjector,
    faults: &dyn UpdateFaultInjector,
) -> Result<(), String> {
    let records = store
        .read_all()
        .map_err(|error| format!("update journal is corrupt or unsafe: {error}"))?;
    for record in records {
        writer_lock.validate().map_err(|error| {
            format!("receipt-store writer lock changed during update recovery ({error})")
        })?;
        let operation_id = record.operation_id.clone();
        let mut journal = store
            .resume(record)
            .map_err(|error| format!("could not resume update journal: {error}"))?;
        drive_forward(
            &mut journal,
            receipt_directory,
            writer_lock,
            receipt_faults,
            faults,
        )
        .map_err(|detail| format!("update operation {operation_id} recovery conflict: {detail}"))?;
        if !is_update_terminal(Some(journal.record())) {
            return Err(format!(
                "update operation {operation_id} recovery stopped before a terminal state"
            ));
        }
        store
            .cleanup_completed(&operation_id)
            .map_err(|error| format!("update operation {operation_id} reached a terminal state but journal cleanup failed: {error}"))?;
    }
    Ok(())
}

fn open_plan_parents(plan: &RemovalPlan) -> Result<(SecureDir, SecureDir, SecureDir), UpdateError> {
    let project = SecureDir::open_absolute(&plan.receipt().root_path()).map_err(|error| {
        UpdateError::Conflict(format!(
            "project root is unsafe or unavailable ({})",
            error.kind()
        ))
    })?;
    if project.identity().map_err(|error| {
        UpdateError::Conflict(format!(
            "could not identify project root ({})",
            error.kind()
        ))
    })? != plan.receipt().scope_root_identity()
    {
        return Err(UpdateError::Conflict(
            "project root identity changed after update planning".to_owned(),
        ));
    }
    let agents = open_required_directory(&project, plan.receipt().agent_directory())
        .map_err(UpdateError::Conflict)?;
    let skills = open_required_directory(&agents, "skills").map_err(UpdateError::Conflict)?;
    if agents.identity().map_err(|error| {
        UpdateError::Conflict(format!(
            "could not identify agent directory ({})",
            error.kind()
        ))
    })? != plan.agents_identity()
        || skills.identity().map_err(|error| {
            UpdateError::Conflict(format!(
                "could not identify skills directory ({})",
                error.kind()
            ))
        })? != plan.skills_identity()
    {
        return Err(UpdateError::Conflict(
            "agent destination parent identity changed after update planning".to_owned(),
        ));
    }
    Ok((project, agents, skills))
}

fn open_required_directory(parent: &SecureDir, name: &str) -> Result<SecureDir, String> {
    match open_directory_child(parent, name)? {
        Some(directory) => Ok(directory),
        None => Err(format!("required directory {name:?} is missing")),
    }
}

fn open_update_parents(record: &UpdateRecord) -> Result<(SecureDir, SecureDir, SecureDir), String> {
    let project = SecureDir::open_absolute(&record.old_receipt.root_path())
        .map_err(|error| format!("project root is unsafe or unavailable ({})", error.kind()))?;
    if project
        .identity()
        .map_err(|error| format!("could not identify project root ({})", error.kind()))?
        != record.old_receipt.scope_root_identity()
    {
        return Err("project root identity differs from the update journal".to_owned());
    }
    let agents = open_required_directory(&project, record.old_receipt.agent_directory())?;
    let skills = open_required_directory(&agents, "skills")?;
    if agents
        .identity()
        .map_err(|error| format!("could not identify agent directory ({})", error.kind()))?
        != FileIdentity::from(record.agents_identity)
        || skills
            .identity()
            .map_err(|error| format!("could not identify skills directory ({})", error.kind()))?
            != FileIdentity::from(record.skills_identity)
    {
        return Err("agent destination parent identity differs from the update journal".to_owned());
    }
    Ok((project, agents, skills))
}

fn open_directory_child(parent: &SecureDir, expected: &str) -> Result<Option<SecureDir>, String> {
    let entries = parent
        .entries()
        .map_err(|error| format!("could not inspect directory entries ({})", error.kind()))?;
    let mut matches = entries.iter().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected))
    });
    let Some((name, kind)) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || name.to_str() != Some(expected) || *kind != NodeKind::Directory {
        return Err(format!(
            "directory component {expected:?} is linked, unsupported, or case-colliding"
        ));
    }
    parent
        .open_directory(std::ffi::OsStr::new(expected))
        .map(Some)
        .map_err(|error| format!("could not open directory component ({})", error.kind()))
}

fn open_file_child(parent: &SecureDir, expected: &str) -> Result<Option<fs::File>, String> {
    let entries = parent
        .entries()
        .map_err(|error| format!("could not inspect directory entries ({})", error.kind()))?;
    let mut matches = entries.iter().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected))
    });
    let Some((name, kind)) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || name.to_str() != Some(expected) || *kind != NodeKind::RegularFile
    {
        return Err(format!(
            "file component {expected:?} is linked, unsupported, or case-colliding"
        ));
    }
    parent
        .open_regular_file(expected)
        .map(Some)
        .map_err(|error| format!("could not open file component ({})", error.kind()))
}

fn verify_named_tree(
    skills: &SecureDir,
    name: &str,
    receipt: &InstallationReceipt,
    root_identity: FileIdentity,
    expected_objects: &[RemovalObject],
) -> Result<Option<SecureDir>, String> {
    let Some(root) = open_directory_child(skills, name)? else {
        return Ok(None);
    };
    if root
        .identity()
        .map_err(|error| format!("could not identify installed directory ({})", error.kind()))?
        != root_identity
    {
        return Err(format!(
            "directory {name:?} is not the recorded operation-owned object"
        ));
    }
    let objects = super::removal::verify_inventory(receipt, root.clone()).map_err(|error| {
        format!("directory {name:?} failed complete inventory verification: {error}")
    })?;
    if objects != expected_objects {
        return Err(format!(
            "directory {name:?} object identities differ from the update journal"
        ));
    }
    Ok(Some(root))
}

#[cfg(test)]
mod tests {
    use super::{SecureDir, UpdateFileChangeKind, UpdatePlan};
    use std::{
        collections::BTreeMap,
        fs,
        io::{Cursor, Write},
        path::PathBuf,
        process::Command,
        time::Duration,
    };

    use tempfile::{TempDir, tempdir};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        install::{
            check::{CheckStatus, classify},
            command::prepare_install_session,
            commit::InstallFaultInjector,
            journal::NoJournalFaults,
            plan::{InstallAgent, InstallScope},
            receipt_store::{NoReceiptFaults, ReceiptStore, ReceiptStoreError},
            removal::NoRemovalFaults,
            scope::ResolvedScope,
            update::{
                NoUpdateFaults, UpdateError, UpdateFaultInjector, UpdateOperation,
                UpdatePreparation, format_dry_run, prepare_update, prepare_update_in_scope,
            },
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, TransportError},
            staging::TempDirFactory,
        },
    };

    const OLD_REVISION: &str = "2f68bee292a8c68346b4294fb31d9563eefe1ade";
    const NEW_REVISION: &str = "4f68bee292a8c68346b4294fb31d9563eefe1ade";

    struct Fixture {
        _parent: Option<TempDir>,
        project: PathBuf,
        home: PathBuf,
        staging: PathBuf,
        store: ReceiptStore,
        data: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = tempdir().expect("test parent");
            let root = parent.path().canonicalize().expect("canonical temp root");
            Self::from_root(root, Some(parent))
        }

        fn from_root(root: PathBuf, parent: Option<TempDir>) -> Self {
            let root = root.canonicalize().expect("canonical fixture root");
            let project = root.join("project");
            let home = root.join("home");
            let staging = root.join("staging");
            let data = root.join("data");
            fs::create_dir_all(&project).expect("project");
            fs::create_dir_all(&home).expect("home");
            fs::create_dir_all(&staging).expect("staging parent");
            Self {
                _parent: parent,
                project,
                home,
                staging,
                store: ReceiptStore::new(data.join("receipts")),
                data,
            }
        }

        fn install_old(&self) -> crate::install::receipt::InstallationReceipt {
            let catalog = catalog(OLD_REVISION, "1.0.0");
            let session = prepare_install_session(
                &catalog,
                "uncharted",
                "codex",
                None,
                &self.project,
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("validated install session");
            session
                .commit_and_record_at(&self.store, 100, &NoInstallFaults, &NoReceiptFaults)
                .expect("install old version")
        }

        fn claude_project_scope(&self) -> ResolvedScope {
            ResolvedScope::from_agent_validated(
                InstallAgent::ClaudeCode,
                InstallScope::Project,
                self.project.clone(),
            )
        }

        fn install_old_claude(&self) -> crate::install::receipt::InstallationReceipt {
            let scope = self.claude_project_scope();
            let session = super::super::command::prepare_install_session_in_scope(
                &catalog(OLD_REVISION, "1.0.0"),
                "uncharted",
                "claude-code",
                &scope,
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("validated Claude install session");
            session
                .commit_and_record_at(&self.store, 100, &NoInstallFaults, &NoReceiptFaults)
                .expect("install old Claude version")
        }

        fn install_old_adapter(
            &self,
            agent: InstallAgent,
        ) -> crate::install::receipt::InstallationReceipt {
            let scope = ResolvedScope::from_agent_validated(
                agent,
                InstallScope::Project,
                self.project.clone(),
            );
            let session = super::super::command::prepare_install_session_in_scope(
                &catalog(OLD_REVISION, "1.0.0"),
                "uncharted",
                agent.as_str(),
                &scope,
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("validated adapter install session");
            session
                .commit_and_record_at(&self.store, 100, &NoInstallFaults, &NoReceiptFaults)
                .expect("install old adapter version")
        }

        fn user_scope(&self) -> ResolvedScope {
            ResolvedScope::for_test(InstallScope::User, self.home.clone())
        }

        fn install_old_user(&self) -> crate::install::receipt::InstallationReceipt {
            let catalog = catalog(OLD_REVISION, "1.0.0");
            let session = super::super::command::prepare_install_session_in_scope(
                &catalog,
                "uncharted",
                "codex",
                &self.user_scope(),
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("validated user install session");
            session
                .commit_and_record_at(&self.store, 100, &NoInstallFaults, &NoReceiptFaults)
                .expect("install old user version")
        }

        fn prepare_new(&self) -> UpdatePlan {
            match prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &self.project,
                &self.store,
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("prepare update")
            {
                UpdatePreparation::Ready(plan) => *plan,
                UpdatePreparation::AlreadyUpToDate(_) => panic!("new pin differs"),
            }
        }

        fn prepare_new_user(&self) -> UpdatePlan {
            match prepare_update_in_scope(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                &self.user_scope(),
                &self.store,
                &TestTransport,
                &TestFactory(self.staging.clone()),
            )
            .expect("prepare user update")
            {
                UpdatePreparation::Ready(plan) => *plan,
                UpdatePreparation::AlreadyUpToDate(_) => panic!("new pin differs"),
            }
        }
    }

    struct NoInstallFaults;
    impl InstallFaultInjector for NoInstallFaults {}

    struct TestFactory(PathBuf);
    impl TempDirFactory for TestFactory {
        fn create_private(&self) -> std::io::Result<TempDir> {
            tempfile::Builder::new()
                .prefix("update-stage-")
                .tempdir_in(&self.0)
        }
    }

    struct TestTransport;
    impl GithubTransport for TestTransport {
        fn get(&self, url: &str) -> Result<GithubResponse, TransportError> {
            let revision = if url.contains(OLD_REVISION) {
                OLD_REVISION
            } else if url.contains(NEW_REVISION) {
                NEW_REVISION
            } else {
                panic!("unexpected fixture URL: {url}");
            };
            let archive = archive(revision);
            Ok(GithubResponse::new(
                200,
                Some("application/zip"),
                Some(archive.len() as u64),
                None,
                Cursor::new(archive),
            ))
        }
    }

    fn catalog(revision: &str, version: &str) -> crate::models::catalog::Catalog {
        let json = format!(
            r#"{{"schemaVersion":1,"resources":[{{"id":"uncharted","kind":"skill","displayName":"Uncharted","skillName":"uncharted-ideation","description":"Synthetic update fixture.","version":"{version}","aliases":[],"source":{{"provider":"github","owner":"Example-Org","repository":"catalog-test-fixtures","path":"skills/uncharted-ideation","commit":"{revision}"}}}}]}}"#
        );
        parse_catalog_json(json.as_bytes()).expect("validated local fixture catalog")
    }

    fn archive(revision: &str) -> Vec<u8> {
        let root = format!("catalog-test-fixtures-{revision}");
        let skill = format!("{root}/skills/uncharted-ideation");
        let mut files = BTreeMap::<String, Vec<u8>>::new();
        if revision == OLD_REVISION {
            files.insert(
                "SKILL.md".to_owned(),
                b"---\nname: uncharted-ideation\ndescription: Synthetic update skill.\n---\nOld version.\n".to_vec(),
            );
            files.insert("assets/changed.bin".to_owned(), vec![0, 255, 1]);
            files.insert("assets/unchanged.bin".to_owned(), vec![7, 0, 8]);
            files.insert("references/guide.md".to_owned(), b"same guide".to_vec());
            files.insert("scripts/inert.sh".to_owned(), b"echo inert\n".to_vec());
            files.insert("removed.txt".to_owned(), b"remove me".to_vec());
        } else {
            files.insert(
                "SKILL.md".to_owned(),
                b"---\nname: uncharted-ideation\ndescription: Synthetic update skill.\n---\nNew version.\n".to_vec(),
            );
            files.insert("assets/added.bin".to_owned(), vec![0, 200, 255, 3]);
            files.insert("assets/changed.bin".to_owned(), vec![0, 255, 2, 3]);
            files.insert("assets/unchanged.bin".to_owned(), vec![7, 0, 8]);
            files.insert("references/guide.md".to_owned(), b"same guide".to_vec());
            files.insert("scripts/inert.sh".to_owned(), b"echo inert\n".to_vec());
        }

        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let mut bytes = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(&mut bytes);
        zip.add_directory(format!("{root}/"), options)
            .expect("root");
        zip.add_directory(format!("{root}/skills/"), options)
            .expect("skills directory");
        zip.add_directory(format!("{skill}/"), options)
            .expect("skill directory");
        for path in ["assets/", "references/", "scripts/"] {
            zip.add_directory(format!("{skill}/{path}"), options)
                .expect("nested directory");
        }
        for (path, content) in files {
            zip.start_file(format!("{skill}/{path}"), options)
                .expect("start file");
            zip.write_all(&content).expect("write fixture bytes");
        }
        zip.finish().expect("finish archive");
        bytes.into_inner()
    }

    struct FailBefore(UpdateOperation);
    impl UpdateFaultInjector for FailBefore {
        fn before(&self, operation: UpdateOperation) -> std::io::Result<()> {
            if operation == self.0 {
                Err(std::io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        }
    }

    struct FailAfter(UpdateOperation);
    impl UpdateFaultInjector for FailAfter {
        fn after(&self, operation: UpdateOperation) -> std::io::Result<()> {
            if operation == self.0 {
                Err(std::io::Error::other("injected post-mutation failure"))
            } else {
                Ok(())
            }
        }
    }

    struct CrashAfter(UpdateOperation);
    impl UpdateFaultInjector for CrashAfter {
        fn after(&self, operation: UpdateOperation) -> std::io::Result<()> {
            if operation == self.0 {
                std::process::exit(79);
            }
            Ok(())
        }
    }

    fn parse_crash_operation(value: &str) -> Option<UpdateOperation> {
        [
            UpdateOperation::CreateStage,
            UpdateOperation::BackupRename,
            UpdateOperation::CommitReplacement,
            UpdateOperation::PrepareReceipt,
            UpdateOperation::ReceiptBackupRename,
            UpdateOperation::PublishReceipt,
            UpdateOperation::CleanupFile,
            UpdateOperation::CleanupDirectory,
            UpdateOperation::CleanupReceiptBackup,
        ]
        .into_iter()
        .find(|operation| format!("{operation:?}") == value)
    }

    #[test]
    fn read_only_check_rejects_a_pending_update_without_recovery_or_mutation() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let plan = fixture.prepare_new();
        let old_content =
            fs::read(receipt.destination_path().join("SKILL.md")).expect("old installed file");
        fixture
            .store
            .update(&plan, &FailBefore(UpdateOperation::CreateStage))
            .expect_err("injected stage failure leaves a pending update journal");

        let journal_directory = fixture.data.join("updates");
        let journal_snapshot = fs::read_dir(&journal_directory)
            .expect("pending update journal directory")
            .map(|entry| {
                let entry = entry.expect("journal entry");
                (
                    entry.file_name(),
                    fs::read(entry.path()).expect("journal bytes"),
                )
            })
            .collect::<Vec<_>>();
        assert!(!journal_snapshot.is_empty());
        let error = fixture
            .store
            .inspect_project(None, &fixture.project)
            .expect_err("check must fail while update recovery is pending");
        assert!(error.to_string().contains("recovery is required"));
        assert_eq!(
            fs::read(receipt.destination_path().join("SKILL.md")).expect("old file remains"),
            old_content
        );
        let journal_after = fs::read_dir(&journal_directory)
            .expect("journal remains")
            .map(|entry| {
                let entry = entry.expect("journal entry");
                (
                    entry.file_name(),
                    fs::read(entry.path()).expect("journal bytes"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(journal_after, journal_snapshot);
    }

    #[test]
    fn successful_update_replaces_tree_and_records_new_schema_v2_identity() {
        let fixture = Fixture::new();
        let old_receipt = fixture.install_old();
        let old_directory = old_receipt.destination_path();
        let unrelated = old_directory
            .parent()
            .expect("skill parent")
            .join("manual-skill");
        fs::create_dir(&unrelated).expect("unrelated project skill");
        fs::write(unrelated.join("keep.txt"), b"unrelated user content")
            .expect("unrelated sentinel");
        let plan = fixture.prepare_new();
        let changes = plan.changes();
        assert!(
            changes
                .iter()
                .any(|entry| entry.path() == "assets/added.bin"
                    && entry.kind() == UpdateFileChangeKind::Added)
        );
        assert!(
            changes.iter().any(|entry| entry.path() == "removed.txt"
                && entry.kind() == UpdateFileChangeKind::Removed)
        );
        assert!(
            changes
                .iter()
                .any(|entry| entry.path() == "assets/changed.bin"
                    && entry.kind() == UpdateFileChangeKind::Changed)
        );
        assert!(
            changes
                .iter()
                .any(|entry| entry.path() == "assets/unchanged.bin"
                    && entry.kind() == UpdateFileChangeKind::Unchanged)
        );

        let summary = fixture
            .store
            .update(&plan, &NoUpdateFaults)
            .expect("update succeeds");
        assert_eq!(
            crate::cli::format_update_success(&summary),
            format!(
                "Updated THRN skill\nResource: uncharted\nSkill name: uncharted-ideation\nAgent: codex\nScope: project\nDestination: {}\nPrevious pinned revision: {OLD_REVISION}\nNew pinned revision: {NEW_REVISION}\nFiles updated: {}\nBytes installed: {}\nOwnership receipt: updated and verified\n",
                old_directory.display(),
                summary.file_count(),
                summary.byte_count()
            )
        );
        assert_eq!(summary.previous_revision(), OLD_REVISION);
        assert_eq!(summary.new_revision(), NEW_REVISION);
        assert_eq!(
            std::fs::read(old_directory.join("assets/added.bin")).expect("new binary asset"),
            [0, 200, 255, 3]
        );
        assert!(!old_directory.join("removed.txt").exists());
        assert_eq!(
            std::fs::read(old_directory.join("scripts/inert.sh")).expect("inert script"),
            b"echo inert\n"
        );
        assert_eq!(
            fs::read(unrelated.join("keep.txt")).expect("unrelated content remains"),
            b"unrelated user content"
        );
        assert_eq!(fixture.store.list().expect("new active receipt").len(), 1);
        let receipts = fixture.store.list().expect("list replacement receipt");
        let receipt = &receipts[0];
        assert_eq!(receipt.source_commit(), NEW_REVISION);
        assert_eq!(receipt.schema_version(), 2);
        let destination_identity = SecureDir::open_absolute(&receipt.destination_path())
            .expect("new destination handle")
            .identity()
            .expect("new destination identity");
        assert_eq!(
            receipt.installed_directory_identity(),
            Some(destination_identity)
        );
        assert!(
            fixture
                .store
                .update_journal_store()
                .expect("update store")
                .pending_count()
                .expect("no pending updates")
                == 0
        );
        assert!(
            fixture
                .store
                .removal_journal_store()
                .expect("removal store")
                .pending_count()
                .expect("no pending removals")
                == 0
        );
    }

    #[test]
    fn dry_run_compares_files_and_leaves_project_and_receipt_store_unchanged() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let receipt_name = receipt.receipt_filename().expect("receipt name");
        let receipt_path = fixture.data.join("receipts").join(&receipt_name);
        let before_receipt = std::fs::read(&receipt_path).expect("old receipt bytes");
        let old_bytes = std::fs::read(receipt.destination_path().join("assets/changed.bin"))
            .expect("old file bytes");
        let plan = fixture.prepare_new();
        assert_eq!(
            plan.receipt_transition().publication(),
            super::ReceiptPublicationMode::BackupOldThenPublishNoReplace
        );
        assert_eq!(
            plan.receipt_transition().active_receipt_name(),
            receipt_name
        );
        assert!(
            plan.receipt_transition()
                .old_receipt_sha256()
                .starts_with("sha256:")
        );
        let output = format_dry_run(&plan);
        assert!(output.contains(&format!("Previous revision: {OLD_REVISION}")));
        assert!(output.contains(&format!("New revision: {NEW_REVISION}")));
        assert!(output.contains("changed: assets/changed.bin"));
        assert!(output.contains("removed: removed.txt"));
        assert!(output.contains("added: assets/added.bin"));
        assert!(output.contains("unchanged: references/guide.md"));
        assert!(output.contains("Exact pinned source may be fetched"));
        assert!(!plan.planned_backup_directory().exists());
        assert_eq!(
            std::fs::read(receipt_path).expect("receipt unchanged"),
            before_receipt
        );
        assert_eq!(
            std::fs::read(receipt.destination_path().join("assets/changed.bin"))
                .expect("installed file unchanged"),
            old_bytes
        );
        assert!(!fixture.data.join("updates").exists());
        drop(plan);
        assert_eq!(
            std::fs::read_dir(&fixture.staging)
                .expect("stage parent")
                .count(),
            0
        );
    }

    #[test]
    fn byte_equivalent_json_rewrite_after_planning_conflicts_before_mutation() {
        let fixture = Fixture::new();
        let old = fixture.install_old();
        let plan = fixture.prepare_new();
        let receipt_path = fixture
            .data
            .join("receipts")
            .join(old.receipt_filename().expect("receipt filename"));
        let mut equivalent_json = fs::read(&receipt_path).expect("old receipt bytes");
        equivalent_json.push(b' ');
        fs::write(&receipt_path, &equivalent_json)
            .expect("preserve parsed receipt with raw change");
        assert_eq!(fixture.store.list().expect("JSON remains valid")[0], old);

        let error = fixture
            .store
            .update(&plan, &NoUpdateFaults)
            .expect_err("raw receipt change invalidates the immutable plan");
        assert!(matches!(error, UpdateError::Conflict(_)), "{error}");
        assert!(!fixture.data.join("updates").exists());
        assert_eq!(
            fs::read(old.destination_path().join("SKILL.md")).expect("old tree unchanged"),
            b"---\nname: uncharted-ideation\ndescription: Synthetic update skill.\n---\nOld version.\n"
        );
    }

    #[test]
    fn same_pinned_revision_is_a_non_mutating_success_without_fetching() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let before = std::fs::read(receipt.destination_path().join("SKILL.md")).expect("old skill");
        let result = prepare_update(
            &catalog(OLD_REVISION, "1.0.0"),
            "uncharted",
            "codex",
            None,
            &fixture.project,
            &fixture.store,
            &NeverTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect("up-to-date result");
        let current = match result {
            UpdatePreparation::AlreadyUpToDate(receipt) => receipt,
            UpdatePreparation::Ready(_) => panic!("same revision must be current"),
        };
        assert_eq!(
            crate::cli::format_update_already_up_to_date(&current),
            format!(
                "Already up to date\nResource: uncharted\nSkill name: uncharted-ideation\nAgent: codex\nScope: project\nDestination: {}\nPinned revision: Example-Org/catalog-test-fixtures@{OLD_REVISION}\nOwnership receipt: verified, no changes needed\n",
                receipt.destination_path().display()
            )
        );
        assert_eq!(
            std::fs::read(receipt.destination_path().join("SKILL.md")).expect("unchanged skill"),
            before
        );
        assert_eq!(fixture.store.list().expect("receipt remains")[0], receipt);
        assert!(!fixture.data.join("updates").exists());
    }

    #[test]
    fn claude_project_and_user_updates_use_v4_receipts_and_scoped_v3_recovery() {
        let fixture = Fixture::new();
        let codex_project = fixture.install_old();
        let claude_project = ResolvedScope::from_agent_validated(
            crate::agents::AgentId::ClaudeCode,
            InstallScope::Project,
            fixture.project.clone(),
        );
        let claude_user = ResolvedScope::from_agent_validated(
            crate::agents::AgentId::ClaudeCode,
            InstallScope::User,
            fixture.home.clone(),
        );
        let install_claude = |scope: &ResolvedScope| {
            super::super::command::prepare_install_session_in_scope(
                &catalog(OLD_REVISION, "1.0.0"),
                "uncharted",
                "claude-code",
                scope,
                &TestTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .expect("Claude install session")
            .commit_and_record_at(&fixture.store, 101, &NoInstallFaults, &NoReceiptFaults)
            .expect("Claude schema-v4 receipt")
        };
        let old_project = install_claude(&claude_project);
        let old_user = install_claude(&claude_user);
        assert_eq!(old_project.schema_version(), 4);
        assert_eq!(old_user.schema_version(), 4);

        let prepare_claude_update = |scope: &ResolvedScope| match prepare_update_in_scope(
            &catalog(NEW_REVISION, "1.1.0"),
            "uncharted",
            "claude-code",
            scope,
            &fixture.store,
            &TestTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect("Claude update plan")
        {
            UpdatePreparation::Ready(plan) => *plan,
            UpdatePreparation::AlreadyUpToDate(_) => panic!("new pin differs"),
        };
        let project_plan = prepare_claude_update(&claude_project);
        let error = fixture
            .store
            .update(
                &project_plan,
                &FailAfter(UpdateOperation::CommitReplacement),
            )
            .expect_err("post-commit fault cannot report update success");
        assert!(matches!(error, UpdateError::Incomplete { .. }), "{error}");
        let pending_journal = fs::read_dir(fixture.data.join("updates"))
            .expect("pending Claude update journal")
            .next()
            .expect("one pending journal")
            .expect("journal entry")
            .path();
        let journal_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&pending_journal).expect("Claude journal bytes"))
                .expect("Claude journal JSON");
        assert_eq!(journal_json["schemaVersion"], 3);
        assert_eq!(journal_json["oldReceipt"]["agent"], "claude-code");
        assert_eq!(journal_json["oldReceipt"]["scope"], "project");
        assert!(fixture.store.list().is_err());

        fixture
            .store
            .recover_scope_with_faults(&claude_project, &NoJournalFaults, &NoReceiptFaults)
            .expect("recover only the selected Claude project target");
        fixture
            .store
            .recover_scope_with_faults(&claude_project, &NoJournalFaults, &NoReceiptFaults)
            .expect("same-target recovery is idempotent");
        let after_project_recovery = fixture.store.list().expect("all receipts recovered");
        let new_project = after_project_recovery
            .iter()
            .find(|receipt| receipt.agent() == "claude-code" && receipt.scope() == "project")
            .expect("updated Claude project receipt");
        let unchanged_user = after_project_recovery
            .iter()
            .find(|receipt| receipt.agent() == "claude-code" && receipt.scope() == "user")
            .expect("unmodified Claude user receipt");
        let unchanged_codex = after_project_recovery
            .iter()
            .find(|receipt| receipt.agent() == "codex")
            .expect("Codex project receipt");
        assert_eq!(new_project.schema_version(), 4);
        assert_eq!(new_project.source_commit(), NEW_REVISION);
        assert_eq!(unchanged_user.source_commit(), OLD_REVISION);
        assert_eq!(unchanged_codex.source_commit(), OLD_REVISION);
        assert_eq!(
            unchanged_codex.destination_path(),
            codex_project.destination_path()
        );

        let user_plan = prepare_claude_update(&claude_user);
        let summary = fixture
            .store
            .update(&user_plan, &NoUpdateFaults)
            .expect("Claude user update");
        assert_eq!(summary.agent(), "claude-code");
        assert_eq!(summary.scope(), "user");
        assert_eq!(summary.destination(), old_user.destination_path());
        assert!(
            crate::cli::format_update_success(&summary)
                .contains("Agent: claude-code\nScope: user\n")
        );
        let after_user_update = fixture.store.list().expect("updated receipts");
        assert_eq!(after_user_update.len(), 3);
        assert!(
            after_user_update
                .iter()
                .all(|receipt| receipt.schema_version() == 4 || receipt.agent() == "codex")
        );
        assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        assert!(!fixture.home.join("SKILL-WAS-EXECUTED").exists());
    }

    #[test]
    fn cursor_and_gemini_project_and_user_updates_use_v5_receipts() {
        for agent in [InstallAgent::Cursor, InstallAgent::Gemini] {
            let fixture = Fixture::new();
            let scopes = [
                ResolvedScope::from_agent_validated(
                    agent,
                    InstallScope::Project,
                    fixture.project.clone(),
                ),
                ResolvedScope::from_agent_validated(
                    agent,
                    InstallScope::User,
                    fixture.home.clone(),
                ),
            ];

            for scope in &scopes {
                let session = super::super::command::prepare_install_session_in_scope(
                    &catalog(OLD_REVISION, "1.0.0"),
                    "uncharted",
                    agent.as_str(),
                    scope,
                    &TestTransport,
                    &TestFactory(fixture.staging.clone()),
                )
                .expect("adapter install session");
                let installed = session
                    .commit_and_record_at(&fixture.store, 100, &NoInstallFaults, &NoReceiptFaults)
                    .expect("adapter install");
                assert_eq!(installed.schema_version(), 5);
                assert_eq!(installed.agent(), agent.as_str());
                assert!(
                    installed
                        .receipt_filename()
                        .expect("v5 key")
                        .starts_with("v5-")
                );
            }

            for scope in &scopes {
                let inspected = fixture.store.inspect_scope(scope).expect("offline check");
                let findings = classify(&catalog(NEW_REVISION, "1.1.0"), inspected.installations);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].status(), CheckStatus::UpdateAvailable);

                let plan = match prepare_update_in_scope(
                    &catalog(NEW_REVISION, "1.1.0"),
                    "uncharted",
                    agent.as_str(),
                    scope,
                    &fixture.store,
                    &TestTransport,
                    &TestFactory(fixture.staging.clone()),
                )
                .expect("adapter update plan")
                {
                    UpdatePreparation::Ready(plan) => *plan,
                    UpdatePreparation::AlreadyUpToDate(_) => panic!("different pin must update"),
                };
                let summary = fixture
                    .store
                    .update(&plan, &NoUpdateFaults)
                    .expect("adapter update");
                assert_eq!(summary.agent(), agent.as_str());
                assert_eq!(summary.scope(), scope.scope().as_str());
                let after = fixture.store.inspect_scope(scope).expect("updated check");
                let findings = classify(&catalog(NEW_REVISION, "1.1.0"), after.installations);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].status(), CheckStatus::UpToDate);
            }

            let receipts = fixture.store.list().expect("updated adapter receipts");
            assert_eq!(receipts.len(), 2);
            assert!(receipts.iter().all(|receipt| {
                receipt.agent() == agent.as_str()
                    && receipt.schema_version() == 5
                    && receipt.source_commit() == NEW_REVISION
            }));
        }
    }

    struct NeverTransport;
    impl GithubTransport for NeverTransport {
        fn get(&self, _: &str) -> Result<GithubResponse, TransportError> {
            panic!("same-revision and validation cases must not fetch")
        }
    }

    #[test]
    fn failed_update_boundaries_recover_forward_and_remain_list_blocked_until_complete() {
        for operation in [
            UpdateOperation::BackupRename,
            UpdateOperation::CommitReplacement,
            UpdateOperation::ReceiptBackupRename,
            UpdateOperation::PublishReceipt,
            UpdateOperation::CleanupFile,
        ] {
            let fixture = Fixture::new();
            let old = fixture.install_old();
            let plan = fixture.prepare_new();
            let error = fixture
                .store
                .update(&plan, &FailBefore(operation))
                .expect_err("injected failure is non-success");
            assert!(matches!(error, UpdateError::Incomplete { .. }), "{error}");
            assert!(
                fixture.store.list().is_err(),
                "pending {operation:?} blocks listing"
            );
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recover operation");
            let receipts = fixture.store.list().expect("recovered receipt visible");
            assert_eq!(receipts.len(), 1, "operation={operation:?}");
            assert_eq!(
                receipts[0].source_commit(),
                NEW_REVISION,
                "operation={operation:?}"
            );
            assert_eq!(receipts[0].schema_version(), 2, "operation={operation:?}");
            assert_eq!(
                receipts[0].installed_directory_identity(),
                Some(
                    SecureDir::open_absolute(&receipts[0].destination_path())
                        .expect("new destination")
                        .identity()
                        .expect("new identity")
                ),
                "operation={operation:?}"
            );
            assert_ne!(receipts[0], old, "operation={operation:?}");
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recovery is idempotent");
            assert_eq!(fixture.store.list().expect("still visible").len(), 1);
        }
    }

    #[test]
    fn failures_after_mutations_are_non_successful_and_recover_forward() {
        for operation in [
            UpdateOperation::BackupRename,
            UpdateOperation::CommitReplacement,
            UpdateOperation::ReceiptBackupRename,
            UpdateOperation::PublishReceipt,
            UpdateOperation::CleanupFile,
            UpdateOperation::CleanupDirectory,
            UpdateOperation::CleanupReceiptBackup,
        ] {
            let fixture = Fixture::new();
            fixture.install_old();
            let plan = fixture.prepare_new();
            let error = fixture
                .store
                .update(&plan, &FailAfter(operation))
                .expect_err("post-mutation failure cannot report success");
            assert!(matches!(error, UpdateError::Incomplete { .. }), "{error}");
            assert!(
                fixture.store.list().is_err(),
                "pending {operation:?} blocks listing"
            );
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recovery resolves the recorded post-mutation boundary");
            let receipt = fixture.store.list().expect("receipt is active").remove(0);
            assert_eq!(receipt.source_commit(), NEW_REVISION, "{operation:?}");
            assert_eq!(receipt.schema_version(), 2, "{operation:?}");
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recovery is idempotent");
        }
    }

    #[test]
    fn update_crash_child() {
        let (Ok(root), Ok(operation)) = (
            std::env::var("THRN_UPDATE_CRASH_ROOT"),
            std::env::var("THRN_UPDATE_CRASH_AFTER"),
        ) else {
            return;
        };
        let operation = parse_crash_operation(&operation).expect("known crash boundary");
        let fixture = Fixture::from_root(PathBuf::from(root), None);
        let user_scope = std::env::var_os("THRN_UPDATE_CRASH_SCOPE").is_some();
        let agent = std::env::var("THRN_UPDATE_CRASH_AGENT").unwrap_or_else(|_| "codex".into());
        let plan = if let Some(agent_id) = crate::agents::AgentId::from_identifier(&agent)
            .filter(|agent_id| *agent_id != crate::agents::AgentId::Codex)
        {
            let scope = if user_scope {
                ResolvedScope::from_agent_validated(
                    agent_id,
                    InstallScope::User,
                    fixture.home.clone(),
                )
            } else {
                ResolvedScope::from_agent_validated(
                    agent_id,
                    InstallScope::Project,
                    fixture.project.clone(),
                )
            };
            match prepare_update_in_scope(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                &agent,
                &scope,
                &fixture.store,
                &TestTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .expect("prepare adapter crash update")
            {
                UpdatePreparation::Ready(plan) => *plan,
                UpdatePreparation::AlreadyUpToDate(_) => panic!("new pin differs"),
            }
        } else if user_scope {
            fixture.prepare_new_user()
        } else {
            fixture.prepare_new()
        };
        let _ = fixture.store.update(&plan, &CrashAfter(operation));
        panic!("fault injector did not terminate at {operation:?}");
    }

    #[test]
    fn update_lock_child() {
        let (Ok(lock_path), Ok(ready_path)) = (
            std::env::var("THRN_UPDATE_LOCK_PATH"),
            std::env::var("THRN_UPDATE_LOCK_READY"),
        ) else {
            return;
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("existing lock file");
        file.try_lock()
            .expect("acquire OS writer lock without waiting");
        fs::write(ready_path, b"ready").expect("signal lock readiness");
        std::thread::sleep(Duration::from_millis(1800));
    }

    #[test]
    fn subprocess_crashes_recover_idempotently_or_preserve_uncertain_pre_intent_objects() {
        let recoverable = [
            UpdateOperation::BackupRename,
            UpdateOperation::CommitReplacement,
            UpdateOperation::ReceiptBackupRename,
            UpdateOperation::PublishReceipt,
            UpdateOperation::CleanupFile,
            UpdateOperation::CleanupDirectory,
            UpdateOperation::CleanupReceiptBackup,
        ];
        for operation in recoverable {
            let fixture = Fixture::new();
            fixture.install_old();
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::update::tests::update_crash_child")
                .arg("--nocapture")
                .env(
                    "THRN_UPDATE_CRASH_ROOT",
                    fixture._parent.as_ref().expect("fixture root").path(),
                )
                .env("THRN_UPDATE_CRASH_AFTER", format!("{operation:?}"))
                .status()
                .expect("crash child starts");
            assert_eq!(status.code(), Some(79), "crash at {operation:?}");
            assert!(
                fixture.store.list().is_err(),
                "crashed {operation:?} blocks listing"
            );
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .unwrap_or_else(|error| panic!("recovering {operation:?}: {error}"));
            let receipt = fixture.store.list().expect("recovered receipt").remove(0);
            assert_eq!(receipt.source_commit(), NEW_REVISION, "{operation:?}");
            assert_eq!(receipt.schema_version(), 2, "{operation:?}");
            fixture
                .store
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("second recovery is idempotent");
        }

        for operation in [
            UpdateOperation::CreateStage,
            UpdateOperation::PrepareReceipt,
        ] {
            let fixture = Fixture::new();
            let old = fixture.install_old();
            let old_skill_md = fs::read(old.destination_path().join("SKILL.md"))
                .expect("old installation content");
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::update::tests::update_crash_child")
                .arg("--nocapture")
                .env(
                    "THRN_UPDATE_CRASH_ROOT",
                    fixture._parent.as_ref().expect("fixture root").path(),
                )
                .env("THRN_UPDATE_CRASH_AFTER", format!("{operation:?}"))
                .status()
                .expect("crash child starts");
            assert_eq!(status.code(), Some(79), "crash at {operation:?}");
            assert!(
                fixture
                    .store
                    .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                    .is_err()
            );
            assert!(
                fixture.store.list().is_err(),
                "uncertain {operation:?} blocks listing"
            );
            if operation == UpdateOperation::CreateStage {
                assert_eq!(
                    fs::read(old.destination_path().join("SKILL.md")).expect("old tree remains"),
                    old_skill_md
                );
            } else {
                let destination = old.destination_path();
                assert_eq!(
                    fs::read(destination.join("SKILL.md")).expect("new tree remains"),
                    b"---\nname: uncharted-ideation\ndescription: Synthetic update skill.\n---\nNew version.\n"
                );
                assert!(
                    fs::read_dir(fixture.data.join("receipts"))
                        .expect("receipt store directory")
                        .any(|entry| entry
                            .expect("receipt entry")
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".pending-update-")),
                    "unrecorded receipt temporary remains for inspection"
                );
            }
        }
    }

    #[test]
    fn user_scope_update_crash_recovers_schema_v3_receipt_without_touching_project_scope() {
        let fixture = Fixture::new();
        let project_receipt = fixture.install_old();
        let user_receipt = fixture.install_old_user();
        let project_skill_md = fs::read(project_receipt.destination_path().join("SKILL.md"))
            .expect("project skill before user update");
        assert_eq!(user_receipt.scope(), "user");
        assert_eq!(user_receipt.schema_version(), 3);

        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::update::tests::update_crash_child")
            .arg("--nocapture")
            .env(
                "THRN_UPDATE_CRASH_ROOT",
                fixture._parent.as_ref().expect("fixture root").path(),
            )
            .env("THRN_UPDATE_CRASH_AFTER", "CommitReplacement")
            .env("THRN_UPDATE_CRASH_SCOPE", "user")
            .status()
            .expect("user update crash child starts");
        assert_eq!(status.code(), Some(79));
        assert!(fixture.store.list().is_err());
        fixture
            .store
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("user update crash recovers");

        let receipts = fixture.store.list().expect("receipts after recovery");
        assert_eq!(receipts.len(), 2);
        let user = receipts
            .iter()
            .find(|receipt| receipt.scope() == "user")
            .expect("user receipt remains");
        let project = receipts
            .iter()
            .find(|receipt| receipt.scope() == "project")
            .expect("project receipt remains");
        assert_eq!(user.schema_version(), 3);
        assert_eq!(user.source_commit(), NEW_REVISION);
        assert_eq!(project.source_commit(), OLD_REVISION);
        assert_eq!(
            project.destination_path(),
            project_receipt.destination_path()
        );
        assert_eq!(
            fs::read(project_receipt.destination_path().join("SKILL.md"))
                .expect("project tree remains old"),
            project_skill_md
        );
        fixture
            .store
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("user update recovery is idempotent");
    }

    #[test]
    fn claude_update_crash_during_receipt_transition_recovers_only_claude() {
        for operation in [
            UpdateOperation::ReceiptBackupRename,
            UpdateOperation::PublishReceipt,
        ] {
            let fixture = Fixture::new();
            fixture.install_old();
            let old_claude = fixture.install_old_claude();
            assert_eq!(old_claude.schema_version(), 4);

            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::update::tests::update_crash_child")
                .arg("--nocapture")
                .env(
                    "THRN_UPDATE_CRASH_ROOT",
                    fixture._parent.as_ref().expect("fixture root").path(),
                )
                .env("THRN_UPDATE_CRASH_AFTER", format!("{operation:?}"))
                .env("THRN_UPDATE_CRASH_AGENT", "claude-code")
                .status()
                .expect("Claude update crash child starts");
            assert_eq!(status.code(), Some(79), "crash at {operation:?}");
            assert!(fixture.store.list().is_err());

            let journal_path = fs::read_dir(fixture.data.join("updates"))
                .expect("pending Claude update journal directory")
                .next()
                .expect("one pending update journal")
                .expect("journal entry")
                .path();
            let journal: serde_json::Value = serde_json::from_slice(
                &fs::read(journal_path).expect("Claude update journal bytes"),
            )
            .expect("Claude update journal JSON");
            assert_eq!(journal["schemaVersion"], 3);
            assert_eq!(journal["oldReceipt"]["agent"], "claude-code");
            assert_eq!(journal["oldReceipt"]["scope"], "project");

            let scope = fixture.claude_project_scope();
            fixture
                .store
                .recover_scope_with_faults(&scope, &NoJournalFaults, &NoReceiptFaults)
                .unwrap_or_else(|error| panic!("recover Claude update {operation:?}: {error}"));
            fixture
                .store
                .recover_scope_with_faults(&scope, &NoJournalFaults, &NoReceiptFaults)
                .expect("Claude update recovery is idempotent");

            let receipts = fixture.store.list().expect("both receipts remain");
            assert_eq!(receipts.len(), 2);
            let claude = receipts
                .iter()
                .find(|receipt| receipt.agent() == "claude-code")
                .expect("updated Claude receipt");
            let codex = receipts
                .iter()
                .find(|receipt| receipt.agent() == "codex")
                .expect("unchanged Codex receipt");
            assert_eq!(claude.schema_version(), 4);
            assert_eq!(claude.source_commit(), NEW_REVISION);
            assert_eq!(codex.source_commit(), OLD_REVISION);
        }
    }

    #[test]
    fn cursor_and_gemini_update_crashes_recover_only_the_selected_v5_receipt() {
        for agent in [InstallAgent::Cursor, InstallAgent::Gemini] {
            for operation in [
                UpdateOperation::ReceiptBackupRename,
                UpdateOperation::PublishReceipt,
            ] {
                let fixture = Fixture::new();
                let old_adapter = fixture.install_old_adapter(agent);
                assert_eq!(old_adapter.schema_version(), 5);

                let status = Command::new(std::env::current_exe().expect("test executable"))
                    .arg("--exact")
                    .arg("install::update::tests::update_crash_child")
                    .arg("--nocapture")
                    .env(
                        "THRN_UPDATE_CRASH_ROOT",
                        fixture._parent.as_ref().expect("fixture root").path(),
                    )
                    .env("THRN_UPDATE_CRASH_AFTER", format!("{operation:?}"))
                    .env("THRN_UPDATE_CRASH_AGENT", agent.as_str())
                    .status()
                    .expect("adapter update crash child starts");
                assert_eq!(
                    status.code(),
                    Some(79),
                    "agent={} op={operation:?}",
                    agent.as_str()
                );
                assert!(fixture.store.list().is_err());

                let journal_path = fs::read_dir(fixture.data.join("updates"))
                    .expect("pending adapter update journal directory")
                    .next()
                    .expect("pending adapter journal")
                    .expect("journal entry")
                    .path();
                let journal: serde_json::Value = serde_json::from_slice(
                    &fs::read(journal_path).expect("adapter update journal bytes"),
                )
                .expect("adapter update journal JSON");
                assert_eq!(journal["schemaVersion"], 4);
                assert_eq!(journal["oldReceipt"]["schemaVersion"], 5);
                assert_eq!(journal["oldReceipt"]["agent"], agent.as_str());
                assert_eq!(journal["oldReceipt"]["scope"], "project");

                let scope = ResolvedScope::from_agent_validated(
                    agent,
                    InstallScope::Project,
                    fixture.project.clone(),
                );
                fixture
                    .store
                    .recover_scope_with_faults(&scope, &NoJournalFaults, &NoReceiptFaults)
                    .unwrap_or_else(|error| {
                        panic!("recover {} update {operation:?}: {error}", agent.as_str())
                    });
                fixture
                    .store
                    .recover_scope_with_faults(&scope, &NoJournalFaults, &NoReceiptFaults)
                    .expect("adapter update recovery is idempotent");

                let receipts = fixture
                    .store
                    .list()
                    .expect("all adapter receipts recovered");
                assert_eq!(receipts.len(), 1);
                let updated = receipts
                    .iter()
                    .find(|receipt| receipt.agent() == agent.as_str())
                    .expect("selected adapter receipt");
                assert_eq!(updated.schema_version(), 5);
                assert_eq!(updated.source_commit(), NEW_REVISION);
            }
        }
    }

    #[test]
    fn update_install_remove_listing_lock_contention_is_bounded_and_exclusive() {
        let fixture = Fixture::new();
        let old = fixture.install_old();
        let plan = fixture.prepare_new();
        let lock_path = fixture.data.join("receipts/.writer.lock");
        let ready_path = fixture.data.join("lock-ready");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("install::update::tests::update_lock_child")
            .arg("--nocapture")
            .env("THRN_UPDATE_LOCK_PATH", &lock_path)
            .env("THRN_UPDATE_LOCK_READY", &ready_path)
            .spawn()
            .expect("lock child starts");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready_path.exists(),
            "lock child acquired lock before deadline"
        );

        assert!(fixture.store.update(&plan, &NoUpdateFaults).is_err());
        assert!(matches!(
            fixture.store.reserve(old.clone(), &NoReceiptFaults),
            Err(ReceiptStoreError::BusyOrInterrupted)
        ));
        assert!(
            fixture
                .store
                .remove_by_id(
                    "uncharted",
                    Some(&fixture.project),
                    &fixture.project,
                    &NoRemovalFaults
                )
                .is_err()
        );
        assert!(fixture.store.list().is_err());

        let wait_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll lock child") {
                break status;
            }
            if std::time::Instant::now() >= wait_deadline {
                let _ = child.kill();
                panic!("lock child did not exit before the deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        fixture
            .store
            .update(&plan, &NoUpdateFaults)
            .expect("update after release");
    }

    #[test]
    fn cursor_and_gemini_updates_and_removals_share_the_os_writer_lock() {
        for agent in [InstallAgent::Cursor, InstallAgent::Gemini] {
            let fixture = Fixture::new();
            let old = fixture.install_old_adapter(agent);
            let scope = ResolvedScope::from_agent_validated(
                agent,
                InstallScope::Project,
                fixture.project.clone(),
            );
            let plan = match prepare_update_in_scope(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                agent.as_str(),
                &scope,
                &fixture.store,
                &TestTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .expect("adapter update plan")
            {
                UpdatePreparation::Ready(plan) => *plan,
                UpdatePreparation::AlreadyUpToDate(_) => panic!("new pin must differ"),
            };
            let lock_path = fixture.data.join("receipts/.writer.lock");
            let ready_path = fixture.data.join("lock-ready");
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("install::update::tests::update_lock_child")
                .arg("--nocapture")
                .env("THRN_UPDATE_LOCK_PATH", &lock_path)
                .env("THRN_UPDATE_LOCK_READY", &ready_path)
                .spawn()
                .expect("adapter lock child starts");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !ready_path.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                ready_path.exists(),
                "agent={} lock acquired",
                agent.as_str()
            );

            assert!(fixture.store.update(&plan, &NoUpdateFaults).is_err());
            assert!(matches!(
                fixture.store.reserve(old.clone(), &NoReceiptFaults),
                Err(ReceiptStoreError::BusyOrInterrupted)
            ));
            assert!(
                fixture
                    .store
                    .remove_by_id_in_scope("uncharted", &scope, &NoRemovalFaults)
                    .is_err()
            );
            let other_agent = if agent == InstallAgent::Cursor {
                InstallAgent::Gemini
            } else {
                InstallAgent::Cursor
            };
            let other_scope = ResolvedScope::from_agent_validated(
                other_agent,
                InstallScope::Project,
                fixture.project.clone(),
            );
            assert!(
                fixture
                    .store
                    .remove_by_id_in_scope("uncharted", &other_scope, &NoRemovalFaults)
                    .is_err()
            );
            assert!(old.destination_path().join("SKILL.md").is_file());
            assert!(
                !other_scope
                    .root()
                    .join(other_agent.adapter().skill_directory())
                    .exists()
            );
            assert!(fixture.store.list().is_err());

            let wait_deadline = std::time::Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().expect("poll adapter lock child") {
                    break status;
                }
                if std::time::Instant::now() >= wait_deadline {
                    let _ = child.kill();
                    panic!("adapter lock child did not exit before deadline");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(status.success());
            fixture
                .store
                .update(&plan, &NoUpdateFaults)
                .unwrap_or_else(|error| panic!("agent={} update: {error}", agent.as_str()));
            fixture
                .store
                .remove_by_id_in_scope("uncharted", &scope, &NoRemovalFaults)
                .unwrap_or_else(|error| panic!("agent={} removal: {error}", agent.as_str()));
            assert!(fixture.store.list().expect("receipts retired").is_empty());
        }
    }

    #[test]
    fn modified_or_extra_installed_content_is_rejected_before_update_staging() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let modified = receipt.destination_path().join("assets/changed.bin");
        std::fs::write(&modified, [0, 255, 9]).expect("same-length modification");
        let error = prepare_update(
            &catalog(NEW_REVISION, "1.1.0"),
            "uncharted",
            "codex",
            None,
            &fixture.project,
            &fixture.store,
            &NeverTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect_err("modified same-length content conflicts");
        assert!(
            error
                .to_string()
                .contains("installed files or directories differ")
        );
        assert_eq!(
            std::fs::read_dir(&fixture.staging)
                .expect("no fetched replacement")
                .count(),
            0
        );

        for extra_directory in [false, true] {
            let fixture = Fixture::new();
            let receipt = fixture.install_old();
            let extra = receipt.destination_path().join(if extra_directory {
                "user-added-directory"
            } else {
                "user-added.txt"
            });
            if extra_directory {
                fs::create_dir(&extra).expect("extra directory");
            } else {
                fs::write(&extra, b"unrelated user data").expect("extra file");
            }
            let error = prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .expect_err("extra objects conflict");
            assert!(!error.to_string().is_empty());
            assert!(extra.exists(), "extra user object remains");
        }

        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let missing = receipt.destination_path().join("assets/changed.bin");
        fs::remove_file(&missing).expect("remove receipt-listed file");
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn byte_identical_replacement_directory_and_legacy_receipt_are_not_update_authority() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let destination = receipt.destination_path();
        let displaced = fixture
            ._parent
            .as_ref()
            .expect("fixture root")
            .path()
            .join("displaced");
        fs::rename(&destination, &displaced).expect("preserve original installation");
        copy_tree(&displaced, &destination);
        assert_eq!(
            fs::read(destination.join("SKILL.md")).expect("copied content"),
            fs::read(displaced.join("SKILL.md")).expect("original content")
        );
        let error = prepare_update(
            &catalog(NEW_REVISION, "1.1.0"),
            "uncharted",
            "codex",
            None,
            &fixture.project,
            &fixture.store,
            &NeverTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect_err("byte-identical new directory is not the installed object");
        assert!(error.to_string().contains("identity"));
        assert!(displaced.join("SKILL.md").is_file());

        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let legacy = receipt.clone().into_legacy_for_test();
        let receipt_path = fixture
            .data
            .join("receipts")
            .join(receipt.receipt_filename().expect("receipt filename"));
        fs::write(
            &receipt_path,
            legacy.to_json().expect("valid legacy receipt"),
        )
        .expect("replace receipt contents in place");
        let error = prepare_update(
            &catalog(NEW_REVISION, "1.1.0"),
            "uncharted",
            "codex",
            None,
            &fixture.project,
            &fixture.store,
            &NeverTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect_err("schema-v1 remains listable but cannot authorize update");
        assert!(error.to_string().contains("schema-v1"));
        assert_eq!(
            fixture.store.list().expect("legacy receipt is listable")[0],
            legacy
        );
    }

    #[test]
    fn corrupt_receipts_and_journals_fail_closed_without_fetch_or_mutation() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let receipt_path = fixture
            .data
            .join("receipts")
            .join(receipt.receipt_filename().expect("receipt filename"));
        fs::write(&receipt_path, b"{not valid receipt").expect("corrupt receipt fixture");
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert!(receipt.destination_path().join("SKILL.md").is_file());

        let fixture = Fixture::new();
        fixture.install_old();
        let updates = fixture.data.join("updates");
        fs::create_dir(&updates).expect("update journal directory");
        fs::write(updates.join("unexpected.json"), b"{}").expect("corrupt journal object");
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert!(
            fixture
                .project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn conflicting_receipt_after_old_receipt_backup_is_preserved_for_inspection() {
        let fixture = Fixture::new();
        let old = fixture.install_old();
        let plan = fixture.prepare_new();
        fixture
            .store
            .update(&plan, &FailAfter(UpdateOperation::ReceiptBackupRename))
            .expect_err("receipt backup fault is incomplete");
        let name = old.receipt_filename().expect("deterministic receipt name");
        let active_path = fixture.data.join("receipts").join(&name);
        let conflict = b"conflicting receipt object";
        fs::write(&active_path, conflict).expect("place conflicting active receipt");
        let error = fixture
            .store
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect_err("conflicting receipt is not replaced or adopted");
        assert!(error.to_string().contains("update"));
        assert_eq!(
            fs::read(&active_path).expect("conflict preserved"),
            conflict
        );
        assert!(fixture.store.list().is_err());
        assert!(
            fixture
                .data
                .join("receipts")
                .read_dir()
                .expect("receipt backups")
                .any(|entry| entry
                    .expect("receipt entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".thrn-update-"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_rejects_symlink_substitution_and_preserves_external_target() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let file = receipt.destination_path().join("assets/changed.bin");
        let external = fixture
            ._parent
            .as_ref()
            .expect("fixture root")
            .path()
            .join("outside.bin");
        fs::write(&external, b"outside user data").expect("external sentinel");
        fs::remove_file(&file).expect("remove managed path for hostile fixture");
        symlink(&external, &file).expect("substitute symbolic link");
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert_eq!(
            fs::read(external).expect("external content remains"),
            b"outside user data"
        );
    }

    #[cfg(windows)]
    #[test]
    fn update_rejects_real_windows_destination_junction_and_preserves_target() {
        let fixture = Fixture::new();
        let receipt = fixture.install_old();
        let destination = receipt.destination_path();
        let displaced = fixture
            ._parent
            .as_ref()
            .expect("fixture root")
            .path()
            .join("displaced");
        let external = fixture
            ._parent
            .as_ref()
            .expect("fixture root")
            .path()
            .join("external");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("keep.txt"), b"outside user data").expect("external sentinel");
        fs::rename(&destination, &displaced).expect("preserve installed directory");
        crate::install::windows_test_support::create_junction(&destination, &external);
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("external content remains"),
            b"outside user data"
        );
        assert!(displaced.join("SKILL.md").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn update_rejects_case_insensitive_destination_collision() {
        let fixture = Fixture::new();
        fixture.install_old();
        let collision = fixture.project.join(".agents/skills/Uncharted-Ideation");
        fs::create_dir(&collision).expect("case collision");
        fs::write(collision.join("keep.txt"), b"user data").expect("collision sentinel");
        assert!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .is_err()
        );
        assert_eq!(
            fs::read(collision.join("keep.txt")).expect("preserved"),
            b"user data"
        );
    }

    fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
        fs::create_dir(destination).expect("copy destination directory");
        for entry in fs::read_dir(source).expect("source tree") {
            let entry = entry.expect("source entry");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().expect("source object type").is_dir() {
                copy_tree(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).expect("copy file");
            }
        }
    }

    #[test]
    fn exact_canonical_id_and_catalog_source_identity_are_required() {
        let fixture = Fixture::new();
        fixture.install_old();
        assert!(matches!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "Uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            ),
            Err(UpdateError::NonCanonicalId)
        ));
        let changed_source = catalog_json_with_source(
            NEW_REVISION,
            "1.1.0",
            "Elsewhere-Org",
            "catalog-test-fixtures",
            "skills/uncharted-ideation",
            "uncharted-ideation",
        );
        let error = prepare_update(
            &parse_catalog_json(changed_source.as_bytes()).expect("valid catalog"),
            "uncharted",
            "codex",
            None,
            &fixture.project,
            &fixture.store,
            &NeverTransport,
            &TestFactory(fixture.staging.clone()),
        )
        .expect_err("repository transfer blocked before fetch");
        assert!(error.to_string().contains("owner/repository"));

        for (owner, repository, path, skill_name) in [
            (
                "Example-Org",
                "transferred-repository",
                "skills/uncharted-ideation",
                "uncharted-ideation",
            ),
            (
                "Example-Org",
                "catalog-test-fixtures",
                "skills/renamed-source",
                "uncharted-ideation",
            ),
            (
                "Example-Org",
                "catalog-test-fixtures",
                "skills/uncharted-ideation",
                "renamed-skill",
            ),
        ] {
            let changed = catalog_json_with_source(
                NEW_REVISION,
                "1.1.0",
                owner,
                repository,
                path,
                skill_name,
            );
            let changed = parse_catalog_json(changed.as_bytes()).expect("valid catalog");
            let error = prepare_update(
                &changed,
                "uncharted",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            )
            .expect_err("changed source identity is rejected before fetching");
            assert!(error.to_string().contains("owner/repository"));
        }

        assert!(matches!(
            prepare_update(
                &catalog(NEW_REVISION, "1.1.0"),
                "missing-resource",
                "codex",
                None,
                &fixture.project,
                &fixture.store,
                &NeverTransport,
                &TestFactory(fixture.staging.clone()),
            ),
            Err(UpdateError::UnknownResource(_))
        ));
    }

    fn catalog_json_with_source(
        revision: &str,
        version: &str,
        owner: &str,
        repository: &str,
        path: &str,
        skill_name: &str,
    ) -> String {
        format!(
            r#"{{"schemaVersion":1,"resources":[{{"id":"uncharted","kind":"skill","displayName":"Uncharted","skillName":"{skill_name}","description":"Synthetic update fixture.","version":"{version}","aliases":[],"source":{{"provider":"github","owner":"{owner}","repository":"{repository}","path":"{path}","commit":"{revision}"}}}}]}}"#
        )
    }
}
