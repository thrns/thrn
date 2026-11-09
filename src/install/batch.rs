//! Validated multi-agent installation plans and their deterministic target matrix.
//!
//! Planning resolves the catalog resource and fetches/stages its pinned source once. Each
//! selected adapter gets an independent immutable destination plan while sharing the retained
//! stage and validated inventory.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    agents::AgentId,
    models::catalog::{Catalog, SkillId},
    registry::discovery::{CatalogResolver, ResolutionError},
    sources::{
        github::{FetchError, GithubTransport, fetch_and_stage_skill_with_factory},
        staging::{StageError, TempDirFactory},
    },
};

use super::{
    commit::{
        CommitError, InstallFaultInjector, InstallationCompletionError, InstallationSession,
        validate_install_session_preflight,
        validate_install_session_preflight_with_batch_directories,
    },
    commit::{NodeKind, SecureDir},
    destination::{
        ComponentState, DestinationError, DestinationSnapshot, FileIdentity,
        inspect_destination_for_agent,
    },
    inventory::{InventoryError, inspect_staged_skill},
    journal::JournalFaultInjector,
    journal::NoJournalFaults,
    plan::{InstallPlan, InstallScope, PlanError, build_install_plan_for_agent_scope},
    receipt::InstallationReceipt,
    receipt_store::{NoReceiptFaults, ReceiptContentState, ReceiptStore, SharedSkillStatus},
    removal::{NoRemovalFaults, RemovalFaultInjector},
    scope::ResolvedScope,
};

const BATCH_JOURNAL_SCHEMA_VERSION: u64 = 1;
const MAX_BATCH_JOURNAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_BATCH_JOURNAL_ENTRIES: usize = 4096;
const MAX_BATCH_JOURNAL_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const BATCH_TEMP_NAME_ATTEMPTS: usize = 32;

/// Pairwise relation between two explicitly selected agent targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetConflict {
    SameTarget,
    Independent,
    SharedDiscovery {
        shared_agent: AgentId,
        discovering_agent: AgentId,
    },
}

impl TargetConflict {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::SharedDiscovery { .. })
    }
}

/// Ordered, symmetric matrix. Rows and columns follow the user's repeated `--agent` order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetConflictMatrix {
    agents: Vec<AgentId>,
    cells: Vec<Vec<TargetConflict>>,
}

impl TargetConflictMatrix {
    pub fn agents(&self) -> &[AgentId] {
        &self.agents
    }

    pub fn cells(&self) -> &[Vec<TargetConflict>] {
        &self.cells
    }

    pub fn has_conflicts(&self) -> bool {
        self.cells.iter().flatten().any(TargetConflict::is_conflict)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BatchIdentity {
    volume: u64,
    object: u64,
}

impl From<FileIdentity> for BatchIdentity {
    fn from(value: FileIdentity) -> Self {
        Self {
            volume: value.volume(),
            object: value.object(),
        }
    }
}

impl From<BatchIdentity> for FileIdentity {
    fn from(value: BatchIdentity) -> Self {
        Self::new(value.volume, value.object)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BatchPreflightEvidence {
    scope_root_identity: BatchIdentity,
    agent_directory_state: String,
    agent_directory_identity: Option<BatchIdentity>,
    skills_directory_state: String,
    skills_directory_identity: Option<BatchIdentity>,
    destination_was_absent: bool,
    shared_path_was_absent: Option<bool>,
    conflicts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum BatchTargetState {
    Pending,
    Committing,
    Verified,
    CompensationPending,
    Compensated,
    Partial,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum BatchProgress {
    NotStarted,
    InProgress,
    Complete,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum BatchOutcome {
    Installing,
    Compensating,
    Complete,
    Compensated,
    Partial,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BatchTargetRecord {
    agent: String,
    scope: String,
    scope_root: String,
    destination: String,
    expected_receipt_name: String,
    expected_receipt: InstallationReceipt,
    preflight: BatchPreflightEvidence,
    state: BatchTargetState,
    commit_progress: BatchProgress,
    receipt_progress: BatchProgress,
    verification_succeeded: Option<bool>,
    compensation_detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BatchRecord {
    schema_version: u64,
    operation_id: String,
    sequence: u64,
    source_resource_id: String,
    source_owner: String,
    source_repository: String,
    source_commit: String,
    source_directory: String,
    scope: String,
    scope_root: String,
    scope_root_identity: BatchIdentity,
    ordered_agents: Vec<String>,
    conflict_matrix: Vec<Vec<String>>,
    targets: Vec<BatchTargetRecord>,
    outcome: BatchOutcome,
}

impl BatchRecord {
    fn validate(&self) -> Result<(), BatchJournalError> {
        if self.schema_version != BATCH_JOURNAL_SCHEMA_VERSION {
            return Err(BatchJournalError::UnsupportedSchema(self.schema_version));
        }
        if !is_lower_hex(&self.operation_id, 32)
            || self.scope != "project" && self.scope != "user"
            || self.scope_root.is_empty()
            || self.ordered_agents.len() < 2
            || self.ordered_agents.len() > 4
            || self.targets.len() != self.ordered_agents.len()
            || self.conflict_matrix.len() != self.ordered_agents.len()
            || self
                .conflict_matrix
                .iter()
                .any(|row| row.len() != self.ordered_agents.len())
            || self.targets.iter().any(|target| {
                !target.preflight.destination_was_absent
                    || target.preflight.shared_path_was_absent == Some(false)
                    || !target.preflight.conflicts.is_empty()
            })
        {
            return Err(BatchJournalError::InvalidRecord);
        }

        let mut agents = Vec::with_capacity(self.targets.len());
        for (index, target) in self.targets.iter().enumerate() {
            let agent =
                AgentId::from_identifier(&target.agent).ok_or(BatchJournalError::InvalidRecord)?;
            if agents.contains(&agent)
                || self.ordered_agents.get(index).map(String::as_str) != Some(target.agent.as_str())
                || target.scope != self.scope
                || target.scope_root != self.scope_root
                || Path::new(&target.destination).is_relative()
                || target.expected_receipt.agent() != target.agent
                || target.expected_receipt.scope() != target.scope
                || target.expected_receipt.root_path() != Path::new(&target.scope_root)
                || target.expected_receipt.destination_path() != Path::new(&target.destination)
                || target.expected_receipt.catalog_resource_id() != self.source_resource_id
                || target.expected_receipt.source_owner() != self.source_owner
                || target.expected_receipt.source_repository() != self.source_repository
                || target.expected_receipt.source_commit() != self.source_commit
                || target.expected_receipt.source_directory() != self.source_directory
                || target.expected_receipt.scope_root_identity()
                    != FileIdentity::from(target.preflight.scope_root_identity)
                || target.expected_receipt_name
                    != target
                        .expected_receipt
                        .receipt_filename()
                        .map_err(|_| BatchJournalError::InvalidRecord)?
            {
                return Err(BatchJournalError::InvalidRecord);
            }
            if target
                .expected_receipt
                .installed_directory_identity()
                .is_some()
            {
                target
                    .expected_receipt
                    .validate()
                    .map_err(|_| BatchJournalError::InvalidRecord)?;
            } else {
                target
                    .expected_receipt
                    .validate_install_intent()
                    .map_err(|_| BatchJournalError::InvalidRecord)?;
            }
            if (target.state == BatchTargetState::Verified
                && (target.verification_succeeded != Some(true)
                    || target.commit_progress != BatchProgress::Complete
                    || target.receipt_progress != BatchProgress::Complete
                    || target
                        .expected_receipt
                        .installed_directory_identity()
                        .is_none()))
                || (target.state == BatchTargetState::Pending
                    && (target.commit_progress != BatchProgress::NotStarted
                        || target.receipt_progress != BatchProgress::NotStarted))
                || (target.state == BatchTargetState::Committing
                    && target.commit_progress == BatchProgress::NotStarted)
                || (target.state == BatchTargetState::Partial
                    && target.compensation_detail.is_none())
            {
                return Err(BatchJournalError::InvalidRecord);
            }
            agents.push(agent);
        }

        let expected_matrix = target_conflict_matrix(&agents)
            .cells()
            .iter()
            .map(|row| row.iter().map(encode_target_conflict).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if self.conflict_matrix != expected_matrix
            || target_conflict_matrix(&agents).has_conflicts()
        {
            return Err(BatchJournalError::InvalidRecord);
        }

        let verified = self
            .targets
            .iter()
            .all(|target| target.state == BatchTargetState::Verified);
        let all_compensated = self
            .targets
            .iter()
            .all(|target| target.state == BatchTargetState::Compensated);
        let has_partial = self
            .targets
            .iter()
            .any(|target| target.state == BatchTargetState::Partial);
        let outcome_valid = match self.outcome {
            BatchOutcome::Installing => self.targets.iter().all(|target| {
                matches!(
                    target.state,
                    BatchTargetState::Pending
                        | BatchTargetState::Committing
                        | BatchTargetState::Verified
                )
            }),
            BatchOutcome::Compensating => self.targets.iter().all(|target| {
                matches!(
                    target.state,
                    BatchTargetState::CompensationPending | BatchTargetState::Compensated
                )
            }),
            BatchOutcome::Complete => verified,
            BatchOutcome::Compensated => all_compensated,
            BatchOutcome::Partial => {
                has_partial
                    && self.targets.iter().all(|target| {
                        matches!(
                            target.state,
                            BatchTargetState::CompensationPending
                                | BatchTargetState::Compensated
                                | BatchTargetState::Partial
                        )
                    })
            }
        };
        if !outcome_valid {
            return Err(BatchJournalError::InvalidRecord);
        }
        Ok(())
    }

    pub(crate) fn contains_scope(&self, scope: &super::scope::ResolvedScope) -> bool {
        self.targets.iter().any(|target| {
            target.agent == scope.agent().as_str()
                && target.scope == scope.scope().as_str()
                && Path::new(&target.scope_root) == scope.root()
        })
    }

    pub(crate) fn expected_receipts(&self) -> Vec<InstallationReceipt> {
        self.targets
            .iter()
            .map(|target| target.expected_receipt.clone())
            .collect()
    }

    fn same_immutable_identity(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.operation_id == other.operation_id
            && self.source_resource_id == other.source_resource_id
            && self.source_owner == other.source_owner
            && self.source_repository == other.source_repository
            && self.source_commit == other.source_commit
            && self.source_directory == other.source_directory
            && self.scope == other.scope
            && self.scope_root == other.scope_root
            && self.scope_root_identity == other.scope_root_identity
            && self.ordered_agents == other.ordered_agents
            && self.conflict_matrix == other.conflict_matrix
            && self
                .targets
                .iter()
                .zip(&other.targets)
                .all(|(left, right)| {
                    left.agent == right.agent
                        && left.scope == right.scope
                        && left.scope_root == right.scope_root
                        && left.destination == right.destination
                        && left.expected_receipt_name == right.expected_receipt_name
                        && left.preflight == right.preflight
                        && (left.expected_receipt == right.expected_receipt
                            || right.expected_receipt.is_identity_upgrade_of(
                                &left.expected_receipt,
                                right
                                    .expected_receipt
                                    .installed_directory_identity()
                                    .unwrap_or_else(|| FileIdentity::new(0, 0)),
                            ))
                })
    }
}

fn encode_target_conflict(conflict: &TargetConflict) -> String {
    match conflict {
        TargetConflict::SameTarget => "same-target".to_owned(),
        TargetConflict::Independent => "independent".to_owned(),
        TargetConflict::SharedDiscovery {
            shared_agent,
            discovering_agent,
        } => format!(
            "shared-discovery:{}:{}",
            shared_agent.as_str(),
            discovering_agent.as_str()
        ),
    }
}

fn component_state_name(state: ComponentState) -> &'static str {
    match state {
        ComponentState::Missing => "missing",
        ComponentState::Directory => "directory",
        ComponentState::RegularFile => "regularFile",
        ComponentState::Symlink => "symlink",
        ComponentState::Other => "other",
        ComponentState::NotInspected => "notInspected",
    }
}

#[derive(Debug, Error)]
pub(crate) enum BatchJournalError {
    #[error("batch operation journal storage is unsafe")]
    UnsafeDirectory,
    #[error("batch operation journal contains an unknown, linked, or nonregular object")]
    UnsafeObject,
    #[error("batch operation journal has too many records")]
    TooManyEntries,
    #[error("batch operation journal storage exceeds its size limit")]
    StoreOversized,
    #[error("batch operation journal record exceeds its size limit")]
    Oversized,
    #[error("batch operation journal schema version {0} is unsupported")]
    UnsupportedSchema(u64),
    #[error("batch operation journal record is invalid")]
    InvalidRecord,
    #[error("batch operation journal record {0:?} is corrupt or truncated")]
    Corrupt(String),
    #[error("batch operation journal filename does not match its record")]
    FilenameMismatch,
    #[error("batch operation journal contains a duplicate sequence")]
    DuplicateSequence,
    #[error("batch operation journal history has a sequence gap")]
    SequenceGap,
    #[error("batch operation journal transition is invalid")]
    InvalidTransition,
    #[error("interrupted batch operation journal publication remains at {0:?}")]
    InterruptedPublication(String),
    #[error("batch operation journal serialization failed")]
    Serialization,
    #[error("temporary batch operation journal names were exhausted")]
    TemporaryNameExhausted,
    #[error("could not {action} batch operation journal ({kind}, os_error={os_error:?})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
}

fn batch_io(action: &'static str, error: io::Error) -> BatchJournalError {
    BatchJournalError::Io {
        action,
        kind: error.kind(),
        os_error: error.raw_os_error(),
    }
}

pub(crate) struct BatchJournalStore {
    directory: PathBuf,
}

pub(crate) struct BatchJournal {
    directory: SecureDir,
    record: BatchRecord,
}

impl BatchJournalStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn prepare(
        &self,
        mut record: BatchRecord,
    ) -> Result<BatchJournal, BatchJournalError> {
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| batch_io("create batch journal storage", error))?;
        record.schema_version = BATCH_JOURNAL_SCHEMA_VERSION;
        record.operation_id = random_batch_id()?;
        record.sequence = 0;
        record.outcome = BatchOutcome::Installing;
        for target in &mut record.targets {
            target.state = BatchTargetState::Pending;
            target.commit_progress = BatchProgress::NotStarted;
            target.receipt_progress = BatchProgress::NotStarted;
            target.verification_succeeded = None;
            target.compensation_detail = None;
        }
        record.validate()?;
        persist_batch_record(&directory, &record)?;
        Ok(BatchJournal { directory, record })
    }

    pub(crate) fn read_all(&self) -> Result<Vec<BatchRecord>, BatchJournalError> {
        if !self.directory.is_absolute() {
            return Err(BatchJournalError::UnsafeDirectory);
        }
        let directory = match SecureDir::open_absolute(&self.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(batch_io("inspect batch journal storage", error)),
        };
        let entries = directory
            .entries()
            .map_err(|error| batch_io("read batch journal storage", error))?;
        if entries.len() > MAX_BATCH_JOURNAL_ENTRIES {
            return Err(BatchJournalError::TooManyEntries);
        }
        let mut groups = std::collections::BTreeMap::<
            String,
            std::collections::BTreeMap<u64, BatchRecord>,
        >::new();
        let mut total_bytes = 0_u64;
        for (entry, kind) in entries {
            let name = entry.to_str().ok_or(BatchJournalError::UnsafeObject)?;
            if name.starts_with(".pending-") {
                return Err(BatchJournalError::InterruptedPublication(name.to_owned()));
            }
            let (operation_id, sequence) =
                parse_batch_filename(name).ok_or(BatchJournalError::UnsafeObject)?;
            if kind != NodeKind::RegularFile {
                return Err(BatchJournalError::UnsafeObject);
            }
            let mut file = directory
                .open_regular_file(name)
                .map_err(|error| batch_io("open batch journal record", error))?;
            let length = file
                .metadata()
                .map_err(|error| batch_io("inspect batch journal record", error))?
                .len();
            if length > MAX_BATCH_JOURNAL_BYTES as u64 {
                return Err(BatchJournalError::Oversized);
            }
            total_bytes = total_bytes
                .checked_add(length)
                .filter(|total| *total <= MAX_BATCH_JOURNAL_TOTAL_BYTES)
                .ok_or(BatchJournalError::StoreOversized)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_BATCH_JOURNAL_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| batch_io("read batch journal record", error))?;
            if bytes.len() > MAX_BATCH_JOURNAL_BYTES {
                return Err(BatchJournalError::Oversized);
            }
            let record: BatchRecord = serde_json::from_slice(&bytes)
                .map_err(|_| BatchJournalError::Corrupt(name.to_owned()))?;
            record.validate()?;
            if record.operation_id != operation_id || record.sequence != sequence {
                return Err(BatchJournalError::FilenameMismatch);
            }
            let records = groups.entry(operation_id.to_owned()).or_default();
            if records.insert(sequence, record).is_some() {
                return Err(BatchJournalError::DuplicateSequence);
            }
        }

        let mut latest = Vec::new();
        for (_, records) in groups {
            let terminal = records.last_key_value().is_some_and(|(_, record)| {
                matches!(
                    record.outcome,
                    BatchOutcome::Complete | BatchOutcome::Compensated
                )
            });
            let mut previous: Option<(u64, &BatchRecord)> = None;
            for (sequence, record) in &records {
                if let Some((previous_sequence, previous_record)) = &previous {
                    if *sequence != previous_sequence + 1 && !terminal {
                        return Err(BatchJournalError::SequenceGap);
                    }
                    if !previous_record.same_immutable_identity(record)
                        || *sequence == previous_sequence + 1
                            && !valid_batch_successor(previous_record, record)
                    {
                        return Err(BatchJournalError::InvalidTransition);
                    }
                } else if !terminal
                    && (*sequence != 0 || record.outcome != BatchOutcome::Installing)
                {
                    return Err(BatchJournalError::SequenceGap);
                }
                previous = Some((*sequence, record));
            }
            if let Some((_, record)) = records.into_iter().next_back() {
                latest.push(record);
            }
        }
        latest.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        Ok(latest)
    }

    pub(crate) fn pending_count(&self) -> Result<usize, BatchJournalError> {
        Ok(self.read_all()?.len())
    }

    pub(crate) fn resume(&self, record: BatchRecord) -> Result<BatchJournal, BatchJournalError> {
        record.validate()?;
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| batch_io("open batch journal storage", error))?;
        Ok(BatchJournal { directory, record })
    }

    pub(crate) fn cleanup_completed(&self, operation_id: &str) -> Result<(), BatchJournalError> {
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| batch_io("open completed batch journal", error))?;
        let latest = self
            .read_all()?
            .into_iter()
            .find(|record| record.operation_id == operation_id)
            .ok_or(BatchJournalError::InvalidRecord)?;
        if !matches!(
            latest.outcome,
            BatchOutcome::Complete | BatchOutcome::Compensated
        ) {
            return Err(BatchJournalError::InvalidTransition);
        }
        let mut names = directory
            .entries()
            .map_err(|error| batch_io("inspect completed batch journal", error))?
            .into_iter()
            .filter_map(|(name, kind)| {
                let name = name.to_str()?.to_owned();
                let (candidate, sequence) = parse_batch_filename(&name)?;
                (candidate == operation_id).then_some((sequence, name, kind))
            })
            .collect::<Vec<_>>();
        names.sort_by_key(|entry| entry.0);
        for (_, name, kind) in names {
            if kind != NodeKind::RegularFile {
                return Err(BatchJournalError::UnsafeObject);
            }
            directory
                .remove_file(&name)
                .map_err(|error| batch_io("clean completed batch journal", error))?;
        }
        directory
            .sync_directory()
            .map_err(|error| batch_io("synchronize batch journal cleanup", error))?;
        Ok(())
    }
}

impl BatchJournal {
    pub(crate) fn record(&self) -> &BatchRecord {
        &self.record
    }

    pub(crate) fn begin_target(&mut self, index: usize) -> Result<(), BatchJournalError> {
        self.update(|record| {
            if let Some(target) = record.targets.get_mut(index) {
                target.state = BatchTargetState::Committing;
                target.commit_progress = BatchProgress::InProgress;
                target.receipt_progress = BatchProgress::NotStarted;
            }
        })
    }

    fn record_failure_progress(
        &mut self,
        index: usize,
        commit_progress: BatchProgress,
        receipt_progress: BatchProgress,
    ) -> Result<(), BatchJournalError> {
        self.update(|record| {
            if let Some(target) = record.targets.get_mut(index) {
                target.commit_progress = commit_progress;
                target.receipt_progress = receipt_progress;
            }
        })
    }

    pub(crate) fn mark_verified(
        &mut self,
        index: usize,
        receipt: InstallationReceipt,
    ) -> Result<(), BatchJournalError> {
        self.update(|record| {
            if let Some(target) = record.targets.get_mut(index) {
                target.expected_receipt = receipt;
                target.state = BatchTargetState::Verified;
                target.commit_progress = BatchProgress::Complete;
                target.receipt_progress = BatchProgress::Complete;
                target.verification_succeeded = Some(true);
            }
        })
    }

    pub(crate) fn begin_compensation_all(&mut self) -> Result<(), BatchJournalError> {
        self.update(|record| {
            record.outcome = BatchOutcome::Compensating;
            for target in &mut record.targets {
                target.state = match target.state {
                    BatchTargetState::Pending => BatchTargetState::Compensated,
                    BatchTargetState::Committing | BatchTargetState::Verified => {
                        BatchTargetState::CompensationPending
                    }
                    other => other,
                };
            }
        })
    }

    pub(crate) fn mark_compensated(&mut self, index: usize) -> Result<(), BatchJournalError> {
        self.update(|record| {
            if let Some(target) = record.targets.get_mut(index) {
                target.state = BatchTargetState::Compensated;
                target.compensation_detail = None;
            }
        })
    }

    pub(crate) fn mark_partial(
        &mut self,
        index: usize,
        detail: String,
    ) -> Result<(), BatchJournalError> {
        self.update(|record| {
            record.outcome = BatchOutcome::Partial;
            if let Some(target) = record.targets.get_mut(index) {
                target.state = BatchTargetState::Partial;
                target.compensation_detail = Some(detail);
            }
        })
    }

    pub(crate) fn finish_complete(&mut self) -> Result<(), BatchJournalError> {
        self.update(|record| record.outcome = BatchOutcome::Complete)
    }

    pub(crate) fn finish_compensated(&mut self) -> Result<(), BatchJournalError> {
        self.update(|record| record.outcome = BatchOutcome::Compensated)
    }

    fn retry_compensation(&mut self) -> Result<(), BatchJournalError> {
        self.update(|record| {
            record.outcome = BatchOutcome::Compensating;
            for target in &mut record.targets {
                if target.state == BatchTargetState::Partial {
                    target.state = BatchTargetState::CompensationPending;
                    target.compensation_detail = None;
                }
            }
        })
    }

    fn update(&mut self, update: impl FnOnce(&mut BatchRecord)) -> Result<(), BatchJournalError> {
        let mut next = self.record.clone();
        next.sequence = next
            .sequence
            .checked_add(1)
            .ok_or(BatchJournalError::InvalidRecord)?;
        update(&mut next);
        next.validate()?;
        if !valid_batch_successor(&self.record, &next) {
            return Err(BatchJournalError::InvalidTransition);
        }
        persist_batch_record(&self.directory, &next)?;
        self.record = next;
        Ok(())
    }
}

fn valid_batch_successor(previous: &BatchRecord, next: &BatchRecord) -> bool {
    let outcome_valid = match (previous.outcome, next.outcome) {
        (
            BatchOutcome::Installing,
            BatchOutcome::Installing
            | BatchOutcome::Compensating
            | BatchOutcome::Complete
            | BatchOutcome::Partial,
        ) => true,
        (
            BatchOutcome::Compensating,
            BatchOutcome::Compensating | BatchOutcome::Compensated | BatchOutcome::Partial,
        ) => true,
        (BatchOutcome::Partial, BatchOutcome::Compensating | BatchOutcome::Partial) => true,
        (left, right) => left == right,
    };
    if !outcome_valid {
        return false;
    }
    previous
        .targets
        .iter()
        .zip(&next.targets)
        .all(|(left, right)| {
            let state_valid = match (left.state, right.state) {
                (
                    BatchTargetState::Pending,
                    BatchTargetState::Committing
                    | BatchTargetState::Compensated
                    | BatchTargetState::Partial,
                ) => true,
                (
                    BatchTargetState::Committing,
                    BatchTargetState::Verified
                    | BatchTargetState::CompensationPending
                    | BatchTargetState::Partial,
                ) => true,
                (BatchTargetState::Verified, BatchTargetState::CompensationPending) => true,
                (BatchTargetState::Partial, BatchTargetState::CompensationPending) => true,
                (
                    BatchTargetState::CompensationPending,
                    BatchTargetState::Compensated | BatchTargetState::Partial,
                ) => true,
                (left, right) => left == right,
            };
            let receipt_valid = left.expected_receipt == right.expected_receipt
                || right.expected_receipt.is_identity_upgrade_of(
                    &left.expected_receipt,
                    right
                        .expected_receipt
                        .installed_directory_identity()
                        .unwrap_or_else(|| FileIdentity::new(0, 0)),
                );
            state_valid
                && receipt_valid
                && valid_progress_successor(left.commit_progress, right.commit_progress)
                && valid_progress_successor(left.receipt_progress, right.receipt_progress)
        })
}

fn valid_progress_successor(previous: BatchProgress, next: BatchProgress) -> bool {
    match (previous, next) {
        (
            BatchProgress::NotStarted,
            BatchProgress::InProgress
            | BatchProgress::Complete
            | BatchProgress::Failed
            | BatchProgress::Unknown,
        ) => true,
        (
            BatchProgress::InProgress,
            BatchProgress::Complete | BatchProgress::Failed | BatchProgress::Unknown,
        ) => true,
        (
            BatchProgress::Failed | BatchProgress::Unknown,
            BatchProgress::Complete | BatchProgress::Failed | BatchProgress::Unknown,
        ) => true,
        (left, right) => left == right,
    }
}

fn persist_batch_record(
    directory: &SecureDir,
    record: &BatchRecord,
) -> Result<(), BatchJournalError> {
    let bytes = serde_json::to_vec(record).map_err(|_| BatchJournalError::Serialization)?;
    if bytes.len() > MAX_BATCH_JOURNAL_BYTES {
        return Err(BatchJournalError::Oversized);
    }
    let filename = batch_filename(&record.operation_id, record.sequence);
    let mut temporary_file = None;
    for _ in 0..BATCH_TEMP_NAME_ATTEMPTS {
        let temporary = format!(".pending-{}", random_batch_id()?);
        match directory.create_regular_file(&temporary) {
            Ok(file) => {
                temporary_file = Some((temporary, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(batch_io("create pending batch record", error)),
        }
    }
    let (temporary, mut file) = temporary_file.ok_or(BatchJournalError::TemporaryNameExhausted)?;
    let identity = SecureDir::file_identity(&file)
        .map_err(|error| batch_io("identify pending batch record", error))?;
    let mut active = true;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|error| batch_io("write pending batch record", error))?;
        file.flush()
            .map_err(|error| batch_io("flush pending batch record", error))?;
        file.sync_all()
            .map_err(|error| batch_io("synchronize pending batch record", error))?;
        let verify = directory
            .open_regular_file(&temporary)
            .map_err(|error| batch_io("verify pending batch record", error))?;
        let mut verified_bytes = Vec::new();
        verify
            .take(MAX_BATCH_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut verified_bytes)
            .map_err(|error| batch_io("read pending batch record", error))?;
        let verified: BatchRecord = serde_json::from_slice(&verified_bytes)
            .map_err(|_| BatchJournalError::Serialization)?;
        if verified != *record
            || SecureDir::file_identity(&file)
                .map_err(|error| batch_io("reidentify pending batch record", error))?
                != identity
        {
            return Err(BatchJournalError::InvalidRecord);
        }
        directory
            .rename_file_child_noreplace(&temporary, &filename, &file)
            .map_err(|error| batch_io("publish batch journal record without replacement", error))?;
        active = false;
        directory
            .sync_directory()
            .map_err(|error| batch_io("synchronize batch journal directory", error))?;
        Ok(())
    })();
    if active
        && let Ok(named) = directory.open_regular_file(&temporary)
        && SecureDir::file_identity(&named).ok() == Some(identity)
    {
        let _ = directory.remove_file(&temporary);
        let _ = directory.sync_directory();
    }
    result
}

fn random_batch_id() -> Result<String, BatchJournalError> {
    let mut bytes = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| BatchJournalError::Serialization)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn batch_filename(operation_id: &str, sequence: u64) -> String {
    format!("{operation_id}-{sequence:016x}.json")
}

fn parse_batch_filename(filename: &str) -> Option<(&str, u64)> {
    let (stem, extension) = filename.rsplit_once('.')?;
    if extension != "json" {
        return None;
    }
    let (operation_id, sequence) = stem.split_once('-')?;
    if !is_lower_hex(operation_id, 32) || !is_lower_hex(sequence, 16) {
        return None;
    }
    Some((operation_id, u64::from_str_radix(sequence, 16).ok()?))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One target's complete install plan, including retained source data for secure commit.
pub struct MultiInstallTarget {
    session: InstallationSession,
}

impl MultiInstallTarget {
    pub fn plan(&self) -> &InstallPlan {
        self.session.plan()
    }

    pub fn agent(&self) -> AgentId {
        self.session.plan().agent()
    }

    pub(crate) fn session(&self) -> &InstallationSession {
        &self.session
    }
}

/// Immutable complete plan for one canonical resource and a single explicit scope.
pub struct MultiInstallPlan {
    resource_id: SkillId,
    scope: InstallScope,
    scope_root: std::path::PathBuf,
    targets: Vec<MultiInstallTarget>,
    conflict_matrix: TargetConflictMatrix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingTargetState {
    Absent,
    VerifiedManaged,
    ManagedContentChanged,
    Unmanaged,
    FilesystemConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchConflictDetail {
    pub agent: Option<AgentId>,
    pub path: PathBuf,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiInstallPreflight {
    pub existing_targets: Vec<(AgentId, ExistingTargetState)>,
    pub conflicts: Vec<BatchConflictDetail>,
}

impl MultiInstallPreflight {
    pub fn is_eligible(&self) -> bool {
        self.conflicts.is_empty()
    }
}

impl MultiInstallPlan {
    pub fn resource_id(&self) -> &SkillId {
        &self.resource_id
    }

    pub fn scope(&self) -> InstallScope {
        self.scope
    }

    pub fn scope_root(&self) -> &Path {
        &self.scope_root
    }

    pub fn targets(&self) -> &[MultiInstallTarget] {
        &self.targets
    }

    pub fn conflict_matrix(&self) -> &TargetConflictMatrix {
        &self.conflict_matrix
    }

    pub fn is_eligible_without_store_conflicts(&self) -> bool {
        !self.conflict_matrix.has_conflicts()
            && self
                .targets
                .iter()
                .all(|target| target.plan().conflicts().is_empty())
    }

    pub(crate) fn initial_journal_record(
        &self,
        installed_at_unix_seconds: u64,
    ) -> Result<BatchRecord, BatchJournalError> {
        if !self.is_eligible_without_store_conflicts() {
            return Err(BatchJournalError::InvalidRecord);
        }
        let first = self
            .targets
            .first()
            .ok_or(BatchJournalError::InvalidRecord)?;
        let first_plan = first.plan();
        let scope_root = self
            .scope_root
            .to_str()
            .ok_or(BatchJournalError::InvalidRecord)?
            .to_owned();
        let scope_root_identity = first_plan
            .destination_snapshot()
            .project_identity()
            .ok_or(BatchJournalError::InvalidRecord)?;
        let mut targets = Vec::with_capacity(self.targets.len());
        for target in &self.targets {
            let plan = target.plan();
            let snapshot = plan.destination_snapshot();
            let root_identity = snapshot
                .project_identity()
                .ok_or(BatchJournalError::InvalidRecord)?;
            if root_identity != scope_root_identity
                || !plan.conflicts().is_empty()
                || snapshot.skill_state() != ComponentState::Missing
            {
                return Err(BatchJournalError::InvalidRecord);
            }
            let receipt = InstallationReceipt::from_committed_plan(
                plan,
                target.session.inventory(),
                root_identity,
                installed_at_unix_seconds,
            )
            .map_err(|_| BatchJournalError::InvalidRecord)?;
            let expected_receipt_name = receipt
                .receipt_filename()
                .map_err(|_| BatchJournalError::InvalidRecord)?;
            let shared_path_was_absent = plan.shared_path_snapshot().map(|shared| {
                shared.skill_state() == ComponentState::Missing && shared.conflicts().is_empty()
            });
            let preflight = BatchPreflightEvidence {
                scope_root_identity: root_identity.into(),
                agent_directory_state: component_state_name(snapshot.agents_state()).to_owned(),
                agent_directory_identity: snapshot.agents_identity().map(Into::into),
                skills_directory_state: component_state_name(snapshot.skills_state()).to_owned(),
                skills_directory_identity: snapshot.skills_identity().map(Into::into),
                destination_was_absent: true,
                shared_path_was_absent,
                conflicts: Vec::new(),
            };
            targets.push(BatchTargetRecord {
                agent: target.agent().as_str().to_owned(),
                scope: self.scope.as_str().to_owned(),
                scope_root: scope_root.clone(),
                destination: plan
                    .destination()
                    .to_str()
                    .ok_or(BatchJournalError::InvalidRecord)?
                    .to_owned(),
                expected_receipt_name,
                expected_receipt: receipt,
                preflight,
                state: BatchTargetState::Pending,
                commit_progress: BatchProgress::NotStarted,
                receipt_progress: BatchProgress::NotStarted,
                verification_succeeded: None,
                compensation_detail: None,
            });
        }
        let matrix = self
            .conflict_matrix
            .cells()
            .iter()
            .map(|row| row.iter().map(encode_target_conflict).collect())
            .collect();
        Ok(BatchRecord {
            schema_version: BATCH_JOURNAL_SCHEMA_VERSION,
            operation_id: String::new(),
            sequence: 0,
            source_resource_id: first_plan.resource_id().as_str().to_owned(),
            source_owner: first_plan.source_owner().as_str().to_owned(),
            source_repository: first_plan.source_repository().as_str().to_owned(),
            source_commit: first_plan.source_commit().as_str().to_owned(),
            source_directory: first_plan.source_directory().as_str().to_owned(),
            scope: self.scope.as_str().to_owned(),
            scope_root,
            scope_root_identity: scope_root_identity.into(),
            ordered_agents: self
                .targets
                .iter()
                .map(|target| target.agent().as_str().to_owned())
                .collect(),
            conflict_matrix: matrix,
            targets,
            outcome: BatchOutcome::Installing,
        })
    }
}

#[derive(Debug, Error)]
pub enum MultiInstallPlanError {
    #[error(transparent)]
    Resolution(#[from] ResolutionError),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Stage(#[from] StageError),
    #[error(transparent)]
    Inventory(#[from] InventoryError),
    #[error(transparent)]
    Destination(#[from] DestinationError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("a multi-agent install requires at least one explicit target")]
    EmptyTargets,
    #[error("the same agent was selected more than once: {0}")]
    DuplicateAgent(&'static str),
    #[error("all multi-agent targets must use the same scope and validated root")]
    MixedScope,
}

#[derive(Debug, Error)]
pub(crate) enum MultiInstallPreflightError {
    #[error("multi-agent target scopes do not match the validated plan")]
    ScopeCountMismatch,
    #[error(transparent)]
    ReceiptStore(#[from] super::receipt_store::ReceiptStoreError),
}

#[derive(Debug, Error)]
pub(crate) enum BatchInstallError {
    #[error("multi-agent preflight conflict; no batch targets were mutated: {0}")]
    PreflightConflict(String),
    #[error("batch operation journal could not be prepared; no targets were mutated: {0}")]
    JournalPrepare(String),
    #[error(
        "target {agent} failed before directory commitment; batch compensation succeeded: {detail}"
    )]
    CompensatedBeforeCommit { agent: String, detail: String },
    #[error(
        "target {agent} committed but receipt publication was incomplete; batch compensation succeeded: {detail}"
    )]
    CompensatedReceiptPublication { agent: String, detail: String },
    #[error(
        "target {agent} committed but post-commit verification or target-journal completion was incomplete; batch compensation succeeded: {detail}"
    )]
    CompensatedPostCommit { agent: String, detail: String },
    #[error("partial batch requires inspection; target {agent}: {detail}")]
    Partial { agent: String, detail: String },
    #[error("all target receipts verified, but batch journal finalization is incomplete: {0}")]
    Finalization(String),
    #[error(transparent)]
    Plan(#[from] MultiInstallPreflightError),
}

struct BatchNoInstallFaults;
impl InstallFaultInjector for BatchNoInstallFaults {}

#[derive(Clone, Copy)]
enum TargetFailureStage {
    BeforeDirectoryCommit,
    UnknownCommit,
    DirectoryCommittedBeforeReceipt,
    ReceiptPublication,
    PostCommit,
}

pub(crate) fn execute_multi_install(
    plan: &MultiInstallPlan,
    scopes: &[ResolvedScope],
    store: &ReceiptStore,
) -> Result<String, BatchInstallError> {
    execute_multi_install_with_faults(
        plan,
        scopes,
        store,
        &BatchNoInstallFaults,
        &NoReceiptFaults,
        &NoJournalFaults,
    )
}

pub(crate) fn execute_multi_install_with_faults(
    plan: &MultiInstallPlan,
    scopes: &[ResolvedScope],
    store: &ReceiptStore,
    install_faults: &dyn InstallFaultInjector,
    receipt_faults: &dyn super::receipt_store::ReceiptFaultInjector,
    journal_faults: &dyn JournalFaultInjector,
) -> Result<String, BatchInstallError> {
    let faults = BatchFaults {
        install: install_faults,
        receipt: receipt_faults,
        journal: journal_faults,
        removal: &NoRemovalFaults,
        #[cfg(test)]
        batch: &NoBatchFaults,
    };
    execute_multi_install_impl(plan, scopes, store, &faults)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatchCrashPoint {
    TargetVerified(usize),
    CompensationIntentRecorded,
    CompleteOutcomeRecorded,
}

#[cfg(test)]
pub(crate) trait BatchFaultInjector: Send + Sync {
    fn reached(&self, _point: BatchCrashPoint) {}
}

#[cfg(test)]
struct NoBatchFaults;

#[cfg(test)]
impl BatchFaultInjector for NoBatchFaults {}

struct BatchFaults<'a> {
    install: &'a dyn InstallFaultInjector,
    receipt: &'a dyn super::receipt_store::ReceiptFaultInjector,
    journal: &'a dyn JournalFaultInjector,
    removal: &'a dyn RemovalFaultInjector,
    #[cfg(test)]
    batch: &'a dyn BatchFaultInjector,
}

#[cfg(test)]
pub(crate) struct BatchTestFaults<'a> {
    faults: BatchFaults<'a>,
}

#[cfg(test)]
impl<'a> BatchTestFaults<'a> {
    pub(crate) fn new(
        install: &'a dyn InstallFaultInjector,
        receipt: &'a dyn super::receipt_store::ReceiptFaultInjector,
        journal: &'a dyn JournalFaultInjector,
        removal: &'a dyn RemovalFaultInjector,
        batch: &'a dyn BatchFaultInjector,
    ) -> Self {
        Self {
            faults: BatchFaults {
                install,
                receipt,
                journal,
                removal,
                batch,
            },
        }
    }
}

#[cfg(test)]
pub(crate) fn execute_multi_install_with_test_faults(
    plan: &MultiInstallPlan,
    scopes: &[ResolvedScope],
    store: &ReceiptStore,
    faults: &BatchTestFaults<'_>,
) -> Result<String, BatchInstallError> {
    execute_multi_install_impl(plan, scopes, store, &faults.faults)
}

fn execute_multi_install_impl(
    plan: &MultiInstallPlan,
    scopes: &[ResolvedScope],
    store: &ReceiptStore,
    faults: &BatchFaults<'_>,
) -> Result<String, BatchInstallError> {
    for scope in scopes {
        store
            .recover_scope_with_faults(scope, faults.journal, faults.receipt)
            .map_err(|error| BatchInstallError::PreflightConflict(error.to_string()))?;
    }
    let preflight = inspect_multi_install_preflight(plan, scopes, store)?;
    if !preflight.is_eligible() {
        return Err(BatchInstallError::PreflightConflict(
            format_preflight_conflicts(&preflight.conflicts),
        ));
    }

    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| BatchInstallError::JournalPrepare(error.to_string()))?
        .as_secs();
    let initial_record = plan
        .initial_journal_record(installed_at)
        .map_err(|error| BatchInstallError::JournalPrepare(error.to_string()))?;
    let receipt_intents = initial_record
        .targets
        .iter()
        .map(|target| target.expected_receipt.clone())
        .collect::<Vec<_>>();
    let first_receipt = receipt_intents
        .first()
        .cloned()
        .ok_or_else(|| BatchInstallError::JournalPrepare("empty target list".to_owned()))?;
    let mut reservation = store
        .reserve(first_receipt, faults.receipt)
        .map_err(|error| BatchInstallError::PreflightConflict(error.to_string()))?;
    reservation
        .validate_receipt_intents(&receipt_intents)
        .map_err(|error| BatchInstallError::PreflightConflict(error.to_string()))?;
    for target in &plan.targets {
        validate_install_session_preflight(target.session()).map_err(|error| {
            BatchInstallError::PreflightConflict(format!(
                "target {} changed after planning: {error}",
                target.agent().as_str()
            ))
        })?;
    }

    let journal_store = store
        .batch_journal_store()
        .map_err(|error| BatchInstallError::JournalPrepare(error.to_string()))?;
    let mut journal = journal_store
        .prepare(initial_record)
        .map_err(|error| BatchInstallError::JournalPrepare(error.to_string()))?;
    let mut allowed_batch_directories = Vec::new();

    macro_rules! abort_batch_with_compensation {
        ($index:expr, $detail:expr, $failure_stage:expr $(,)?) => {
            return compensate_failed_batch(
                BatchCompensationContext {
                    store,
                    reservation: &mut reservation,
                    journal_store: &journal_store,
                    journal: &mut journal,
                    faults,
                },
                $index,
                $detail,
                $failure_stage,
            );
        };
    }

    for (index, target) in plan.targets.iter().enumerate() {
        if let Err(error) = validate_install_session_preflight_with_batch_directories(
            target.session(),
            &allowed_batch_directories,
        ) {
            abort_batch_with_compensation!(
                index,
                format!(
                    "target {} changed before its commit: {error}",
                    target.agent().as_str()
                ),
                TargetFailureStage::BeforeDirectoryCommit,
            );
        }
        if let Err(error) = journal.begin_target(index) {
            abort_batch_with_compensation!(
                index,
                format!("batch journal could not record target start: {error}"),
                TargetFailureStage::BeforeDirectoryCommit,
            );
        }
        let result = target
            .session()
            .commit_and_record_under_reservation_at_with_journal(
                &mut reservation,
                installed_at,
                faults.install,
                faults.receipt,
                faults.journal,
                &allowed_batch_directories,
            );
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(error) => {
                let failure_stage = match &error {
                    InstallationCompletionError::ReceiptAfterCommit(_) => {
                        TargetFailureStage::ReceiptPublication
                    }
                    InstallationCompletionError::JournalAfterDirectoryCommit(_) => {
                        TargetFailureStage::DirectoryCommittedBeforeReceipt
                    }
                    InstallationCompletionError::JournalAfterReceipt(_) => {
                        TargetFailureStage::PostCommit
                    }
                    InstallationCompletionError::Commit(
                        CommitError::CommittedButUnverified
                        | CommitError::Operation {
                            operation:
                                super::commit::InstallOperation::CommitNoReplace
                                | super::commit::InstallOperation::SyncDestinationDirectory,
                            ..
                        },
                    ) => TargetFailureStage::UnknownCommit,
                    _ => TargetFailureStage::BeforeDirectoryCommit,
                };
                let detail = record_failure_progress(&mut journal, index, failure_stage)
                    .map(|()| error.to_string())
                    .unwrap_or_else(|journal_error| {
                        format!(
                            "{}; batch could not persist target progress: {journal_error}",
                            error
                        )
                    });
                abort_batch_with_compensation!(index, detail, failure_stage,);
            }
        };
        if let Err(error) = reservation.verify_published_receipt(&receipt) {
            let failure_stage = TargetFailureStage::PostCommit;
            let detail = record_failure_progress(&mut journal, index, failure_stage)
                .map(|()| format!("published target receipt failed verification: {error}"))
                .unwrap_or_else(|journal_error| {
                    format!(
                        "published target receipt verification failed: {error}; batch could not persist target progress: {journal_error}"
                    )
                });
            abort_batch_with_compensation!(index, detail, failure_stage,);
        }
        if target.plan().destination_snapshot().agents_state() == ComponentState::Missing {
            let current = match inspect_destination_for_agent(
                target.agent(),
                Some(target.plan().scope_root()),
                target.plan().scope_root(),
                target.plan().skill_name().as_str(),
            ) {
                Ok(current) => current,
                Err(error) => {
                    let failure_stage = TargetFailureStage::PostCommit;
                    let detail = record_failure_progress(&mut journal, index, failure_stage)
                        .map(|()| {
                            format!(
                                "committed target parent identity could not be verified: {error}"
                            )
                        })
                        .unwrap_or_else(|journal_error| {
                            format!(
                                "committed target parent identity could not be verified: {error}; batch could not persist target progress: {journal_error}"
                            )
                        });
                    abort_batch_with_compensation!(index, detail, failure_stage,);
                }
            };
            let Some(identity) = current.agents_identity() else {
                let failure_stage = TargetFailureStage::PostCommit;
                let detail = record_failure_progress(&mut journal, index, failure_stage)
                    .map(|()| {
                        "committed target parent has no stable filesystem identity".to_owned()
                    })
                    .unwrap_or_else(|journal_error| {
                        format!(
                            "committed target parent has no stable filesystem identity; batch could not persist target progress: {journal_error}"
                        )
                    });
                abort_batch_with_compensation!(index, detail, failure_stage,);
            };
            allowed_batch_directories.push((
                target.agent().adapter().skill_directory().to_owned(),
                identity,
            ));
        }
        if let Err(error) = journal.mark_verified(index, receipt) {
            let failure_stage = TargetFailureStage::PostCommit;
            let detail = record_failure_progress(&mut journal, index, failure_stage)
                .map(|()| format!("batch journal could not record verified target: {error}"))
                .unwrap_or_else(|journal_error| {
                    format!(
                        "batch journal could not record verified target: {error}; target progress update failed: {journal_error}"
                    )
                });
            abort_batch_with_compensation!(index, detail, failure_stage,);
        }
        #[cfg(test)]
        faults.batch.reached(BatchCrashPoint::TargetVerified(index));
    }

    if let Err(error) = journal.finish_complete() {
        return Err(BatchInstallError::Finalization(error.to_string()));
    }
    #[cfg(test)]
    faults
        .batch
        .reached(BatchCrashPoint::CompleteOutcomeRecorded);
    let output = format_multi_install_success(plan, journal.record());
    let operation_id = journal.record().operation_id.clone();
    journal_store
        .cleanup_completed(&operation_id)
        .map_err(|error| BatchInstallError::Finalization(error.to_string()))?;
    reservation
        .release()
        .map_err(|error| BatchInstallError::Finalization(error.to_string()))?;
    Ok(output)
}

/// Resume the one durable multi-target operation while the receipt store's shared writer lock is
/// already held. Child installation/removal journals are recovered before batch state is read.
pub(crate) fn recover_all(
    store: &ReceiptStore,
    reservation: &mut super::receipt_store::ReceiptReservation,
    records: Vec<BatchRecord>,
) -> Result<(), String> {
    if records.len() != 1 {
        return Err("multiple batch operation journals require manual inspection".to_owned());
    }
    let journal_store = store
        .batch_journal_store()
        .map_err(|error| error.to_string())?;
    let record = records
        .into_iter()
        .next()
        .ok_or_else(|| "batch operation journal is missing".to_owned())?;
    store
        .recover_batch_children_under_reservation(reservation, &record.expected_receipts())
        .map_err(|error| format!("nested target recovery failed safely: {error}"))?;

    let mut journal = journal_store
        .resume(record)
        .map_err(|error| format!("batch operation journal could not be resumed: {error}"))?;
    match journal.record().outcome {
        BatchOutcome::Installing => {
            let mut all_verified = true;
            for index in 0..journal.record().targets.len() {
                let target = &journal.record().targets[index];
                let state = target.state;
                let expected = target.expected_receipt.clone();
                let current = store
                    .inspect_batch_target_receipt(reservation, &expected)
                    .map_err(|error| {
                        format!(
                            "target {} could not be verified; content is preserved: {error}",
                            target.agent
                        )
                    })?;
                match (state, current) {
                    (BatchTargetState::Pending, Some(_)) => {
                        return Err(format!(
                            "target {} has receipt evidence before its commit state; preserving it for inspection",
                            target.agent
                        ));
                    }
                    (BatchTargetState::Verified, None) => {
                        return Err(format!(
                            "verified target {} is now absent; preserving batch state for inspection",
                            target.agent
                        ));
                    }
                    (BatchTargetState::Committing, Some(receipt)) => {
                        journal
                            .mark_verified(index, receipt)
                            .map_err(|error| error.to_string())?;
                    }
                    (BatchTargetState::Verified, Some(_)) => {}
                    (BatchTargetState::Pending | BatchTargetState::Committing, None) => {
                        all_verified = false;
                    }
                    (
                        BatchTargetState::CompensationPending
                        | BatchTargetState::Compensated
                        | BatchTargetState::Partial,
                        _,
                    ) => {
                        return Err("installing batch contains compensation state".to_owned());
                    }
                }
            }
            if all_verified
                && journal
                    .record()
                    .targets
                    .iter()
                    .all(|target| target.state == BatchTargetState::Verified)
            {
                journal
                    .finish_complete()
                    .map_err(|error| error.to_string())?;
                verify_terminal_batch(store, reservation, journal.record(), true)?;
                journal_store
                    .cleanup_completed(&journal.record().operation_id)
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            journal
                .begin_compensation_all()
                .map_err(|error| error.to_string())?;
            compensate_during_recovery(store, reservation, &journal_store, &mut journal)
        }
        BatchOutcome::Compensating => {
            compensate_during_recovery(store, reservation, &journal_store, &mut journal)
        }
        BatchOutcome::Partial => {
            journal
                .retry_compensation()
                .map_err(|error| error.to_string())?;
            compensate_during_recovery(store, reservation, &journal_store, &mut journal)
        }
        BatchOutcome::Complete => {
            verify_terminal_batch(store, reservation, journal.record(), true)?;
            journal_store
                .cleanup_completed(&journal.record().operation_id)
                .map_err(|error| error.to_string())
        }
        BatchOutcome::Compensated => {
            verify_terminal_batch(store, reservation, journal.record(), false)?;
            journal_store
                .cleanup_completed(&journal.record().operation_id)
                .map_err(|error| error.to_string())
        }
    }
}

fn compensate_during_recovery(
    store: &ReceiptStore,
    reservation: &mut super::receipt_store::ReceiptReservation,
    journal_store: &BatchJournalStore,
    journal: &mut BatchJournal,
) -> Result<(), String> {
    for index in (0..journal.record().targets.len()).rev() {
        if journal.record().targets[index].state != BatchTargetState::CompensationPending {
            continue;
        }
        let expected = journal.record().targets[index].expected_receipt.clone();
        if let Err(error) = store.compensate_batch_target(reservation, &expected) {
            let detail = error.to_string();
            let _ = journal.mark_partial(index, detail.clone());
            return Err(format!(
                "target {} compensation remains unresolved; preserving content: {detail}",
                journal.record().targets[index].agent
            ));
        }
        journal
            .mark_compensated(index)
            .map_err(|error| error.to_string())?;
    }
    journal
        .finish_compensated()
        .map_err(|error| error.to_string())?;
    verify_terminal_batch(store, reservation, journal.record(), false)?;
    journal_store
        .cleanup_completed(&journal.record().operation_id)
        .map_err(|error| error.to_string())
}

fn verify_terminal_batch(
    store: &ReceiptStore,
    reservation: &super::receipt_store::ReceiptReservation,
    record: &BatchRecord,
    should_be_installed: bool,
) -> Result<(), String> {
    for target in &record.targets {
        let actual = store
            .inspect_batch_target_receipt(reservation, &target.expected_receipt)
            .map_err(|error| error.to_string())?;
        if should_be_installed != actual.is_some() {
            return Err(format!(
                "terminal batch target {} no longer matches its recorded outcome; content is preserved",
                target.agent
            ));
        }
        if let Some(actual) = actual
            && actual != target.expected_receipt
        {
            return Err(format!(
                "terminal batch target {} has a different receipt identity; content is preserved",
                target.agent
            ));
        }
    }
    Ok(())
}

fn record_failure_progress(
    journal: &mut BatchJournal,
    index: usize,
    stage: TargetFailureStage,
) -> Result<(), BatchJournalError> {
    let (commit_progress, receipt_progress) = match stage {
        TargetFailureStage::BeforeDirectoryCommit => {
            (BatchProgress::Failed, BatchProgress::NotStarted)
        }
        TargetFailureStage::UnknownCommit => (BatchProgress::Unknown, BatchProgress::Unknown),
        TargetFailureStage::DirectoryCommittedBeforeReceipt => {
            (BatchProgress::Complete, BatchProgress::NotStarted)
        }
        TargetFailureStage::ReceiptPublication => (BatchProgress::Complete, BatchProgress::Failed),
        TargetFailureStage::PostCommit => (BatchProgress::Complete, BatchProgress::Complete),
    };
    journal.record_failure_progress(index, commit_progress, receipt_progress)
}

struct BatchCompensationContext<'a> {
    store: &'a ReceiptStore,
    reservation: &'a mut super::receipt_store::ReceiptReservation,
    journal_store: &'a BatchJournalStore,
    journal: &'a mut BatchJournal,
    faults: &'a BatchFaults<'a>,
}

fn compensate_failed_batch(
    context: BatchCompensationContext<'_>,
    failed_index: usize,
    detail: String,
    failure_stage: TargetFailureStage,
) -> Result<String, BatchInstallError> {
    let BatchCompensationContext {
        store,
        reservation,
        journal_store,
        journal,
        faults,
    } = context;
    let failed_agent = journal
        .record()
        .targets
        .get(failed_index)
        .map(|target| target.agent.clone())
        .unwrap_or_else(|| "unknown".to_owned());
    if let Err(error) = journal.begin_compensation_all() {
        return Err(BatchInstallError::Partial {
            agent: failed_agent,
            detail: format!("{detail}; compensation could not be journaled: {error}"),
        });
    }
    #[cfg(test)]
    faults
        .batch
        .reached(BatchCrashPoint::CompensationIntentRecorded);

    if let Err(error) = store.recover_batch_children_under_reservation(
        reservation,
        &journal.record().expected_receipts(),
    ) {
        let _ = journal.mark_partial(failed_index, error.to_string());
        return Err(BatchInstallError::Partial {
            agent: failed_agent,
            detail: format!("{detail}; nested target recovery stopped safely: {error}"),
        });
    }

    for index in (0..journal.record().targets.len()).rev() {
        if journal.record().targets[index].state != BatchTargetState::CompensationPending {
            continue;
        }
        let expected = journal.record().targets[index].expected_receipt.clone();
        if let Err(error) =
            store.compensate_batch_target_with_faults(reservation, &expected, faults.removal)
        {
            let _ = journal.mark_partial(index, error.to_string());
            return Err(BatchInstallError::Partial {
                agent: journal.record().targets[index].agent.clone(),
                detail: format!(
                    "{detail}; verified compensation was unsafe or incomplete: {error}"
                ),
            });
        }
        if let Err(error) = journal.mark_compensated(index) {
            return Err(BatchInstallError::Partial {
                agent: journal.record().targets[index].agent.clone(),
                detail: format!(
                    "{detail}; compensation completed but its batch state could not be recorded: {error}"
                ),
            });
        }
    }
    if let Err(error) = journal.finish_compensated() {
        return Err(BatchInstallError::Partial {
            agent: failed_agent,
            detail: format!("{detail}; compensated state could not be finalized: {error}"),
        });
    }
    let operation_id = journal.record().operation_id.clone();
    if let Err(error) = journal_store.cleanup_completed(&operation_id) {
        return Err(BatchInstallError::Partial {
            agent: failed_agent,
            detail: format!("{detail}; compensated batch journal cleanup failed: {error}"),
        });
    }
    let error = match failure_stage {
        TargetFailureStage::BeforeDirectoryCommit => BatchInstallError::CompensatedBeforeCommit {
            agent: failed_agent,
            detail,
        },
        TargetFailureStage::ReceiptPublication => {
            BatchInstallError::CompensatedReceiptPublication {
                agent: failed_agent,
                detail,
            }
        }
        TargetFailureStage::UnknownCommit
        | TargetFailureStage::DirectoryCommittedBeforeReceipt
        | TargetFailureStage::PostCommit => BatchInstallError::CompensatedPostCommit {
            agent: failed_agent,
            detail,
        },
    };
    Err(error)
}

fn format_multi_install_success(plan: &MultiInstallPlan, record: &BatchRecord) -> String {
    let first = &plan.targets[0].plan();
    let mut output = format!(
        "Installed THRN skill for multiple agents\nResource: {}\nScope: {}\nPinned revision: {}\nTargets completed: {}/{}\n",
        plan.resource_id().as_str(),
        plan.scope.as_str(),
        first.source_commit().as_str(),
        record
            .targets
            .iter()
            .filter(|target| target.state == BatchTargetState::Verified)
            .count(),
        record.targets.len(),
    );
    for (target, receipt) in plan.targets.iter().zip(&record.targets) {
        output.push_str(&format!(
            "\nAgent: {}\nDestination: {}\nFiles installed: {}\nBytes installed: {}\nOwnership receipt: recorded and verified\n",
            target.agent().as_str(),
            safe_batch_path(&target.plan().destination().display().to_string()),
            receipt.expected_receipt.file_count(),
            receipt.expected_receipt.byte_count(),
        ));
    }
    output.push_str("\nBatch status: complete\n");
    output
}

fn format_preflight_conflicts(conflicts: &[BatchConflictDetail]) -> String {
    conflicts
        .iter()
        .map(|conflict| {
            let agent = conflict
                .agent
                .map(|agent| format!("{}: ", agent.as_str()))
                .unwrap_or_default();
            format!(
                "{agent}{} — {}",
                safe_batch_path(&conflict.path.display().to_string()),
                conflict.detail
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn safe_batch_path(path: &str) -> String {
    path.chars().flat_map(char::escape_debug).collect()
}

/// Build every selected target from one catalog resolution, one pinned-source fetch, and one
/// retained validated stage. No target writes occur here.
pub(crate) fn prepare_multi_install_plan<T: GithubTransport + ?Sized>(
    catalog: &Catalog,
    identifier: &str,
    scopes: &[ResolvedScope],
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<MultiInstallPlan, MultiInstallPlanError> {
    if scopes.is_empty() {
        return Err(MultiInstallPlanError::EmptyTargets);
    }
    let first = &scopes[0];
    let mut selected = Vec::new();
    for scope in scopes {
        if selected.contains(&scope.agent()) {
            return Err(MultiInstallPlanError::DuplicateAgent(
                scope.agent().as_str(),
            ));
        }
        selected.push(scope.agent());
        if scope.scope() != first.scope() || scope.root() != first.root() {
            return Err(MultiInstallPlanError::MixedScope);
        }
    }

    let resource = CatalogResolver::new(catalog).resolve(identifier)?;
    let staged = Arc::new(fetch_and_stage_skill_with_factory(
        resource,
        transport,
        tempdir_factory,
    )?);
    let inventory = inspect_staged_skill(&staged)?;
    let mut targets = Vec::with_capacity(scopes.len());
    let mut shared_snapshot: Option<DestinationSnapshot> = None;

    for scope in scopes {
        let agent = scope.agent();
        let destination = inspect_destination_for_agent(
            agent,
            Some(scope.root()),
            scope.root(),
            staged.skill_name().as_str(),
        )?;
        let mut plan = build_install_plan_for_agent_scope(
            &staged,
            &inventory,
            destination,
            agent,
            scope.scope(),
        )?;
        if agent.adapter().discovers_shared_agents_skills() {
            let snapshot = match &shared_snapshot {
                Some(snapshot) => snapshot.clone(),
                None => {
                    let snapshot = inspect_destination_for_agent(
                        AgentId::Codex,
                        Some(scope.root()),
                        scope.root(),
                        staged.skill_name().as_str(),
                    )?;
                    shared_snapshot = Some(snapshot.clone());
                    snapshot
                }
            };
            plan.set_shared_path_snapshot(snapshot);
        }
        targets.push(MultiInstallTarget {
            session: InstallationSession::new_shared(staged.clone(), inventory.clone(), plan),
        });
    }

    let agents = scopes.iter().map(ResolvedScope::agent).collect::<Vec<_>>();
    let conflict_matrix = target_conflict_matrix(&agents);
    Ok(MultiInstallPlan {
        resource_id: staged.catalog_id().clone(),
        scope: first.scope(),
        scope_root: first.root().to_path_buf(),
        targets,
        conflict_matrix,
    })
}

pub(crate) fn inspect_multi_install_preflight(
    plan: &MultiInstallPlan,
    scopes: &[ResolvedScope],
    store: &ReceiptStore,
) -> Result<MultiInstallPreflight, MultiInstallPreflightError> {
    if plan.targets.len() != scopes.len() {
        return Err(MultiInstallPreflightError::ScopeCountMismatch);
    }
    let mut existing_targets = Vec::with_capacity(plan.targets.len());
    let mut conflicts = Vec::new();
    for (target, scope) in plan.targets.iter().zip(scopes) {
        let snapshot = store.inspect_scope(scope)?;
        let matching_receipt = snapshot.installations.iter().find(|installation| {
            installation
                .receipt
                .destination_path()
                .to_string_lossy()
                .eq_ignore_ascii_case(&target.plan().destination().to_string_lossy())
        });
        let existing = match matching_receipt {
            Some(installation) if installation.content == ReceiptContentState::Verified => {
                ExistingTargetState::VerifiedManaged
            }
            Some(_) => ExistingTargetState::ManagedContentChanged,
            None if target.plan().conflicts().iter().any(|conflict| {
                conflict.path() == target.plan().destination()
                    && conflict.kind() == super::destination::ConflictKind::ExistingDirectory
            }) =>
            {
                ExistingTargetState::Unmanaged
            }
            None if target
                .plan()
                .conflicts()
                .iter()
                .any(|conflict| conflict.path() == target.plan().destination()) =>
            {
                ExistingTargetState::FilesystemConflict
            }
            None => ExistingTargetState::Absent,
        };
        existing_targets.push((target.agent(), existing));

        if existing != ExistingTargetState::Absent {
            conflicts.push(BatchConflictDetail {
                agent: Some(target.agent()),
                path: target.plan().destination().to_path_buf(),
                detail: existing_target_detail(existing).to_owned(),
            });
        }
        for conflict in target.plan().conflicts() {
            let detail = if target
                .plan()
                .shared_path_snapshot()
                .is_some_and(|shared| shared.destination() == conflict.path())
            {
                let shared = target
                    .plan()
                    .shared_path_snapshot()
                    .ok_or(MultiInstallPreflightError::ScopeCountMismatch)?;
                let status = store.inspect_shared_skill(
                    plan.scope,
                    plan.scope_root(),
                    target.plan().skill_name().as_str(),
                    shared,
                )?;
                shared_status_detail(status)
            } else {
                conflict.detail().to_owned()
            };
            conflicts.push(BatchConflictDetail {
                agent: Some(target.agent()),
                path: conflict.path().to_path_buf(),
                detail,
            });
        }
    }

    for row in 0..plan.conflict_matrix.agents.len() {
        for column in row + 1..plan.conflict_matrix.agents.len() {
            let relation = &plan.conflict_matrix.cells[row][column];
            if relation.is_conflict() {
                conflicts.push(BatchConflictDetail {
                    agent: None,
                    path: plan.targets[row].plan().destination().to_path_buf(),
                    detail: format!(
                        "selected target pair {} and {} conflicts through shared .agents/skills discovery",
                        plan.conflict_matrix.agents[row].as_str(),
                        plan.conflict_matrix.agents[column].as_str()
                    ),
                });
            }
        }
    }
    conflicts.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| {
                left.agent
                    .map(AgentId::as_str)
                    .cmp(&right.agent.map(AgentId::as_str))
            })
            .then_with(|| left.detail.cmp(&right.detail))
    });
    Ok(MultiInstallPreflight {
        existing_targets,
        conflicts,
    })
}

fn existing_target_detail(state: ExistingTargetState) -> &'static str {
    match state {
        ExistingTargetState::Absent => "absent",
        ExistingTargetState::VerifiedManaged => "verified THRN-managed installation already exists",
        ExistingTargetState::ManagedContentChanged => {
            "THRN ownership receipt exists but the current installation is not fully verified"
        }
        ExistingTargetState::Unmanaged => "unmanaged skill directory already exists",
        ExistingTargetState::FilesystemConflict => {
            "symlink, reparse point, unsupported object, or case-insensitive collision exists"
        }
    }
}

fn shared_status_detail(status: SharedSkillStatus) -> String {
    match status {
        SharedSkillStatus::Absent => "shared skill is absent".to_owned(),
        SharedSkillStatus::ManagedByCodex => {
            "a verified THRN-managed Codex skill occupies the shared path".to_owned()
        }
        SharedSkillStatus::UnverifiedCodexReceipt => {
            "a Codex receipt exists but shared-path content is not verified".to_owned()
        }
        SharedSkillStatus::Unmanaged => "unmanaged shared skill occupies the path".to_owned(),
        SharedSkillStatus::ChangedSincePlanning => "shared path changed after planning".to_owned(),
    }
}

fn target_conflict_matrix(agents: &[AgentId]) -> TargetConflictMatrix {
    let cells = agents
        .iter()
        .enumerate()
        .map(|(row_index, row_agent)| {
            agents
                .iter()
                .enumerate()
                .map(|(column_index, column_agent)| {
                    if row_index == column_index {
                        return TargetConflict::SameTarget;
                    }
                    match (*row_agent, *column_agent) {
                        (AgentId::Codex, AgentId::Cursor | AgentId::Gemini) => {
                            TargetConflict::SharedDiscovery {
                                shared_agent: AgentId::Codex,
                                discovering_agent: *column_agent,
                            }
                        }
                        (AgentId::Cursor | AgentId::Gemini, AgentId::Codex) => {
                            TargetConflict::SharedDiscovery {
                                shared_agent: AgentId::Codex,
                                discovering_agent: *row_agent,
                            }
                        }
                        _ => TargetConflict::Independent,
                    }
                })
                .collect()
        })
        .collect();
    TargetConflictMatrix {
        agents: agents.to_vec(),
        cells,
    }
}
