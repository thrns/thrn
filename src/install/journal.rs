//! Durable, append-only records for internal installation transactions.
//!
//! The journal is deliberately separate from both catalog cache data and ownership receipts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::PathBuf,
};

use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    commit::{NodeKind, SecureDir},
    destination::FileIdentity,
    receipt::{InstallationReceipt, MAX_RECEIPT_BYTES},
};

pub(crate) const MAX_JOURNAL_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_JOURNAL_FILES: usize = 16_384;
pub(crate) const MAX_JOURNAL_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const PROJECT_JOURNAL_SCHEMA_VERSION: u64 = 1;
const USER_JOURNAL_SCHEMA_VERSION: u64 = 2;
const CLAUDE_JOURNAL_SCHEMA_VERSION: u64 = 3;
const TEMP_NAME_ATTEMPTS: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OperationState {
    Prepared,
    FilesystemCommitInProgress,
    DirectoryCommitVerified,
    ReceiptPublicationPending,
    ReceiptPublished,
    Complete,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JournalIdentity {
    volume: u64,
    object: u64,
}

impl From<FileIdentity> for JournalIdentity {
    fn from(value: FileIdentity) -> Self {
        Self {
            volume: value.volume(),
            object: value.object(),
        }
    }
}

impl From<JournalIdentity> for FileIdentity {
    fn from(value: JournalIdentity) -> Self {
        Self::new(value.volume, value.object)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JournalFile {
    path: String,
    identity: JournalIdentity,
    verified: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JournalDirectory {
    path: String,
    identity: JournalIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JournalRecord {
    schema_version: u64,
    operation_id: String,
    sequence: u64,
    state: OperationState,
    receipt: InstallationReceipt,
    staging_name: String,
    create_agents_directory: bool,
    create_skills_directory: bool,
    project_agents_identity: Option<JournalIdentity>,
    project_skills_identity: Option<JournalIdentity>,
    staging_identity: Option<JournalIdentity>,
    destination_identity: Option<JournalIdentity>,
    created_files: Vec<JournalFile>,
    created_directories: Vec<JournalDirectory>,
    cleanup_intents: Vec<String>,
}

impl JournalRecord {
    pub(crate) fn state(&self) -> OperationState {
        self.state
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn receipt(&self) -> &InstallationReceipt {
        &self.receipt
    }

    pub(crate) fn staging_name(&self) -> &str {
        &self.staging_name
    }

    pub(crate) fn staging_identity(&self) -> Option<FileIdentity> {
        self.staging_identity.map(Into::into)
    }

    pub(crate) fn destination_identity(&self) -> Option<FileIdentity> {
        self.destination_identity.map(Into::into)
    }

    pub(crate) fn project_agents_identity(&self) -> Option<FileIdentity> {
        self.project_agents_identity.map(Into::into)
    }

    pub(crate) fn create_agents_directory(&self) -> bool {
        self.create_agents_directory
    }

    pub(crate) fn create_skills_directory(&self) -> bool {
        self.create_skills_directory
    }

    pub(crate) fn project_skills_identity(&self) -> Option<FileIdentity> {
        self.project_skills_identity.map(Into::into)
    }

    pub(crate) fn created_files(&self) -> &[JournalFile] {
        &self.created_files
    }

    pub(crate) fn created_directories(&self) -> &[JournalDirectory] {
        &self.created_directories
    }

    pub(crate) fn has_cleanup_intent(&self, intent: &str) -> bool {
        self.cleanup_intents
            .iter()
            .any(|existing| existing == intent)
    }

    pub(crate) fn validate(&self) -> Result<(), JournalError> {
        let expected_schema = if self.receipt.agent() == "claude-code" {
            CLAUDE_JOURNAL_SCHEMA_VERSION
        } else if self.receipt.scope() == "user" {
            USER_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_JOURNAL_SCHEMA_VERSION
        };
        if self.schema_version != expected_schema {
            return Err(JournalError::UnsupportedSchema(self.schema_version));
        }
        if !is_lower_hex(&self.operation_id, 32)
            || self.staging_name.len() != ".thrn-install-".len() + 32
            || !self.staging_name.starts_with(".thrn-install-")
            || !is_lower_hex(&self.staging_name[".thrn-install-".len()..], 32)
            || self.created_files.len() > 4096
            || self.created_directories.len() > 4096
            || self.created_files.len() + self.created_directories.len() > 4096
            || self.cleanup_intents.len() > 4099
        {
            return Err(JournalError::InvalidRecord);
        }
        self.receipt
            .validate_install_intent()
            .map_err(|_| JournalError::InvalidRecord)?;
        if let Some(receipt_identity) = self.receipt.installed_directory_identity()
            && self.destination_identity.map(Into::into) != Some(receipt_identity)
        {
            return Err(JournalError::InvalidRecord);
        }
        if !self.receipt.is_legacy()
            && !self.receipt.is_user_intent()
            && self.destination_identity.is_none()
        {
            return Err(JournalError::InvalidRecord);
        }
        let receipt_bytes = if self.receipt.is_user_intent() {
            self.receipt.to_journal_intent_json()
        } else {
            self.receipt.to_json()
        }
        .map_err(|_| JournalError::InvalidRecord)?;
        if receipt_bytes.len() > MAX_RECEIPT_BYTES {
            return Err(JournalError::InvalidRecord);
        }
        let expected_files = self
            .receipt
            .files()
            .iter()
            .map(|file| file.path().to_owned())
            .collect::<BTreeSet<_>>();
        let expected_directories = self
            .receipt
            .directories()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut seen_files = BTreeSet::new();
        for file in &self.created_files {
            if !expected_files.contains(&file.path) || !seen_files.insert(file.path.clone()) {
                return Err(JournalError::InvalidRecord);
            }
        }
        let mut seen_directories = BTreeSet::new();
        for directory in &self.created_directories {
            if !expected_directories.contains(&directory.path)
                || !seen_directories.insert(directory.path.clone())
            {
                return Err(JournalError::InvalidRecord);
            }
        }
        let mut previous_intent: Option<&str> = None;
        for intent in &self.cleanup_intents {
            let valid = if let Some(path) = intent.strip_prefix("file:") {
                seen_files.contains(path)
            } else if let Some(path) = intent.strip_prefix("directory:") {
                seen_directories.contains(path)
            } else {
                match intent.as_str() {
                    "staging-root" => self.staging_identity.is_some(),
                    "skills-parent" => {
                        self.create_skills_directory && self.project_skills_identity.is_some()
                    }
                    "agents-parent" => {
                        self.create_agents_directory && self.project_agents_identity.is_some()
                    }
                    _ => false,
                }
            };
            if !valid || previous_intent.is_some_and(|previous| previous >= intent.as_str()) {
                return Err(JournalError::InvalidRecord);
            }
            previous_intent = Some(intent);
        }
        if self.state == OperationState::Complete && !self.cleanup_intents.is_empty() {
            return Err(JournalError::InvalidRecord);
        }
        if matches!(
            self.state,
            OperationState::DirectoryCommitVerified
                | OperationState::ReceiptPublicationPending
                | OperationState::ReceiptPublished
                | OperationState::Complete
        ) && (self.staging_identity.is_none() || self.destination_identity.is_none())
        {
            return Err(JournalError::InvalidRecord);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum JournalOperation {
    CreateStore,
    CreateTemporaryRecord,
    WriteRecord,
    SyncRecord,
    VerifyRecord,
    PublishRecord,
    SyncStore,
    CleanupRecord,
    SetDestinationIdentity,
}

pub(crate) trait JournalFaultInjector: Send + Sync {
    fn before(&self, _operation: JournalOperation) -> io::Result<()> {
        Ok(())
    }

    fn after(&self, _operation: JournalOperation) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct NoJournalFaults;
impl JournalFaultInjector for NoJournalFaults {}

pub(crate) struct JournalStore {
    directory: PathBuf,
}

pub(crate) struct JournalIntent {
    pub(crate) receipt: InstallationReceipt,
    pub(crate) staging_name: String,
    pub(crate) create_agents_directory: bool,
    pub(crate) create_skills_directory: bool,
    pub(crate) agents_identity: Option<FileIdentity>,
    pub(crate) skills_identity: Option<FileIdentity>,
}

impl JournalStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn prepare(
        &self,
        intent: JournalIntent,
        faults: &dyn JournalFaultInjector,
    ) -> Result<OperationJournal, JournalError> {
        faults
            .before(JournalOperation::CreateStore)
            .map_err(|error| map_io("create journal storage", error))?;
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| map_io("create journal storage", error))?;
        faults
            .after(JournalOperation::CreateStore)
            .map_err(|error| map_io("create journal storage", error))?;
        let schema_version = if intent.receipt.agent() == "claude-code" {
            CLAUDE_JOURNAL_SCHEMA_VERSION
        } else if intent.receipt.scope() == "user" {
            USER_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_JOURNAL_SCHEMA_VERSION
        };
        let record = JournalRecord {
            schema_version,
            operation_id: random_id().map_err(|error| map_io("name operation", error))?,
            sequence: 0,
            state: OperationState::Prepared,
            receipt: intent.receipt,
            staging_name: intent.staging_name,
            create_agents_directory: intent.create_agents_directory,
            create_skills_directory: intent.create_skills_directory,
            project_agents_identity: intent.agents_identity.map(Into::into),
            project_skills_identity: intent.skills_identity.map(Into::into),
            staging_identity: None,
            destination_identity: None,
            created_files: Vec::new(),
            created_directories: Vec::new(),
            cleanup_intents: Vec::new(),
        };
        record.validate()?;
        let store = self.clone_store();
        store.persist(&directory, &record, faults)?;
        Ok(OperationJournal {
            store,
            directory,
            record,
        })
    }

    pub(crate) fn resume(&self, record: JournalRecord) -> Result<OperationJournal, JournalError> {
        record.validate()?;
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| map_io("open journal storage", error))?;
        Ok(OperationJournal {
            store: self.clone_store(),
            directory,
            record,
        })
    }

    fn clone_store(&self) -> Self {
        Self {
            directory: self.directory.clone(),
        }
    }

    pub(crate) fn read_all(&self) -> Result<Vec<JournalRecord>, JournalError> {
        if !self.directory.is_absolute() {
            return Err(JournalError::UnsafeDirectory);
        }
        let directory = match SecureDir::open_absolute(&self.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(map_io("inspect journal storage", error)),
        };
        let entries = directory
            .entries()
            .map_err(|error| map_io("read journal storage", error))?;
        if entries.len() > MAX_JOURNAL_FILES {
            return Err(JournalError::TooManyEntries);
        }
        let mut groups = BTreeMap::<String, BTreeMap<u64, JournalRecord>>::new();
        let mut total_bytes = 0_u64;
        for (entry, kind) in entries {
            let name = entry.to_str().ok_or(JournalError::UnsafeObject)?;
            if name.starts_with(".pending-") {
                return Err(JournalError::InterruptedPublication {
                    name: name.to_owned(),
                });
            }
            let Some((operation_id, sequence)) = parse_filename(name) else {
                return Err(JournalError::UnsafeObject);
            };
            if kind != NodeKind::RegularFile {
                return Err(JournalError::UnsafeObject);
            }
            let mut file = directory
                .open_regular_file(name)
                .map_err(|error| map_io("open journal record", error))?;
            let length = file
                .metadata()
                .map_err(|error| map_io("inspect journal record", error))?
                .len();
            if length > MAX_JOURNAL_BYTES as u64 {
                return Err(JournalError::Oversized);
            }
            total_bytes = total_bytes
                .checked_add(length)
                .filter(|total| *total <= MAX_JOURNAL_TOTAL_BYTES)
                .ok_or(JournalError::StoreOversized)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_JOURNAL_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| map_io("read journal record", error))?;
            if bytes.len() > MAX_JOURNAL_BYTES {
                return Err(JournalError::Oversized);
            }
            let record: JournalRecord =
                serde_json::from_slice(&bytes).map_err(|_| JournalError::Corrupt {
                    name: name.to_owned(),
                })?;
            record.validate()?;
            if record.operation_id != operation_id || record.sequence != sequence {
                return Err(JournalError::FilenameMismatch);
            }
            let records = groups.entry(operation_id.to_owned()).or_default();
            if records.insert(sequence, record).is_some() {
                return Err(JournalError::DuplicateSequence);
            }
        }

        let mut latest = Vec::new();
        for (_, records) in groups {
            let terminal = records.last_key_value().is_some_and(|(_, record)| {
                matches!(
                    record.state,
                    OperationState::Complete | OperationState::RolledBack
                )
            });
            let mut previous: Option<(u64, &JournalRecord)> = None;
            for (sequence, record) in &records {
                if let Some((previous_sequence, previous_record)) = &previous {
                    let contiguous = *sequence == previous_sequence + 1;
                    if !contiguous && !terminal {
                        return Err(JournalError::SequenceGap);
                    }
                    if (previous_record.receipt != record.receipt
                        && !record.receipt.is_identity_upgrade_of(
                            &previous_record.receipt,
                            record
                                .destination_identity()
                                .ok_or(JournalError::InvalidTransition)?,
                        ))
                        || previous_record.staging_name != record.staging_name
                        || previous_record.create_agents_directory != record.create_agents_directory
                        || previous_record.create_skills_directory != record.create_skills_directory
                        || (contiguous
                            && (!valid_transition(previous_record.state, record.state)
                                || !valid_successor(previous_record, record)))
                    {
                        return Err(JournalError::InvalidTransition);
                    }
                } else if !terminal && (*sequence != 0 || record.state != OperationState::Prepared)
                {
                    // A complete record may be all that remains after interrupted cleanup.
                    return Err(JournalError::SequenceGap);
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

    pub(crate) fn cleanup_completed(
        &self,
        operation_id: &str,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| map_io("open journal storage", error))?;
        let mut records = directory
            .entries()
            .map_err(|error| map_io("read journal storage", error))?
            .into_iter()
            .filter_map(|(name, kind)| {
                let name = name.to_str()?.to_owned();
                let (id, sequence) = parse_filename(&name)?;
                if id != operation_id {
                    return None;
                }
                Some((sequence, name, kind))
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|entry| entry.0);
        if records.is_empty() {
            return Ok(());
        }
        let Some((latest, _, _)) = records.last() else {
            return Ok(());
        };
        let latest = *latest;
        let latest_name = filename(operation_id, latest);
        let latest_record = directory
            .open_regular_file(&latest_name)
            .map_err(|error| map_io("open completed journal record", error))?;
        let mut bytes = Vec::new();
        latest_record
            .take(MAX_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| map_io("read completed journal record", error))?;
        let record: JournalRecord =
            serde_json::from_slice(&bytes).map_err(|_| JournalError::Corrupt {
                name: latest_name.clone(),
            })?;
        record.validate()?;
        if !matches!(
            record.state,
            OperationState::Complete | OperationState::RolledBack
        ) {
            return Err(JournalError::InvalidTransition);
        }
        let previous_records = records
            .iter()
            .filter(|entry| entry.1 != latest_name)
            .collect::<Vec<_>>();
        for (_, name, kind) in &previous_records {
            if *kind != NodeKind::RegularFile {
                return Err(JournalError::UnsafeObject);
            }
            faults
                .before(JournalOperation::CleanupRecord)
                .map_err(|error| map_io("clean completed journal", error))?;
            directory
                .remove_file(name)
                .map_err(|error| map_io("clean completed journal", error))?;
            faults
                .after(JournalOperation::CleanupRecord)
                .map_err(|error| map_io("clean completed journal", error))?;
        }
        if !previous_records.is_empty() {
            faults
                .before(JournalOperation::SyncStore)
                .map_err(|error| map_io("synchronize journal cleanup", error))?;
            directory
                .sync_directory()
                .map_err(|error| map_io("synchronize journal cleanup", error))?;
            faults
                .after(JournalOperation::SyncStore)
                .map_err(|error| map_io("synchronize journal cleanup", error))?;
        }
        faults
            .before(JournalOperation::CleanupRecord)
            .map_err(|error| map_io("clean completed journal", error))?;
        directory
            .remove_file(&latest_name)
            .map_err(|error| map_io("clean completed journal", error))?;
        faults
            .after(JournalOperation::CleanupRecord)
            .map_err(|error| map_io("clean completed journal", error))?;
        faults
            .before(JournalOperation::SyncStore)
            .map_err(|error| map_io("synchronize journal cleanup", error))?;
        directory
            .sync_directory()
            .map_err(|error| map_io("synchronize journal cleanup", error))?;
        faults
            .after(JournalOperation::SyncStore)
            .map_err(|error| map_io("synchronize journal cleanup", error))
    }

    fn persist(
        &self,
        directory: &SecureDir,
        record: &JournalRecord,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec(record).map_err(|_| JournalError::Serialization)?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(JournalError::Oversized);
        }
        ensure_store_capacity(directory, bytes.len() as u64)?;
        let final_name = filename(&record.operation_id, record.sequence);
        let (temporary_name, mut file) =
            create_temporary(directory, &record.operation_id, record.sequence, faults)?;
        let identity = SecureDir::file_identity(&file)
            .map_err(|error| map_io("identify temporary journal record", error))?;
        let mut guard = JournalTemp {
            directory: directory.clone(),
            name: temporary_name.clone(),
            identity,
            active: true,
        };
        faults
            .after(JournalOperation::CreateTemporaryRecord)
            .map_err(|error| map_io("create temporary journal record", error))?;
        faults
            .before(JournalOperation::WriteRecord)
            .map_err(|error| map_io("write journal record", error))?;
        file.write_all(&bytes)
            .map_err(|error| map_io("write journal record", error))?;
        faults
            .after(JournalOperation::WriteRecord)
            .map_err(|error| map_io("write journal record", error))?;
        file.flush()
            .map_err(|error| map_io("flush journal record", error))?;
        faults
            .before(JournalOperation::SyncRecord)
            .map_err(|error| map_io("synchronize journal record", error))?;
        file.sync_all()
            .map_err(|error| map_io("synchronize journal record", error))?;
        faults
            .after(JournalOperation::SyncRecord)
            .map_err(|error| map_io("synchronize journal record", error))?;
        faults
            .before(JournalOperation::VerifyRecord)
            .map_err(|error| map_io("verify journal record", error))?;
        let verify = directory
            .open_regular_file(&temporary_name)
            .map_err(|error| map_io("verify journal record", error))?;
        let mut actual = Vec::new();
        verify
            .take(MAX_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| map_io("verify journal record", error))?;
        let parsed: JournalRecord =
            serde_json::from_slice(&actual).map_err(|_| JournalError::Corrupt {
                name: temporary_name.clone(),
            })?;
        parsed.validate()?;
        if parsed != *record {
            return Err(JournalError::Corrupt {
                name: temporary_name,
            });
        }
        faults
            .after(JournalOperation::VerifyRecord)
            .map_err(|error| map_io("verify journal record", error))?;
        faults
            .before(JournalOperation::PublishRecord)
            .map_err(|error| map_io("publish journal record", error))?;
        directory
            .rename_file_child_noreplace(&temporary_name, &final_name, &file)
            .map_err(|error| map_io("publish journal record", error))?;
        guard.active = false;
        faults
            .after(JournalOperation::PublishRecord)
            .map_err(|error| map_io("publish journal record", error))?;
        faults
            .before(JournalOperation::SyncStore)
            .map_err(|error| map_io("synchronize journal storage", error))?;
        directory
            .sync_directory()
            .map_err(|error| map_io("synchronize journal storage", error))?;
        faults
            .after(JournalOperation::SyncStore)
            .map_err(|error| map_io("synchronize journal storage", error))
    }
}

fn ensure_store_capacity(directory: &SecureDir, incoming_bytes: u64) -> Result<(), JournalError> {
    let entries = directory
        .entries()
        .map_err(|error| map_io("inspect journal capacity", error))?;
    if entries.len() >= MAX_JOURNAL_FILES {
        return Err(JournalError::TooManyEntries);
    }
    let mut total = incoming_bytes;
    for (name, kind) in entries {
        if kind != NodeKind::RegularFile {
            return Err(JournalError::UnsafeObject);
        }
        let name = name.to_str().ok_or(JournalError::UnsafeObject)?;
        let file = directory
            .open_regular_file(name)
            .map_err(|error| map_io("inspect journal capacity", error))?;
        total = total
            .checked_add(
                file.metadata()
                    .map_err(|error| map_io("inspect journal capacity", error))?
                    .len(),
            )
            .filter(|total| *total <= MAX_JOURNAL_TOTAL_BYTES)
            .ok_or(JournalError::StoreOversized)?;
    }
    Ok(())
}

pub(crate) struct OperationJournal {
    store: JournalStore,
    directory: SecureDir,
    record: JournalRecord,
}

pub(crate) fn new_staging_name() -> io::Result<String> {
    Ok(format!(".thrn-install-{}", random_id()?))
}

impl OperationJournal {
    pub(crate) fn record(&self) -> &JournalRecord {
        &self.record
    }

    pub(crate) fn set_created_parent_identities(
        &mut self,
        agents: Option<FileIdentity>,
        skills: Option<FileIdentity>,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        self.record.project_agents_identity = agents.map(Into::into);
        self.record.project_skills_identity = skills.map(Into::into);
        self.persist_update(faults)
    }

    pub(crate) fn set_staging_identity(
        &mut self,
        identity: FileIdentity,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        self.record.staging_identity = Some(identity.into());
        self.persist_update(faults)
    }

    pub(crate) fn set_destination_identity(
        &mut self,
        identity: FileIdentity,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        faults
            .before(JournalOperation::SetDestinationIdentity)
            .map_err(|error| map_io("record committed destination identity", error))?;
        if let Some(existing) = self.record.destination_identity()
            && existing != identity
        {
            return Err(JournalError::InvalidRecord);
        }
        if self.record.receipt.is_legacy() || self.record.receipt.is_user_intent() {
            self.record.receipt = self
                .record
                .receipt
                .clone()
                .with_installed_directory_identity(identity)
                .map_err(|_| JournalError::InvalidRecord)?;
        } else if self.record.receipt.installed_directory_identity() != Some(identity) {
            return Err(JournalError::InvalidRecord);
        }
        self.record.destination_identity = Some(identity.into());
        self.persist_update(faults)?;
        faults
            .after(JournalOperation::SetDestinationIdentity)
            .map_err(|error| map_io("record committed destination identity", error))
    }

    pub(crate) fn record_file(
        &mut self,
        path: &str,
        identity: FileIdentity,
        verified: bool,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        if let Some(file) = self
            .record
            .created_files
            .iter_mut()
            .find(|item| item.path == path)
        {
            file.identity = identity.into();
            file.verified = verified;
        } else {
            self.record.created_files.push(JournalFile {
                path: path.to_owned(),
                identity: identity.into(),
                verified,
            });
        }
        self.record
            .created_files
            .sort_by(|a, b| a.path.cmp(&b.path));
        self.persist_update(faults)
    }

    pub(crate) fn record_directory(
        &mut self,
        path: &str,
        identity: FileIdentity,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        if !self
            .record
            .created_directories
            .iter()
            .any(|item| item.path == path)
        {
            self.record.created_directories.push(JournalDirectory {
                path: path.to_owned(),
                identity: identity.into(),
            });
            self.record
                .created_directories
                .sort_by(|a, b| a.path.cmp(&b.path));
        }
        self.persist_update(faults)
    }

    pub(crate) fn record_cleanup_intent(
        &mut self,
        intent: String,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        if !self
            .record
            .cleanup_intents
            .iter()
            .any(|current| current == &intent)
        {
            self.record.cleanup_intents.push(intent);
            self.record.cleanup_intents.sort();
        }
        self.persist_update(faults)
    }

    pub(crate) fn transition(
        &mut self,
        state: OperationState,
        faults: &dyn JournalFaultInjector,
    ) -> Result<(), JournalError> {
        if !valid_transition(self.record.state, state) {
            return Err(JournalError::InvalidTransition);
        }
        self.record.state = state;
        self.persist_update(faults)
    }

    fn persist_update(&mut self, faults: &dyn JournalFaultInjector) -> Result<(), JournalError> {
        self.record.sequence = self
            .record
            .sequence
            .checked_add(1)
            .ok_or(JournalError::InvalidRecord)?;
        self.record.validate()?;
        self.store.persist(&self.directory, &self.record, faults)
    }
}

impl JournalFile {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity.into()
    }

    pub(crate) fn verified(&self) -> bool {
        self.verified
    }
}

impl JournalDirectory {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity.into()
    }
}

struct JournalTemp {
    directory: SecureDir,
    name: String,
    identity: FileIdentity,
    active: bool,
}

impl Drop for JournalTemp {
    fn drop(&mut self) {
        if self.active
            && let Ok(file) = self.directory.open_regular_file(&self.name)
            && SecureDir::file_identity(&file).ok() == Some(self.identity)
        {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

fn create_temporary(
    directory: &SecureDir,
    operation_id: &str,
    sequence: u64,
    faults: &dyn JournalFaultInjector,
) -> Result<(String, fs::File), JournalError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut bytes = [0_u8; 8];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| {
                map_io(
                    "name temporary journal record",
                    io::Error::other("random source unavailable"),
                )
            })?;
        let random = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = format!(".pending-{operation_id}-{sequence:020}-{random}");
        faults
            .before(JournalOperation::CreateTemporaryRecord)
            .map_err(|error| map_io("create temporary journal record", error))?;
        match directory.create_regular_file(&name) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io("create temporary journal record", error)),
        }
    }
    Err(JournalError::TemporaryNameExhausted)
}

fn valid_transition(from: OperationState, to: OperationState) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                OperationState::Prepared,
                OperationState::FilesystemCommitInProgress
            ) | (
                OperationState::FilesystemCommitInProgress,
                OperationState::DirectoryCommitVerified
            ) | (
                OperationState::DirectoryCommitVerified,
                OperationState::ReceiptPublicationPending
            ) | (
                OperationState::ReceiptPublicationPending,
                OperationState::ReceiptPublished
            ) | (OperationState::ReceiptPublished, OperationState::Complete)
                | (OperationState::Prepared, OperationState::RolledBack)
                | (
                    OperationState::FilesystemCommitInProgress,
                    OperationState::RolledBack
                )
        )
}

fn valid_successor(previous: &JournalRecord, current: &JournalRecord) -> bool {
    let stable =
        |old: Option<JournalIdentity>, new: Option<JournalIdentity>| old.is_none() || old == new;
    if !stable(
        previous.project_agents_identity,
        current.project_agents_identity,
    ) || !stable(
        previous.project_skills_identity,
        current.project_skills_identity,
    ) || !stable(previous.staging_identity, current.staging_identity)
        || !stable(previous.destination_identity, current.destination_identity)
    {
        return false;
    }

    for old in &previous.created_files {
        let Some(new) = current
            .created_files
            .iter()
            .find(|entry| entry.path == old.path)
        else {
            return false;
        };
        if old.identity != new.identity || (old.verified && !new.verified) {
            return false;
        }
    }
    if !previous
        .cleanup_intents
        .iter()
        .all(|old| current.cleanup_intents.contains(old))
    {
        return false;
    }
    previous.created_directories.iter().all(|old| {
        current
            .created_directories
            .iter()
            .any(|new| new.path == old.path && new.identity == old.identity)
    })
}

fn filename(operation_id: &str, sequence: u64) -> String {
    format!("op-{operation_id}-{sequence:020}.json")
}

fn parse_filename(name: &str) -> Option<(&str, u64)> {
    let body = name.strip_prefix("op-")?.strip_suffix(".json")?;
    let (id, sequence) = body.split_once('-')?;
    if !is_lower_hex(id, 32)
        || sequence.len() != 20
        || !sequence.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((id, sequence.parse().ok()?))
}

fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("operating system random source unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn map_io(action: &'static str, error: io::Error) -> JournalError {
    JournalError::Io {
        action,
        kind: error.kind(),
        os_error: error.raw_os_error(),
    }
}

#[derive(Debug, Error)]
pub(crate) enum JournalError {
    #[error("could not {action} installation journal ({kind}, os_error={os_error:?})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
    #[error("installation journal storage is not a real directory")]
    UnsafeDirectory,
    #[error("installation journal contains an unknown, linked, or nonregular object")]
    UnsafeObject,
    #[error("installation journal has too many entries")]
    TooManyEntries,
    #[error("installation journal storage exceeds the total size limit")]
    StoreOversized,
    #[error("installation journal exceeds the size limit")]
    Oversized,
    #[error("installation journal contains an unsupported schema version {0}")]
    UnsupportedSchema(u64),
    #[error("installation journal record is invalid")]
    InvalidRecord,
    #[error("installation journal record {name:?} is corrupt or truncated")]
    Corrupt { name: String },
    #[error("installation journal filename does not match its record")]
    FilenameMismatch,
    #[error("installation journal has duplicate state records")]
    DuplicateSequence,
    #[error("installation journal state history is incomplete")]
    SequenceGap,
    #[error("installation journal state transition is invalid")]
    InvalidTransition,
    #[error(
        "an interrupted journal publication remains at {name:?}; manual inspection is required"
    )]
    InterruptedPublication { name: String },
    #[error("could not serialize installation journal")]
    Serialization,
    #[error("temporary installation journal filename attempts were exhausted")]
    TemporaryNameExhausted,
}
