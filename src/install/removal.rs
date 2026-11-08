//! Receipt-backed, adapter-scoped skill removal.
//!
//! Planning verifies the complete live tree without mutation. Execution moves only that verified
//! tree to an operation-private sibling quarantine, then removes receipt-listed objects one at a
//! time under a separately versioned, append-only removal journal.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use ring::digest::{Context, SHA256};
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    commit::{NodeKind, SecureDir},
    destination::{DestinationFileSystem, FileIdentity, ObjectKind, RealDestinationFileSystem},
    receipt::{InstallationReceipt, MAX_RECEIPT_BYTES, ReceiptFile},
};

const PROJECT_REMOVAL_JOURNAL_SCHEMA_VERSION: u64 = 1;
const USER_REMOVAL_JOURNAL_SCHEMA_VERSION: u64 = 2;
const CLAUDE_REMOVAL_JOURNAL_SCHEMA_VERSION: u64 = 3;
const MAX_REMOVAL_JOURNAL_BYTES: usize = 10 * 1024 * 1024;
const MAX_REMOVAL_JOURNAL_FILES: usize = 16_384;
const MAX_REMOVAL_JOURNAL_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const IO_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RemovalState {
    Prepared,
    DestinationRenamePending,
    Quarantined,
    QuarantineCleanupInProgress,
    ContentFullyRemoved,
    ReceiptRetirementPending,
    ReceiptRetired,
    Complete,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemovalIdentity {
    pub(crate) volume: u64,
    pub(crate) object: u64,
}

impl From<FileIdentity> for RemovalIdentity {
    fn from(value: FileIdentity) -> Self {
        Self {
            volume: value.volume(),
            object: value.object(),
        }
    }
}

impl From<RemovalIdentity> for FileIdentity {
    fn from(value: RemovalIdentity) -> Self {
        Self::new(value.volume, value.object)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RemovalObjectKind {
    Root,
    Directory,
    File,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemovalObject {
    pub(crate) path: String,
    pub(crate) kind: RemovalObjectKind,
    pub(crate) identity: RemovalIdentity,
    pub(crate) byte_count: u64,
    pub(crate) sha256: Option<String>,
}

impl RemovalObject {
    pub(crate) fn key(&self) -> String {
        match self.kind {
            RemovalObjectKind::Root => "root".to_owned(),
            RemovalObjectKind::Directory => format!("directory:{}", self.path),
            RemovalObjectKind::File => format!("file:{}", self.path),
        }
    }

    pub(crate) fn order_key(&self) -> (u8, &str) {
        let kind = match self.kind {
            RemovalObjectKind::Root => 0,
            RemovalObjectKind::Directory => 1,
            RemovalObjectKind::File => 2,
        };
        (kind, &self.path)
    }
}

/// A read-only proof that one exact receipt and installed tree currently agree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemovalPlan {
    receipt: InstallationReceipt,
    receipt_name: String,
    receipt_directory_identity: FileIdentity,
    receipt_file_identity: FileIdentity,
    agents_identity: FileIdentity,
    skills_identity: FileIdentity,
    destination_identity: FileIdentity,
    objects: Vec<RemovalObject>,
}

impl RemovalPlan {
    pub(crate) fn verify(
        receipt: InstallationReceipt,
        receipt_name: String,
        receipt_directory_identity: FileIdentity,
        receipt_file_identity: FileIdentity,
    ) -> Result<Self, RemovalError> {
        receipt.validate().map_err(|error| {
            RemovalError::Conflict(format!("ownership receipt is invalid: {error}"))
        })?;
        if receipt_name
            != receipt.receipt_filename().map_err(|error| {
                RemovalError::Conflict(format!("ownership receipt identity is invalid: {error}"))
            })?
        {
            return Err(RemovalError::Conflict(
                "ownership receipt filename does not match its recorded identity".to_owned(),
            ));
        }

        let project = SecureDir::open_absolute(&receipt.root_path()).map_err(|error| {
            RemovalError::Conflict(format!(
                "project root is unavailable or unsafe ({})",
                error.kind()
            ))
        })?;
        if project
            .identity()
            .map_err(removal_io("identify project root"))?
            != receipt.scope_root_identity()
        {
            return Err(RemovalError::Conflict(
                "canonical project path or filesystem identity differs from the receipt".to_owned(),
            ));
        }
        let agents = open_exact_directory(&project, receipt.agent_directory())?;
        let skills = open_exact_directory(&agents, "skills")?;
        let destination_name = receipt.skill_name();
        let destination = open_exact_directory(&skills, destination_name)?;
        let destination_identity = destination
            .identity()
            .map_err(removal_io("identify installed skill directory"))?;
        if !receipt.supports_lifecycle() {
            return Err(RemovalError::Conflict(
                "legacy schema-v1 receipt has no installation-time directory identity; automatic removal is refused"
                    .to_owned(),
            ));
        }
        let installed_identity = receipt
            .installed_directory_identity()
            .ok_or_else(|| RemovalError::Conflict("installation identity is missing".to_owned()))?;
        if destination_identity != installed_identity {
            return Err(RemovalError::Conflict(
                "installed directory identity differs from the installation-time receipt evidence"
                    .to_owned(),
            ));
        }

        let objects = verify_inventory(&receipt, destination)?;
        Ok(Self {
            receipt,
            receipt_name,
            receipt_directory_identity,
            receipt_file_identity,
            agents_identity: agents
                .identity()
                .map_err(removal_io("identify agent directory"))?,
            skills_identity: skills
                .identity()
                .map_err(removal_io("identify skills directory"))?,
            destination_identity,
            objects,
        })
    }

    pub(crate) fn receipt(&self) -> &InstallationReceipt {
        &self.receipt
    }

    pub(crate) fn file_count(&self) -> u64 {
        self.receipt.file_count()
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.receipt.byte_count()
    }

    pub(crate) fn receipt_name(&self) -> &str {
        &self.receipt_name
    }

    pub(crate) fn receipt_directory_identity(&self) -> FileIdentity {
        self.receipt_directory_identity
    }

    pub(crate) fn receipt_file_identity(&self) -> FileIdentity {
        self.receipt_file_identity
    }

    pub(crate) fn agents_identity(&self) -> FileIdentity {
        self.agents_identity
    }

    pub(crate) fn skills_identity(&self) -> FileIdentity {
        self.skills_identity
    }

    pub(crate) fn destination_identity(&self) -> FileIdentity {
        self.destination_identity
    }

    pub(crate) fn objects(&self) -> &[RemovalObject] {
        &self.objects
    }

    pub(crate) fn files(&self) -> &[ReceiptFile] {
        self.receipt.files()
    }

    fn quarantine_name(&self, operation_id: &str) -> String {
        format!(".thrn-removal-{operation_id}")
    }
}

/// Human facing removal errors are deliberately non-success-shaped after any transaction starts.
#[derive(Debug, Error)]
pub(crate) enum RemovalError {
    #[error("no valid THRN ownership receipt matches this canonical resource ID and project")]
    MissingReceipt,
    #[error("removal planning conflict: {0}")]
    Conflict(String),
    #[error("removal could not proceed: {0}")]
    Store(String),
    #[error("removal journal is corrupt or unsafe: {0}")]
    Journal(String),
    #[error(
        "removal may be incomplete (operation {operation_id}); preserve the remaining content, receipt, and journal for inspection: {detail}"
    )]
    Incomplete {
        operation_id: String,
        detail: String,
    },
}

fn open_exact_directory(
    parent: &SecureDir,
    expected_name: &str,
) -> Result<SecureDir, RemovalError> {
    let entries = parent
        .entries()
        .map_err(removal_io("inspect destination directory"))?;
    let mut matches = entries.iter().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected_name))
    });
    let Some((name, kind)) = matches.next() else {
        return Err(RemovalError::Conflict(format!(
            "recorded destination component {expected_name:?} is missing"
        )));
    };
    if matches.next().is_some() || name != expected_name || *kind != NodeKind::Directory {
        return Err(RemovalError::Conflict(format!(
            "destination component {expected_name:?} is linked, unsupported, or has a case-insensitive collision"
        )));
    }
    parent
        .open_directory(std::ffi::OsStr::new(expected_name))
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "destination component {expected_name:?} cannot be opened without following links ({})",
                error.kind()
            ))
        })
}

/// Reuse the removal ownership proof for read-only update inspection. Legacy receipts can prove
/// their recorded project identity and inventory, but they lack the installation-time destination
/// identity required by removal and update.
pub(super) fn verify_receipt_for_check(receipt: &InstallationReceipt) -> Result<(), RemovalError> {
    receipt.validate().map_err(|error| {
        RemovalError::Conflict(format!("ownership receipt is invalid: {error}"))
    })?;
    let project = SecureDir::open_absolute(&receipt.root_path()).map_err(|error| {
        RemovalError::Conflict(format!(
            "project root is unavailable or unsafe ({})",
            error.kind()
        ))
    })?;
    if project
        .identity()
        .map_err(removal_io("identify project root"))?
        != receipt.scope_root_identity()
    {
        return Err(RemovalError::Conflict(
            "canonical project path or filesystem identity differs from the receipt".to_owned(),
        ));
    }
    let agents = open_exact_directory(&project, receipt.agent_directory())?;
    let skills = open_exact_directory(&agents, "skills")?;
    let destination = open_exact_directory(&skills, receipt.skill_name())?;
    let destination_identity = destination
        .identity()
        .map_err(removal_io("identify installed skill directory"))?;
    if receipt
        .installed_directory_identity()
        .is_some_and(|identity| identity != destination_identity)
    {
        return Err(RemovalError::Conflict(
            "installed directory identity differs from the installation-time receipt evidence"
                .to_owned(),
        ));
    }
    verify_inventory(receipt, destination)?;
    Ok(())
}

pub(super) fn verify_inventory(
    receipt: &InstallationReceipt,
    root: SecureDir,
) -> Result<Vec<RemovalObject>, RemovalError> {
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
    let root_identity = root
        .identity()
        .map_err(removal_io("identify installed skill directory"))?;
    let mut identities = HashSet::from([(root_identity.volume(), root_identity.object())]);
    let mut actual_files = BTreeMap::new();
    let mut actual_directories = BTreeSet::new();
    let mut objects = vec![RemovalObject {
        path: String::new(),
        kind: RemovalObjectKind::Root,
        identity: root_identity.into(),
        byte_count: 0,
        sha256: None,
    }];
    let mut stack = vec![(root, String::new(), 0_usize)];
    let mut total_bytes = 0_u64;

    while let Some((directory, prefix, depth)) = stack.pop() {
        if depth > 32 {
            return Err(RemovalError::Conflict(
                "installed tree exceeds the receipt path depth limit".to_owned(),
            ));
        }
        let mut entries = directory
            .entries()
            .map_err(removal_io("read installed directory inventory"))?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, kind) in entries {
            let name = name.to_str().ok_or_else(|| {
                RemovalError::Conflict("installed tree contains a nonportable path".to_owned())
            })?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if !valid_inventory_path(&path) {
                return Err(RemovalError::Conflict(
                    "installed tree contains an unsupported path".to_owned(),
                ));
            }
            match kind {
                NodeKind::Directory => {
                    let child = directory
                        .open_directory(std::ffi::OsStr::new(name))
                        .map_err(removal_io("open installed subdirectory"))?;
                    let identity = child
                        .identity()
                        .map_err(removal_io("identify installed subdirectory"))?;
                    if !identities.insert((identity.volume(), identity.object())) {
                        return Err(RemovalError::Conflict(
                            "installed tree contains ambiguous repeated filesystem identities"
                                .to_owned(),
                        ));
                    }
                    actual_directories.insert(path.clone());
                    objects.push(RemovalObject {
                        path: path.clone(),
                        kind: RemovalObjectKind::Directory,
                        identity: identity.into(),
                        byte_count: 0,
                        sha256: None,
                    });
                    stack.push((child, path, depth + 1));
                }
                NodeKind::RegularFile => {
                    let mut file = directory
                        .open_regular_file(name)
                        .map_err(removal_io("open installed file"))?;
                    let identity = SecureDir::file_identity(&file)
                        .map_err(removal_io("identify installed file"))?;
                    if !identities.insert((identity.volume(), identity.object()))
                        || SecureDir::hard_link_count(&file)
                            .map_err(removal_io("inspect installed file links"))?
                            != 1
                    {
                        return Err(RemovalError::Conflict(
                            "installed tree contains a hard-linked or otherwise ambiguous file"
                                .to_owned(),
                        ));
                    }
                    let length = file
                        .metadata()
                        .map_err(removal_io("inspect installed file"))?
                        .len();
                    if length > crate::sources::archive::MAX_FILE_UNCOMPRESSED_BYTES {
                        return Err(RemovalError::Conflict(
                            "installed file exceeds the recorded size limit".to_owned(),
                        ));
                    }
                    let (count, digest) = hash_file(&mut file, length)?;
                    if count != length {
                        return Err(RemovalError::Conflict(
                            "installed file changed while being verified".to_owned(),
                        ));
                    }
                    total_bytes = total_bytes
                        .checked_add(count)
                        .filter(|bytes| *bytes <= receipt.byte_count())
                        .ok_or_else(|| {
                            RemovalError::Conflict(
                                "installed byte count exceeds the receipt".to_owned(),
                            )
                        })?;
                    actual_files.insert(path.clone(), (count, digest.clone()));
                    objects.push(RemovalObject {
                        path,
                        kind: RemovalObjectKind::File,
                        identity: identity.into(),
                        byte_count: count,
                        sha256: Some(digest),
                    });
                }
                NodeKind::Symlink | NodeKind::Other => {
                    return Err(RemovalError::Conflict(
                        "installed tree contains a link or unsupported filesystem object"
                            .to_owned(),
                    ));
                }
            }
            if actual_files.len() + actual_directories.len() > 4_096 {
                return Err(RemovalError::Conflict(
                    "installed tree exceeds the receipt entry limit".to_owned(),
                ));
            }
        }
    }
    if actual_files != expected_files
        || actual_directories != expected_directories
        || total_bytes != receipt.byte_count()
    {
        return Err(RemovalError::Conflict(
            "installed files or directories differ from the complete ownership receipt".to_owned(),
        ));
    }
    objects.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
    Ok(objects)
}

fn hash_file(file: &mut fs::File, expected_bytes: u64) -> Result<(u64, String), RemovalError> {
    let mut digest = Context::new(&SHA256);
    let mut count = 0_u64;
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(removal_io("hash installed file"))?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .filter(|value| *value <= expected_bytes)
            .ok_or_else(|| {
                RemovalError::Conflict("installed file changed while being hashed".to_owned())
            })?;
        digest.update(&buffer[..read]);
    }
    let mut hex = String::from("sha256:");
    for byte in digest.finish().as_ref() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((count, hex))
}

fn valid_inventory_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1_024
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= 255
                && component.is_ascii()
                && !component.bytes().any(|byte| {
                    byte.is_ascii_control()
                        || byte == b'\\'
                        || byte == b':'
                        || matches!(byte, b'<' | b'>' | b'"' | b'|' | b'?' | b'*')
                })
        })
}

fn removal_io(action: &'static str) -> impl FnOnce(io::Error) -> RemovalError {
    move |error| RemovalError::Conflict(format!("could not {action} safely ({})", error.kind()))
}

pub(super) fn resolve_project_root(
    project: Option<&Path>,
    current_directory: &Path,
) -> Result<PathBuf, RemovalError> {
    let selected = match project {
        Some(path) if path.as_os_str().is_empty() => {
            return Err(RemovalError::Conflict("project path is empty".to_owned()));
        }
        Some(path) => {
            if path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
            {
                return Err(RemovalError::Conflict(
                    "project path cannot contain parent-directory components".to_owned(),
                ));
            }
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                current_directory.join(path)
            }
        }
        None => current_directory.to_path_buf(),
    };
    if !selected.is_absolute() {
        return Err(RemovalError::Conflict(
            "project path is not absolute".to_owned(),
        ));
    }
    match RealDestinationFileSystem
        .object_kind(&selected)
        .map_err(removal_io("inspect selected project"))?
    {
        ObjectKind::Directory => {}
        ObjectKind::Symlink => {
            return Err(RemovalError::Conflict(
                "selected project root is a symbolic link, junction, or reparse point".to_owned(),
            ));
        }
        _ => {
            return Err(RemovalError::Conflict(
                "selected project is not an existing directory".to_owned(),
            ));
        }
    }
    let canonical = selected
        .canonicalize()
        .map_err(removal_io("resolve selected project"))?;
    let _secure = SecureDir::open_absolute(&canonical).map_err(|error| {
        RemovalError::Conflict(format!(
            "project path contains an unsafe component ({}; os_error={:?})",
            error.kind(),
            error.raw_os_error()
        ))
    })?;
    Ok(canonical)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RemovalOperation {
    CreateJournalStore,
    CreateJournalTemporary,
    WriteJournal,
    SyncJournalRecord,
    VerifyJournalRecord,
    PublishJournalRecord,
    SyncJournalStore,
    CleanupJournalRecord,
    Revalidate,
    RenameToQuarantine,
    RemoveFile,
    RemoveDirectory,
    SyncContentDirectory,
    RetireReceipt,
    SyncReceiptDirectory,
}

pub(crate) trait RemovalFaultInjector: Send + Sync {
    fn before(&self, _operation: RemovalOperation) -> io::Result<()> {
        Ok(())
    }

    fn after(&self, _operation: RemovalOperation) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct NoRemovalFaults;
impl RemovalFaultInjector for NoRemovalFaults {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum RemovalOperationType {
    Removal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RemovalRecord {
    schema_version: u64,
    operation_type: RemovalOperationType,
    operation_id: String,
    sequence: u64,
    state: RemovalState,
    receipt: InstallationReceipt,
    receipt_directory_identity: RemovalIdentity,
    receipt_file_identity: RemovalIdentity,
    agents_identity: RemovalIdentity,
    skills_identity: RemovalIdentity,
    destination_identity: RemovalIdentity,
    quarantine_name: String,
    objects: Vec<RemovalObject>,
    cleanup_intents: BTreeSet<String>,
}

impl RemovalRecord {
    pub(super) fn receipt(&self) -> &InstallationReceipt {
        &self.receipt
    }

    fn validate(&self) -> Result<(), RemovalJournalError> {
        let expected_schema = if self.receipt.agent() == "claude-code" {
            CLAUDE_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else if self.receipt.scope() == "user" {
            USER_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_REMOVAL_JOURNAL_SCHEMA_VERSION
        };
        if self.schema_version != expected_schema {
            return Err(RemovalJournalError::UnsupportedSchema(self.schema_version));
        }
        if self.operation_type != RemovalOperationType::Removal
            || !is_lower_hex(&self.operation_id, 32)
            || self.quarantine_name != format!(".thrn-removal-{}", self.operation_id)
            || self.objects.is_empty()
            || self.objects.len() > 4_097
            || self
                .objects
                .windows(2)
                .any(|pair| pair[0].order_key() >= pair[1].order_key())
        {
            return Err(RemovalJournalError::InvalidRecord);
        }
        self.receipt
            .validate()
            .map_err(|_| RemovalJournalError::InvalidRecord)?;
        if let Some(receipt_identity) = self.receipt.installed_directory_identity() {
            let journal_identity: FileIdentity = self.destination_identity.into();
            if journal_identity != receipt_identity {
                return Err(RemovalJournalError::InvalidRecord);
            }
        }
        if self
            .receipt
            .to_json()
            .map_err(|_| RemovalJournalError::InvalidRecord)?
            .len()
            > MAX_RECEIPT_BYTES
        {
            return Err(RemovalJournalError::InvalidRecord);
        }

        let mut expected = BTreeMap::<String, (RemovalObjectKind, u64, Option<String>)>::new();
        expected.insert("root".to_owned(), (RemovalObjectKind::Root, 0, None));
        for directory in self.receipt.directories() {
            expected.insert(
                format!("directory:{directory}"),
                (RemovalObjectKind::Directory, 0, None),
            );
        }
        for file in self.receipt.files() {
            expected.insert(
                format!("file:{}", file.path()),
                (
                    RemovalObjectKind::File,
                    file.byte_count(),
                    Some(file.sha256().to_owned()),
                ),
            );
        }
        if expected.len() != self.objects.len() {
            return Err(RemovalJournalError::InvalidRecord);
        }
        let mut keys = BTreeSet::new();
        for object in &self.objects {
            let key = object.key();
            let Some((expected_kind, expected_bytes, expected_hash)) = expected.get(&key) else {
                return Err(RemovalJournalError::InvalidRecord);
            };
            if object.kind != *expected_kind
                || object.byte_count != *expected_bytes
                || object.sha256 != *expected_hash
                || !keys.insert(key)
            {
                return Err(RemovalJournalError::InvalidRecord);
            }
        }
        if self
            .cleanup_intents
            .iter()
            .any(|intent| !expected.contains_key(intent))
        {
            return Err(RemovalJournalError::InvalidRecord);
        }
        if matches!(
            self.state,
            RemovalState::ContentFullyRemoved
                | RemovalState::ReceiptRetirementPending
                | RemovalState::ReceiptRetired
                | RemovalState::Complete
        ) && self.cleanup_intents.len() != expected.len()
        {
            return Err(RemovalJournalError::InvalidRecord);
        }
        if self.state == RemovalState::RolledBack && !self.cleanup_intents.is_empty() {
            return Err(RemovalJournalError::InvalidRecord);
        }
        Ok(())
    }
}

pub(crate) struct RemovalJournalStore {
    directory: PathBuf,
}

impl RemovalJournalStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn prepare(
        &self,
        plan: &RemovalPlan,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<RemovalJournal, RemovalJournalError> {
        faults
            .before(RemovalOperation::CreateJournalStore)
            .map_err(|error| map_journal_io("create removal journal storage", error))?;
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| map_journal_io("create removal journal storage", error))?;
        faults
            .after(RemovalOperation::CreateJournalStore)
            .map_err(|error| map_journal_io("create removal journal storage", error))?;
        let operation_id =
            random_id().map_err(|error| map_journal_io("name removal operation", error))?;
        let mut objects = plan.objects.clone();
        objects.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
        let schema_version = if plan.receipt.agent() == "claude-code" {
            CLAUDE_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else if plan.receipt.scope() == "user" {
            USER_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_REMOVAL_JOURNAL_SCHEMA_VERSION
        };
        let record = RemovalRecord {
            schema_version,
            operation_type: RemovalOperationType::Removal,
            operation_id: operation_id.clone(),
            sequence: 0,
            state: RemovalState::Prepared,
            receipt: plan.receipt.clone(),
            receipt_directory_identity: plan.receipt_directory_identity.into(),
            receipt_file_identity: plan.receipt_file_identity.into(),
            agents_identity: plan.agents_identity.into(),
            skills_identity: plan.skills_identity.into(),
            destination_identity: plan.destination_identity.into(),
            quarantine_name: plan.quarantine_name(&operation_id),
            objects,
            cleanup_intents: BTreeSet::new(),
        };
        record.validate()?;
        ensure_removal_operation_capacity(&directory, &record)?;
        let store = self.clone_store();
        store.persist(&directory, &record, faults)?;
        Ok(RemovalJournal {
            store,
            directory,
            record,
        })
    }

    fn resume(&self, record: RemovalRecord) -> Result<RemovalJournal, RemovalJournalError> {
        record.validate()?;
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| map_journal_io("open removal journal storage", error))?;
        Ok(RemovalJournal {
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

    pub(super) fn read_all(&self) -> Result<Vec<RemovalRecord>, RemovalJournalError> {
        if !self.directory.is_absolute() {
            return Err(RemovalJournalError::UnsafeDirectory);
        }
        let directory = match SecureDir::open_absolute(&self.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(map_journal_io("inspect removal journals", error)),
        };
        let entries = directory
            .entries()
            .map_err(|error| map_journal_io("read removal journals", error))?;
        if entries.len() > MAX_REMOVAL_JOURNAL_FILES {
            return Err(RemovalJournalError::TooManyEntries);
        }
        let mut total_bytes = 0_u64;
        let mut groups = BTreeMap::<String, BTreeMap<u64, RemovalRecord>>::new();
        for (entry, kind) in entries {
            let name = entry.to_str().ok_or(RemovalJournalError::UnsafeObject)?;
            if name.starts_with(".pending-") {
                return Err(RemovalJournalError::InterruptedPublication(name.to_owned()));
            }
            let Some((operation_id, sequence)) = parse_filename(name) else {
                return Err(RemovalJournalError::UnsafeObject);
            };
            if kind != NodeKind::RegularFile {
                return Err(RemovalJournalError::UnsafeObject);
            }
            let mut file = directory
                .open_regular_file(name)
                .map_err(|error| map_journal_io("open removal journal record", error))?;
            let length = file
                .metadata()
                .map_err(|error| map_journal_io("inspect removal journal record", error))?
                .len();
            if length > MAX_REMOVAL_JOURNAL_BYTES as u64 {
                return Err(RemovalJournalError::Oversized);
            }
            total_bytes = total_bytes
                .checked_add(length)
                .filter(|total| *total <= MAX_REMOVAL_JOURNAL_TOTAL_BYTES)
                .ok_or(RemovalJournalError::StoreOversized)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_REMOVAL_JOURNAL_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| map_journal_io("read removal journal record", error))?;
            if bytes.len() > MAX_REMOVAL_JOURNAL_BYTES {
                return Err(RemovalJournalError::Oversized);
            }
            let record: RemovalRecord = serde_json::from_slice(&bytes)
                .map_err(|_| RemovalJournalError::Corrupt(name.to_owned()))?;
            record.validate()?;
            if record.operation_id != operation_id || record.sequence != sequence {
                return Err(RemovalJournalError::FilenameMismatch);
            }
            if groups
                .entry(operation_id.to_owned())
                .or_default()
                .insert(sequence, record)
                .is_some()
            {
                return Err(RemovalJournalError::DuplicateSequence);
            }
        }
        let mut latest = Vec::new();
        for (_, records) in groups {
            let terminal = records.last_key_value().is_some_and(|(_, record)| {
                matches!(
                    record.state,
                    RemovalState::Complete | RemovalState::RolledBack
                )
            });
            let mut previous: Option<(u64, &RemovalRecord)> = None;
            for (sequence, record) in &records {
                if let Some((old_sequence, old_record)) = previous {
                    let contiguous = *sequence == old_sequence + 1;
                    if !contiguous && !terminal {
                        return Err(RemovalJournalError::SequenceGap);
                    }
                    if old_record.receipt != record.receipt
                        || old_record.operation_id != record.operation_id
                        || (contiguous && !valid_successor(old_record, record))
                    {
                        return Err(RemovalJournalError::InvalidTransition);
                    }
                } else if !terminal && (*sequence != 0 || record.state != RemovalState::Prepared) {
                    return Err(RemovalJournalError::SequenceGap);
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

    pub(super) fn pending_count(&self) -> Result<usize, RemovalJournalError> {
        Ok(self.read_all()?.len())
    }

    fn cleanup_completed(
        &self,
        operation_id: &str,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<(), RemovalJournalError> {
        let directory = SecureDir::open_absolute(&self.directory)
            .map_err(|error| map_journal_io("open removal journal storage", error))?;
        let mut records = directory
            .entries()
            .map_err(|error| map_journal_io("read removal journal storage", error))?
            .into_iter()
            .filter_map(|(name, kind)| {
                let name = name.to_str()?.to_owned();
                let (id, sequence) = parse_filename(&name)?;
                (id == operation_id).then_some((sequence, name, kind))
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.0);
        let Some((latest_sequence, latest_name, latest_kind)) = records.last().cloned() else {
            return Ok(());
        };
        if latest_kind != NodeKind::RegularFile {
            return Err(RemovalJournalError::UnsafeObject);
        }
        let mut latest_file = directory
            .open_regular_file(&latest_name)
            .map_err(|error| map_journal_io("open terminal removal record", error))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut latest_file)
            .take(MAX_REMOVAL_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| map_journal_io("read terminal removal record", error))?;
        let latest_record: RemovalRecord = serde_json::from_slice(&bytes)
            .map_err(|_| RemovalJournalError::Corrupt(latest_name.clone()))?;
        latest_record.validate()?;
        if latest_record.sequence != latest_sequence
            || !matches!(
                latest_record.state,
                RemovalState::Complete | RemovalState::RolledBack
            )
        {
            return Err(RemovalJournalError::InvalidTransition);
        }
        for (_, name, kind) in records.iter().filter(|record| record.1 != latest_name) {
            if *kind != NodeKind::RegularFile {
                return Err(RemovalJournalError::UnsafeObject);
            }
            faults
                .before(RemovalOperation::CleanupJournalRecord)
                .map_err(|error| map_journal_io("clean completed removal journal", error))?;
            directory
                .remove_file(name)
                .map_err(|error| map_journal_io("clean completed removal journal", error))?;
            faults
                .after(RemovalOperation::CleanupJournalRecord)
                .map_err(|error| map_journal_io("clean completed removal journal", error))?;
        }
        if records.len() > 1 {
            sync_removal_directory(&directory, faults)?;
        }
        faults
            .before(RemovalOperation::CleanupJournalRecord)
            .map_err(|error| map_journal_io("clean completed removal journal", error))?;
        directory
            .remove_file(&latest_name)
            .map_err(|error| map_journal_io("clean completed removal journal", error))?;
        faults
            .after(RemovalOperation::CleanupJournalRecord)
            .map_err(|error| map_journal_io("clean completed removal journal", error))?;
        sync_removal_directory(&directory, faults)
    }

    fn persist(
        &self,
        directory: &SecureDir,
        record: &RemovalRecord,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<(), RemovalJournalError> {
        let bytes = serde_json::to_vec(record).map_err(|_| RemovalJournalError::Serialization)?;
        if bytes.len() > MAX_REMOVAL_JOURNAL_BYTES {
            return Err(RemovalJournalError::Oversized);
        }
        ensure_removal_capacity(directory, bytes.len() as u64)?;
        let final_name = filename(&record.operation_id, record.sequence);
        let (temporary_name, mut file) =
            create_removal_temporary(directory, &record.operation_id, record.sequence, faults)?;
        let identity = SecureDir::file_identity(&file)
            .map_err(|error| map_journal_io("identify temporary removal record", error))?;
        let mut guard = RemovalJournalTemp {
            directory: directory.clone(),
            name: temporary_name.clone(),
            identity,
            active: true,
        };
        faults
            .after(RemovalOperation::CreateJournalTemporary)
            .map_err(|error| map_journal_io("create temporary removal record", error))?;
        faults
            .before(RemovalOperation::WriteJournal)
            .map_err(|error| map_journal_io("write removal journal", error))?;
        file.write_all(&bytes)
            .map_err(|error| map_journal_io("write removal journal", error))?;
        faults
            .after(RemovalOperation::WriteJournal)
            .map_err(|error| map_journal_io("write removal journal", error))?;
        file.flush()
            .map_err(|error| map_journal_io("flush removal journal", error))?;
        faults
            .before(RemovalOperation::SyncJournalRecord)
            .map_err(|error| map_journal_io("synchronize removal journal", error))?;
        file.sync_all()
            .map_err(|error| map_journal_io("synchronize removal journal", error))?;
        faults
            .after(RemovalOperation::SyncJournalRecord)
            .map_err(|error| map_journal_io("synchronize removal journal", error))?;
        faults
            .before(RemovalOperation::VerifyJournalRecord)
            .map_err(|error| map_journal_io("verify removal journal", error))?;
        let mut verify = directory
            .open_regular_file(&temporary_name)
            .map_err(|error| map_journal_io("verify removal journal", error))?;
        let mut actual = Vec::new();
        Read::by_ref(&mut verify)
            .take(MAX_REMOVAL_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| map_journal_io("verify removal journal", error))?;
        let parsed: RemovalRecord = serde_json::from_slice(&actual)
            .map_err(|_| RemovalJournalError::Corrupt(temporary_name.clone()))?;
        parsed.validate()?;
        if parsed != *record {
            return Err(RemovalJournalError::Corrupt(temporary_name));
        }
        faults
            .after(RemovalOperation::VerifyJournalRecord)
            .map_err(|error| map_journal_io("verify removal journal", error))?;
        faults
            .before(RemovalOperation::PublishJournalRecord)
            .map_err(|error| map_journal_io("publish removal journal", error))?;
        directory
            .rename_file_child_noreplace(&temporary_name, &final_name, &file)
            .map_err(|error| map_journal_io("publish removal journal", error))?;
        guard.active = false;
        faults
            .after(RemovalOperation::PublishJournalRecord)
            .map_err(|error| map_journal_io("publish removal journal", error))?;
        sync_removal_directory(directory, faults)
    }
}

pub(crate) struct RemovalJournal {
    store: RemovalJournalStore,
    directory: SecureDir,
    record: RemovalRecord,
}

impl RemovalJournal {
    fn record(&self) -> &RemovalRecord {
        &self.record
    }

    fn operation_id(&self) -> &str {
        &self.record.operation_id
    }

    fn set_state(
        &mut self,
        state: RemovalState,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<(), RemovalJournalError> {
        if !valid_transition(self.record.state, state) {
            return Err(RemovalJournalError::InvalidTransition);
        }
        self.record.state = state;
        self.persist_update(faults)
    }

    fn record_cleanup_intent(
        &mut self,
        key: String,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<(), RemovalJournalError> {
        self.record.cleanup_intents.insert(key);
        self.persist_update(faults)
    }

    fn persist_update(
        &mut self,
        faults: &dyn RemovalFaultInjector,
    ) -> Result<(), RemovalJournalError> {
        self.record.sequence = self
            .record
            .sequence
            .checked_add(1)
            .ok_or(RemovalJournalError::InvalidRecord)?;
        self.record.validate()?;
        self.store.persist(&self.directory, &self.record, faults)
    }
}

fn valid_transition(from: RemovalState, to: RemovalState) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                RemovalState::Prepared,
                RemovalState::DestinationRenamePending
            ) | (
                RemovalState::DestinationRenamePending,
                RemovalState::Quarantined
            ) | (
                RemovalState::Quarantined,
                RemovalState::QuarantineCleanupInProgress
            ) | (
                RemovalState::QuarantineCleanupInProgress,
                RemovalState::ContentFullyRemoved
            ) | (
                RemovalState::ContentFullyRemoved,
                RemovalState::ReceiptRetirementPending
            ) | (
                RemovalState::ReceiptRetirementPending,
                RemovalState::ReceiptRetired
            ) | (RemovalState::ReceiptRetired, RemovalState::Complete)
                | (RemovalState::Prepared, RemovalState::RolledBack)
                | (
                    RemovalState::DestinationRenamePending,
                    RemovalState::RolledBack
                )
                | (RemovalState::Quarantined, RemovalState::RolledBack)
        )
}

fn valid_successor(previous: &RemovalRecord, current: &RemovalRecord) -> bool {
    previous.sequence.checked_add(1) == Some(current.sequence)
        && valid_transition(previous.state, current.state)
        && previous.receipt == current.receipt
        && previous.operation_type == current.operation_type
        && previous.receipt_directory_identity == current.receipt_directory_identity
        && previous.receipt_file_identity == current.receipt_file_identity
        && previous.agents_identity == current.agents_identity
        && previous.skills_identity == current.skills_identity
        && previous.destination_identity == current.destination_identity
        && previous.quarantine_name == current.quarantine_name
        && previous.objects == current.objects
        && previous
            .cleanup_intents
            .iter()
            .all(|intent| current.cleanup_intents.contains(intent))
}

fn ensure_removal_capacity(
    directory: &SecureDir,
    incoming_bytes: u64,
) -> Result<(), RemovalJournalError> {
    let entries = directory
        .entries()
        .map_err(|error| map_journal_io("inspect removal journal capacity", error))?;
    if entries.len() >= MAX_REMOVAL_JOURNAL_FILES {
        return Err(RemovalJournalError::TooManyEntries);
    }
    let mut total = incoming_bytes;
    for (name, kind) in entries {
        if kind != NodeKind::RegularFile {
            return Err(RemovalJournalError::UnsafeObject);
        }
        let name = name.to_str().ok_or(RemovalJournalError::UnsafeObject)?;
        let file = directory
            .open_regular_file(name)
            .map_err(|error| map_journal_io("inspect removal journal capacity", error))?;
        total = total
            .checked_add(
                file.metadata()
                    .map_err(|error| map_journal_io("inspect removal journal capacity", error))?
                    .len(),
            )
            .filter(|total| *total <= MAX_REMOVAL_JOURNAL_TOTAL_BYTES)
            .ok_or(RemovalJournalError::StoreOversized)?;
    }
    Ok(())
}

fn ensure_removal_operation_capacity(
    directory: &SecureDir,
    initial: &RemovalRecord,
) -> Result<(), RemovalJournalError> {
    let mut maximum = initial.clone();
    maximum.cleanup_intents = maximum.objects.iter().map(RemovalObject::key).collect();
    maximum.state = RemovalState::Complete;
    maximum.sequence = u64::try_from(maximum.objects.len())
        .ok()
        .and_then(|count| count.checked_add(7))
        .ok_or(RemovalJournalError::InvalidRecord)?;
    maximum.validate()?;
    let maximum_bytes = serde_json::to_vec(&maximum)
        .map_err(|_| RemovalJournalError::Serialization)?
        .len();
    if maximum_bytes > MAX_REMOVAL_JOURNAL_BYTES {
        return Err(RemovalJournalError::Oversized);
    }
    let snapshots = u64::try_from(maximum.objects.len())
        .ok()
        .and_then(|count| count.checked_add(8))
        .ok_or(RemovalJournalError::InvalidRecord)?;
    let projected = u64::try_from(maximum_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_mul(snapshots))
        .ok_or(RemovalJournalError::StoreOversized)?;
    let entries = directory
        .entries()
        .map_err(|error| map_journal_io("inspect removal journal capacity", error))?;
    if entries.len().saturating_add(snapshots as usize) > MAX_REMOVAL_JOURNAL_FILES {
        return Err(RemovalJournalError::TooManyEntries);
    }
    let mut existing_bytes = 0_u64;
    for (name, kind) in entries {
        if kind != NodeKind::RegularFile {
            return Err(RemovalJournalError::UnsafeObject);
        }
        let name = name.to_str().ok_or(RemovalJournalError::UnsafeObject)?;
        let file = directory
            .open_regular_file(name)
            .map_err(|error| map_journal_io("inspect removal journal capacity", error))?;
        existing_bytes = existing_bytes
            .checked_add(
                file.metadata()
                    .map_err(|error| map_journal_io("inspect removal journal capacity", error))?
                    .len(),
            )
            .ok_or(RemovalJournalError::StoreOversized)?;
    }
    if existing_bytes
        .checked_add(projected)
        .is_none_or(|total| total > MAX_REMOVAL_JOURNAL_TOTAL_BYTES)
    {
        return Err(RemovalJournalError::StoreOversized);
    }
    Ok(())
}

fn create_removal_temporary(
    directory: &SecureDir,
    operation_id: &str,
    sequence: u64,
    faults: &dyn RemovalFaultInjector,
) -> Result<(String, fs::File), RemovalJournalError> {
    for _ in 0..32 {
        let random =
            random_id().map_err(|error| map_journal_io("name temporary removal journal", error))?;
        let name = format!(".pending-{operation_id}-{sequence:020}-{random}");
        faults
            .before(RemovalOperation::CreateJournalTemporary)
            .map_err(|error| map_journal_io("create temporary removal record", error))?;
        match directory.create_regular_file(&name) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_journal_io("create temporary removal record", error)),
        }
    }
    Err(RemovalJournalError::TemporaryNameExhausted)
}

fn sync_removal_directory(
    directory: &SecureDir,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalJournalError> {
    faults
        .before(RemovalOperation::SyncJournalStore)
        .map_err(|error| map_journal_io("synchronize removal journal storage", error))?;
    directory
        .sync_directory()
        .map_err(|error| map_journal_io("synchronize removal journal storage", error))?;
    faults
        .after(RemovalOperation::SyncJournalStore)
        .map_err(|error| map_journal_io("synchronize removal journal storage", error))
}

fn filename(operation_id: &str, sequence: u64) -> String {
    format!("rm-{operation_id}-{sequence:020}.json")
}

fn parse_filename(name: &str) -> Option<(&str, u64)> {
    let body = name.strip_prefix("rm-")?.strip_suffix(".json")?;
    let (operation_id, sequence) = body.split_once('-')?;
    if !is_lower_hex(operation_id, 32)
        || sequence.len() != 20
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((operation_id, sequence.parse().ok()?))
}

fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("operating-system random source unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn map_journal_io(action: &'static str, error: io::Error) -> RemovalJournalError {
    RemovalJournalError::Io {
        action,
        kind: error.kind(),
        os_error: error.raw_os_error(),
    }
}

struct RemovalJournalTemp {
    directory: SecureDir,
    name: String,
    identity: FileIdentity,
    active: bool,
}

impl Drop for RemovalJournalTemp {
    fn drop(&mut self) {
        if self.active
            && let Ok(file) = self.directory.open_regular_file(&self.name)
            && SecureDir::file_identity(&file).ok() == Some(self.identity)
        {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RemovalJournalError {
    #[error("could not {action} removal journal ({kind}, os_error={os_error:?})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
    #[error("removal journal storage is unsafe")]
    UnsafeDirectory,
    #[error("removal journal contains an unknown, linked, or nonregular object")]
    UnsafeObject,
    #[error("removal journal has too many records")]
    TooManyEntries,
    #[error("removal journal storage exceeds its size limit")]
    StoreOversized,
    #[error("removal journal record exceeds its size limit")]
    Oversized,
    #[error("removal journal schema version {0} is unsupported")]
    UnsupportedSchema(u64),
    #[error("removal journal record is invalid")]
    InvalidRecord,
    #[error("removal journal record {0:?} is corrupt or truncated")]
    Corrupt(String),
    #[error("removal journal filename does not match its record")]
    FilenameMismatch,
    #[error("removal journal contains a duplicate sequence")]
    DuplicateSequence,
    #[error("removal journal history has a sequence gap")]
    SequenceGap,
    #[error("removal journal transition is invalid")]
    InvalidTransition,
    #[error("interrupted removal journal publication remains at {0:?}")]
    InterruptedPublication(String),
    #[error("removal journal serialization failed")]
    Serialization,
    #[error("temporary removal journal names were exhausted")]
    TemporaryNameExhausted,
}

#[derive(Clone, Debug)]
pub(crate) struct RemovalSummary {
    receipt: InstallationReceipt,
}

impl RemovalSummary {
    pub(crate) fn receipt(&self) -> &InstallationReceipt {
        &self.receipt
    }

    pub(crate) fn files_removed(&self) -> u64 {
        self.receipt.file_count()
    }

    pub(crate) fn bytes_removed(&self) -> u64 {
        self.receipt.byte_count()
    }
}

pub(super) fn remove_plan(
    plan: &RemovalPlan,
    journal_store: &RemovalJournalStore,
    receipt_directory: SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<RemovalSummary, RemovalError> {
    if receipt_directory
        .identity()
        .map_err(removal_io("identify receipt directory"))?
        != plan.receipt_directory_identity
    {
        return Err(RemovalError::Conflict(
            "receipt directory identity changed after planning".to_owned(),
        ));
    }
    let mut journal = journal_store
        .prepare(plan, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))?;
    let operation_id = journal.operation_id().to_owned();
    let result = perform_removal(plan, &mut journal, &receipt_directory, writer_lock, faults);
    match result {
        Ok(()) => {
            journal
                .set_state(RemovalState::Complete, faults)
                .map_err(|error| incomplete(&operation_id, error.to_string()))?;
            journal_store
                .cleanup_completed(&operation_id, faults)
                .map_err(|error| incomplete(&operation_id, error.to_string()))?;
            Ok(RemovalSummary {
                receipt: plan.receipt.clone(),
            })
        }
        Err(error) => Err(incomplete(&operation_id, error.to_string())),
    }
}

pub(super) fn recover_all(
    journal_store: &RemovalJournalStore,
    receipt_directory: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<usize, RemovalError> {
    let records = journal_store
        .read_all()
        .map_err(|error| RemovalError::Journal(error.to_string()))?;
    let mut recovered = 0;
    for initial in records {
        writer_lock
            .validate()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let operation_id = initial.operation_id.clone();
        let mut journal = journal_store
            .resume(initial)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        recover_one(
            journal_store,
            &mut journal,
            receipt_directory,
            writer_lock,
            faults,
        )
        .map_err(|error| incomplete(&operation_id, error.to_string()))?;
        recovered += 1;
    }
    Ok(recovered)
}

fn perform_removal(
    plan: &RemovalPlan,
    journal: &mut RemovalJournal,
    receipt_directory: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    revalidate_plan(plan, receipt_directory, writer_lock, faults)?;
    journal
        .set_state(RemovalState::DestinationRenamePending, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))?;

    let record = journal.record().clone();
    let (project, agents, skills) = open_record_parents(&record)?;
    check_parent_identities(&record, &project, &agents, &skills)?;
    ensure_quarantine_name_available(&skills, &record.quarantine_name)?;
    revalidate_plan(plan, receipt_directory, writer_lock, faults)?;
    writer_lock
        .validate()
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    faults
        .before(RemovalOperation::RenameToQuarantine)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "quarantine rename failed before mutation ({})",
                error.kind()
            ))
        })?;
    skills
        .rename_directory_child_noreplace(
            record.receipt.skill_name(),
            &record.quarantine_name,
            record.destination_identity.into(),
        )
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "verified destination could not be moved to quarantine without replacement ({})",
                error.kind()
            ))
        })?;
    faults
        .after(RemovalOperation::RenameToQuarantine)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "quarantine rename completed but reported failure ({})",
                error.kind()
            ))
        })?;
    sync_content_directory(&skills, faults)?;
    let quarantine = skills
        .open_directory(std::ffi::OsStr::new(&record.quarantine_name))
        .map_err(removal_io("open quarantined skill"))?;
    if quarantine
        .identity()
        .map_err(removal_io("identify quarantined skill"))?
        != plan.destination_identity
    {
        return Err(RemovalError::Conflict(
            "quarantined directory identity does not match the verified destination".to_owned(),
        ));
    }
    ensure_original_destination_missing(&skills, &record)?;
    journal
        .set_state(RemovalState::Quarantined, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))?;
    journal
        .set_state(RemovalState::QuarantineCleanupInProgress, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))?;
    cleanup_quarantine(journal, &skills, writer_lock, faults)?;
    journal
        .set_state(RemovalState::ContentFullyRemoved, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))?;
    retire_ownership_receipt(journal, receipt_directory, writer_lock, faults)?;
    Ok(())
}

fn revalidate_plan(
    plan: &RemovalPlan,
    receipt_directory: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    faults
        .before(RemovalOperation::Revalidate)
        .map_err(|error| {
            RemovalError::Conflict(format!("removal revalidation failed ({})", error.kind()))
        })?;
    writer_lock
        .validate()
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    if receipt_directory
        .identity()
        .map_err(removal_io("identify receipt directory"))?
        != plan.receipt_directory_identity
    {
        return Err(RemovalError::Conflict(
            "receipt directory changed after removal planning".to_owned(),
        ));
    }
    verify_receipt_exists(&plan_record(plan), receipt_directory, false)?;
    let current = RemovalPlan::verify(
        plan.receipt.clone(),
        plan.receipt_name.clone(),
        plan.receipt_directory_identity,
        plan.receipt_file_identity,
    )?;
    if current != *plan {
        return Err(RemovalError::Conflict(
            "receipt, destination, or installed content changed after removal planning".to_owned(),
        ));
    }
    faults.after(RemovalOperation::Revalidate).map_err(|error| {
        RemovalError::Conflict(format!(
            "removal revalidation completed but reported failure ({})",
            error.kind()
        ))
    })
}

fn recover_one(
    journal_store: &RemovalJournalStore,
    journal: &mut RemovalJournal,
    receipt_directory: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    let state = journal.record().state;
    if journal.record().receipt.is_legacy() {
        let record = journal.record().clone();
        return match state {
            RemovalState::Complete => {
                verify_quarantine_removed(&record, &open_record_parents(&record)?.2)?;
                verify_receipt_missing(&record, receipt_directory)?;
                journal_store
                    .cleanup_completed(journal.operation_id(), faults)
                    .map_err(|error| RemovalError::Journal(error.to_string()))
            }
            RemovalState::RolledBack => {
                verify_legacy_original_tree(&record)?;
                verify_receipt_exists(&record, receipt_directory, false)?;
                journal_store
                    .cleanup_completed(journal.operation_id(), faults)
                    .map_err(|error| RemovalError::Journal(error.to_string()))
            }
            _ => Err(RemovalError::Conflict(
                "legacy removal journal has no installation-time directory identity; automatic recovery is refused and its content and journal are preserved"
                    .to_owned(),
            )),
        };
    }
    if state == RemovalState::Prepared {
        verify_original_tree(journal.record())?;
        verify_receipt_exists(journal.record(), receipt_directory, false)?;
        journal
            .set_state(RemovalState::RolledBack, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        journal_store
            .cleanup_completed(journal.operation_id(), faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
        return Ok(());
    }

    if state == RemovalState::DestinationRenamePending {
        let record = journal.record().clone();
        let (_, _, skills) = open_record_parents(&record)?;
        check_parent_identities_for_skills(&record, &skills)?;
        let original = directory_child(&skills, record.receipt.skill_name())?;
        let quarantine = directory_child(&skills, &record.quarantine_name)?;
        match (original, quarantine) {
            (Some(_), None) => {
                verify_original_tree(&record)?;
                verify_receipt_exists(&record, receipt_directory, false)?;
                journal
                    .set_state(RemovalState::RolledBack, faults)
                    .map_err(|error| RemovalError::Journal(error.to_string()))?;
                journal_store
                    .cleanup_completed(journal.operation_id(), faults)
                    .map_err(|error| RemovalError::Journal(error.to_string()))?;
                return Ok(());
            }
            (None, Some(directory)) => {
                if directory
                    .identity()
                    .map_err(removal_io("identify quarantine"))?
                    != record.destination_identity.into()
                {
                    return Err(RemovalError::Conflict(
                        "quarantine identity differs from the verified installation".to_owned(),
                    ));
                }
                verify_quarantine_before_cleanup(&record, &skills)?;
                journal
                    .set_state(RemovalState::Quarantined, faults)
                    .map_err(|error| RemovalError::Journal(error.to_string()))?;
            }
            _ => {
                return Err(RemovalError::Conflict(
                    "destination and quarantine state is ambiguous after interruption".to_owned(),
                ));
            }
        }
    }

    let state = journal.record().state;
    if matches!(
        state,
        RemovalState::Quarantined | RemovalState::QuarantineCleanupInProgress
    ) {
        let record = journal.record().clone();
        let (_, _, skills) = open_record_parents(&record)?;
        check_parent_identities_for_skills(&record, &skills)?;
        ensure_original_destination_missing(&skills, &record)?;
        if journal.record().state == RemovalState::Quarantined {
            verify_quarantine_before_cleanup(&record, &skills)?;
            journal
                .set_state(RemovalState::QuarantineCleanupInProgress, faults)
                .map_err(|error| RemovalError::Journal(error.to_string()))?;
        }
        cleanup_quarantine(journal, &skills, writer_lock, faults)?;
        journal
            .set_state(RemovalState::ContentFullyRemoved, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }

    if journal.record().state == RemovalState::ContentFullyRemoved {
        verify_quarantine_removed(journal.record(), &open_record_parents(journal.record())?.2)?;
        journal
            .set_state(RemovalState::ReceiptRetirementPending, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }

    if journal.record().state == RemovalState::ReceiptRetirementPending {
        retire_ownership_receipt(journal, receipt_directory, writer_lock, faults)?;
    }

    if journal.record().state == RemovalState::ReceiptRetired {
        verify_quarantine_removed(journal.record(), &open_record_parents(journal.record())?.2)?;
        verify_receipt_missing(journal.record(), receipt_directory)?;
        journal
            .set_state(RemovalState::Complete, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }

    if journal.record().state == RemovalState::Complete {
        verify_quarantine_removed(journal.record(), &open_record_parents(journal.record())?.2)?;
        verify_receipt_missing(journal.record(), receipt_directory)?;
        journal_store
            .cleanup_completed(journal.operation_id(), faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    } else if journal.record().state == RemovalState::RolledBack {
        verify_original_tree(journal.record())?;
        verify_receipt_exists(journal.record(), receipt_directory, false)?;
        journal_store
            .cleanup_completed(journal.operation_id(), faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    } else {
        return Err(RemovalError::Conflict(
            "removal recovery stopped at an unsupported state".to_owned(),
        ));
    }
    Ok(())
}

fn record_plan(record: &RemovalRecord) -> Result<RemovalPlan, RemovalError> {
    Ok(RemovalPlan {
        receipt: record.receipt.clone(),
        receipt_name: record.receipt.receipt_filename().map_err(|error| {
            RemovalError::Conflict(format!("recorded receipt identity is invalid: {error}"))
        })?,
        receipt_directory_identity: record.receipt_directory_identity.into(),
        receipt_file_identity: record.receipt_file_identity.into(),
        agents_identity: record.agents_identity.into(),
        skills_identity: record.skills_identity.into(),
        destination_identity: record.destination_identity.into(),
        objects: record.objects.clone(),
    })
}

fn plan_record(plan: &RemovalPlan) -> RemovalRecord {
    let operation_id = "00000000000000000000000000000000".to_owned();
    RemovalRecord {
        schema_version: if plan.receipt.agent() == "claude-code" {
            CLAUDE_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else if plan.receipt.scope() == "user" {
            USER_REMOVAL_JOURNAL_SCHEMA_VERSION
        } else {
            PROJECT_REMOVAL_JOURNAL_SCHEMA_VERSION
        },
        operation_type: RemovalOperationType::Removal,
        operation_id: operation_id.clone(),
        sequence: 0,
        state: RemovalState::Prepared,
        receipt: plan.receipt.clone(),
        receipt_directory_identity: plan.receipt_directory_identity.into(),
        receipt_file_identity: plan.receipt_file_identity.into(),
        agents_identity: plan.agents_identity.into(),
        skills_identity: plan.skills_identity.into(),
        destination_identity: plan.destination_identity.into(),
        quarantine_name: plan.quarantine_name(&operation_id),
        objects: plan.objects.clone(),
        cleanup_intents: BTreeSet::new(),
    }
}

fn open_record_parents(
    record: &RemovalRecord,
) -> Result<(SecureDir, SecureDir, SecureDir), RemovalError> {
    let project = SecureDir::open_absolute(&record.receipt.root_path()).map_err(|error| {
        RemovalError::Conflict(format!(
            "project root is unavailable or unsafe ({})",
            error.kind()
        ))
    })?;
    if project
        .identity()
        .map_err(removal_io("identify project root"))?
        != record.receipt.scope_root_identity()
    {
        return Err(RemovalError::Conflict(
            "canonical project path or filesystem identity changed".to_owned(),
        ));
    }
    let agents = open_exact_directory(&project, record.receipt.agent_directory())?;
    let skills = open_exact_directory(&agents, "skills")?;
    check_parent_identities(record, &project, &agents, &skills)?;
    Ok((project, agents, skills))
}

fn check_parent_identities(
    record: &RemovalRecord,
    project: &SecureDir,
    agents: &SecureDir,
    skills: &SecureDir,
) -> Result<(), RemovalError> {
    if project
        .identity()
        .map_err(removal_io("identify project root"))?
        != record.receipt.scope_root_identity()
        || agents
            .identity()
            .map_err(removal_io("identify agent directory"))?
            != record.agents_identity.into()
        || skills
            .identity()
            .map_err(removal_io("identify skills directory"))?
            != record.skills_identity.into()
    {
        return Err(RemovalError::Conflict(
            "project or agent parent directory identity changed".to_owned(),
        ));
    }
    Ok(())
}

fn check_parent_identities_for_skills(
    record: &RemovalRecord,
    skills: &SecureDir,
) -> Result<(), RemovalError> {
    if skills
        .identity()
        .map_err(removal_io("identify skills directory"))?
        != record.skills_identity.into()
    {
        return Err(RemovalError::Conflict(
            "agent skills directory identity changed".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_quarantine_name_available(
    skills: &SecureDir,
    quarantine_name: &str,
) -> Result<(), RemovalError> {
    if skills
        .entries()
        .map_err(removal_io("inspect quarantine destination"))?
        .iter()
        .any(|(name, _)| {
            name.to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(quarantine_name))
        })
    {
        return Err(RemovalError::Conflict(
            "operation-private quarantine name already exists or collides by case".to_owned(),
        ));
    }
    Ok(())
}

fn directory_child(parent: &SecureDir, name: &str) -> Result<Option<SecureDir>, RemovalError> {
    let entries = parent
        .entries()
        .map_err(removal_io("inspect project destination"))?;
    let mut matches = entries.iter().filter(|(entry_name, _)| {
        entry_name
            .to_str()
            .is_some_and(|entry_name| entry_name.eq_ignore_ascii_case(name))
    });
    let Some((entry_name, kind)) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || entry_name != name || *kind != NodeKind::Directory {
        return Err(RemovalError::Conflict(format!(
            "directory entry {name:?} is linked, unsupported, or collides by case"
        )));
    }
    parent
        .open_directory(std::ffi::OsStr::new(name))
        .map(Some)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "cannot open directory entry {name:?} safely ({})",
                error.kind()
            ))
        })
}

fn ensure_original_destination_missing(
    skills: &SecureDir,
    record: &RemovalRecord,
) -> Result<(), RemovalError> {
    if directory_child(skills, record.receipt.skill_name())?.is_some() {
        return Err(RemovalError::Conflict(
            "the original skill destination was recreated while removal was in progress".to_owned(),
        ));
    }
    Ok(())
}

fn verify_original_tree(record: &RemovalRecord) -> Result<(), RemovalError> {
    let plan = record_plan(record)?;
    let current = RemovalPlan::verify(
        plan.receipt.clone(),
        plan.receipt_name.clone(),
        plan.receipt_directory_identity,
        plan.receipt_file_identity,
    )?;
    if current.destination_identity != plan.destination_identity
        || current.agents_identity != plan.agents_identity
        || current.skills_identity != plan.skills_identity
        || current.objects != plan.objects
    {
        return Err(RemovalError::Conflict(
            "original destination no longer matches the recorded receipt and filesystem identities"
                .to_owned(),
        ));
    }
    Ok(())
}

fn verify_legacy_original_tree(record: &RemovalRecord) -> Result<(), RemovalError> {
    let (_, _, skills) = open_record_parents(record)?;
    let destination = open_exact_directory(&skills, record.receipt.skill_name())?;
    if destination
        .identity()
        .map_err(removal_io("identify legacy installed directory"))?
        != record.destination_identity.into()
        || verify_inventory(&record.receipt, destination)? != record.objects
    {
        return Err(RemovalError::Conflict(
            "legacy removal rollback does not match its pre-removal journal evidence".to_owned(),
        ));
    }
    Ok(())
}

fn verify_quarantine_before_cleanup(
    record: &RemovalRecord,
    skills: &SecureDir,
) -> Result<(), RemovalError> {
    let quarantine = directory_child(skills, &record.quarantine_name)?.ok_or_else(|| {
        RemovalError::Conflict(
            "operation-owned quarantine directory is missing without a complete cleanup record"
                .to_owned(),
        )
    })?;
    if quarantine
        .identity()
        .map_err(removal_io("identify quarantine"))?
        != record.destination_identity.into()
    {
        return Err(RemovalError::Conflict(
            "quarantine directory identity differs from the verified installation".to_owned(),
        ));
    }
    let objects = verify_inventory(&record.receipt, quarantine)?;
    if objects != record.objects {
        return Err(RemovalError::Conflict(
            "quarantined content or object identities differ from the verified installation"
                .to_owned(),
        ));
    }
    Ok(())
}

fn verify_partial_quarantine(
    record: &RemovalRecord,
    skills: &SecureDir,
) -> Result<bool, RemovalError> {
    let Some(root) = directory_child(skills, &record.quarantine_name)? else {
        if !record.cleanup_intents.contains("root")
            || record.cleanup_intents.len() != record.objects.len()
        {
            return Err(RemovalError::Conflict(
                "quarantine is missing without durable cleanup evidence for every recorded object"
                    .to_owned(),
            ));
        }
        return Ok(false);
    };
    let root_identity = record.destination_identity;
    if root.identity().map_err(removal_io("identify quarantine"))? != root_identity.into() {
        return Err(RemovalError::Conflict(
            "quarantine directory identity changed".to_owned(),
        ));
    }

    let expected = record
        .objects
        .iter()
        .map(|object| (object.key(), object))
        .collect::<BTreeMap<_, _>>();
    let mut actual_keys = BTreeSet::from(["root".to_owned()]);
    let mut stack = vec![(root, String::new(), 0_usize)];
    while let Some((directory, prefix, depth)) = stack.pop() {
        if depth > 32 {
            return Err(RemovalError::Conflict(
                "quarantine exceeds the receipt path depth limit".to_owned(),
            ));
        }
        for (name, kind) in directory
            .entries()
            .map_err(removal_io("inspect quarantine inventory"))?
        {
            let name = name.to_str().ok_or_else(|| {
                RemovalError::Conflict("quarantine contains a nonportable path".to_owned())
            })?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let key = match kind {
                NodeKind::Directory => format!("directory:{path}"),
                NodeKind::RegularFile => format!("file:{path}"),
                NodeKind::Symlink | NodeKind::Other => {
                    return Err(RemovalError::Conflict(
                        "quarantine contains a link or unsupported filesystem object".to_owned(),
                    ));
                }
            };
            let object = expected.get(&key).ok_or_else(|| {
                RemovalError::Conflict("quarantine contains an unrecorded object".to_owned())
            })?;
            match kind {
                NodeKind::Directory => {
                    let child = directory
                        .open_directory(std::ffi::OsStr::new(name))
                        .map_err(removal_io("open quarantined subdirectory"))?;
                    if child
                        .identity()
                        .map_err(removal_io("identify quarantined subdirectory"))?
                        != object.identity.into()
                    {
                        return Err(RemovalError::Conflict(
                            "a quarantined directory identity changed".to_owned(),
                        ));
                    }
                    stack.push((child, path, depth + 1));
                }
                NodeKind::RegularFile => {
                    let mut file = directory
                        .open_regular_file(name)
                        .map_err(removal_io("open quarantined file"))?;
                    if SecureDir::file_identity(&file)
                        .map_err(removal_io("identify quarantined file"))?
                        != object.identity.into()
                        || SecureDir::hard_link_count(&file)
                            .map_err(removal_io("inspect quarantined file links"))?
                            != 1
                    {
                        return Err(RemovalError::Conflict(
                            "a quarantined file identity or link count changed".to_owned(),
                        ));
                    }
                    let length = file
                        .metadata()
                        .map_err(removal_io("inspect quarantined file"))?
                        .len();
                    let (count, hash) = hash_file(&mut file, length)?;
                    if count != object.byte_count || object.sha256.as_deref() != Some(hash.as_str())
                    {
                        return Err(RemovalError::Conflict(
                            "a quarantined file changed after removal began".to_owned(),
                        ));
                    }
                }
                NodeKind::Symlink | NodeKind::Other => {
                    return Err(RemovalError::Conflict(
                        "quarantine contains a link or unsupported filesystem object".to_owned(),
                    ));
                }
            }
            actual_keys.insert(key);
        }
    }
    for key in expected.keys().filter(|key| key.as_str() != "root") {
        if !actual_keys.contains(key) && !record.cleanup_intents.contains(key) {
            return Err(RemovalError::Conflict(format!(
                "recorded quarantine object {key:?} is missing without durable cleanup intent"
            )));
        }
    }
    Ok(true)
}

fn cleanup_quarantine(
    journal: &mut RemovalJournal,
    skills: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    let record = journal.record().clone();
    check_parent_identities_for_skills(&record, skills)?;
    ensure_original_destination_missing(skills, &record)?;
    if !verify_partial_quarantine(&record, skills)? {
        // A crash may have occurred after unlink but before the corresponding parent sync.
        sync_content_directory(skills, faults)?;
        return Ok(());
    }

    let mut files = record
        .objects
        .iter()
        .filter(|object| object.kind == RemovalObjectKind::File)
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for object in files {
        cleanup_file(journal, skills, &object, writer_lock, faults)?;
    }

    let mut directories = record
        .objects
        .iter()
        .filter(|object| object.kind == RemovalObjectKind::Directory)
        .cloned()
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        path_depth(&right.path)
            .cmp(&path_depth(&left.path))
            .then_with(|| right.path.cmp(&left.path))
    });
    for object in directories {
        cleanup_directory(journal, skills, &object, writer_lock, faults)?;
    }

    let root = directory_child(skills, &record.quarantine_name)?;
    if let Some(root) = root {
        if root
            .identity()
            .map_err(removal_io("identify quarantine before removal"))?
            != record.destination_identity.into()
            || !root
                .entries()
                .map_err(removal_io("verify empty quarantine"))?
                .is_empty()
        {
            return Err(RemovalError::Conflict(
                "quarantine root changed or is not empty after listed content cleanup".to_owned(),
            ));
        }
        if !journal.record().cleanup_intents.contains("root") {
            journal
                .record_cleanup_intent("root".to_owned(), faults)
                .map_err(|error| RemovalError::Journal(error.to_string()))?;
        }
        writer_lock
            .validate()
            .map_err(|error| RemovalError::Store(error.to_string()))?;
        let current = directory_child(skills, &record.quarantine_name)?.ok_or_else(|| {
            RemovalError::Conflict("quarantine disappeared before root cleanup".to_owned())
        })?;
        if current
            .identity()
            .map_err(removal_io("recheck quarantine identity"))?
            != record.destination_identity.into()
            || !current
                .entries()
                .map_err(removal_io("recheck empty quarantine"))?
                .is_empty()
        {
            return Err(RemovalError::Conflict(
                "quarantine changed before root cleanup".to_owned(),
            ));
        }
        faults
            .before(RemovalOperation::RemoveDirectory)
            .map_err(|error| {
                RemovalError::Conflict(format!(
                    "quarantine cleanup failed before mutation ({})",
                    error.kind()
                ))
            })?;
        skills
            .remove_directory(&record.quarantine_name)
            .map_err(removal_io("remove empty quarantined root"))?;
        faults
            .after(RemovalOperation::RemoveDirectory)
            .map_err(|error| {
                RemovalError::Conflict(format!(
                    "quarantine root was removed but cleanup reported failure ({})",
                    error.kind()
                ))
            })?;
        sync_content_directory(skills, faults)?;
    } else if !journal.record().cleanup_intents.contains("root") {
        return Err(RemovalError::Conflict(
            "quarantine root is missing without a durable cleanup intent".to_owned(),
        ));
    }
    if journal.record().cleanup_intents.len() != journal.record().objects.len() {
        return Err(RemovalError::Conflict(
            "not every receipt-listed object has durable cleanup evidence".to_owned(),
        ));
    }
    Ok(())
}

fn cleanup_file(
    journal: &mut RemovalJournal,
    skills: &SecureDir,
    object: &RemovalObject,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    let record = journal.record().clone();
    let key = object.key();
    let Some((parent, name)) = open_quarantine_parent(skills, &record, &object.path)? else {
        if journal.record().cleanup_intents.contains(&key) {
            sync_content_directory(skills, faults)?;
            return Ok(());
        }
        return Err(RemovalError::Conflict(format!(
            "file {:?} is missing without durable cleanup intent",
            object.path
        )));
    };
    if !file_matches(&parent, &name, object)? {
        if !parent
            .entries()
            .map_err(removal_io("recheck receipt-listed file"))?
            .iter()
            .any(|(entry, _)| entry.to_str() == Some(name.as_str()))
            && journal.record().cleanup_intents.contains(&key)
        {
            sync_content_directory(&parent, faults)?;
            return Ok(());
        }
        if journal.record().cleanup_intents.contains(&key) {
            return Err(RemovalError::Conflict(format!(
                "file {:?} changed after THRN recorded cleanup intent",
                object.path
            )));
        }
        return Err(RemovalError::Conflict(format!(
            "file {:?} no longer matches its receipt and recorded identity",
            object.path
        )));
    }
    if !journal.record().cleanup_intents.contains(&key) {
        journal
            .record_cleanup_intent(key.clone(), faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }
    writer_lock
        .validate()
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    let (current_parent, current_name) = open_quarantine_parent(skills, &record, &object.path)?
        .ok_or_else(|| {
            RemovalError::Conflict("file parent disappeared before cleanup".to_owned())
        })?;
    if current_name != name || !file_matches(&current_parent, &current_name, object)? {
        return Err(RemovalError::Conflict(format!(
            "file {:?} changed immediately before cleanup",
            object.path
        )));
    }
    faults
        .before(RemovalOperation::RemoveFile)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "file cleanup failed before mutation ({})",
                error.kind()
            ))
        })?;
    current_parent
        .remove_regular_file_if_identity(&current_name, object.identity.into())
        .map_err(removal_io("remove receipt-listed file"))?;
    faults
        .after(RemovalOperation::RemoveFile)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "file was removed but cleanup reported failure ({})",
                error.kind()
            ))
        })?;
    sync_content_directory(&current_parent, faults)
}

fn cleanup_directory(
    journal: &mut RemovalJournal,
    skills: &SecureDir,
    object: &RemovalObject,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    let record = journal.record().clone();
    let key = object.key();
    let Some((parent, name)) = open_quarantine_parent(skills, &record, &object.path)? else {
        if journal.record().cleanup_intents.contains(&key) {
            sync_content_directory(skills, faults)?;
            return Ok(());
        }
        return Err(RemovalError::Conflict(format!(
            "directory {:?} is missing without durable cleanup intent",
            object.path
        )));
    };
    let Some(directory) = directory_child(&parent, &name)? else {
        if journal.record().cleanup_intents.contains(&key) {
            sync_content_directory(&parent, faults)?;
            return Ok(());
        }
        return Err(RemovalError::Conflict(format!(
            "directory {:?} is missing without durable cleanup intent",
            object.path
        )));
    };
    if directory
        .identity()
        .map_err(removal_io("identify quarantined directory"))?
        != object.identity.into()
        || !directory
            .entries()
            .map_err(removal_io("verify quarantined directory is empty"))?
            .is_empty()
    {
        return Err(RemovalError::Conflict(format!(
            "directory {:?} changed or is not empty before cleanup",
            object.path
        )));
    }
    if !journal.record().cleanup_intents.contains(&key) {
        journal
            .record_cleanup_intent(key, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }
    writer_lock
        .validate()
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    let current = directory_child(&parent, &name)?
        .ok_or_else(|| RemovalError::Conflict("directory disappeared before cleanup".to_owned()))?;
    if current
        .identity()
        .map_err(removal_io("recheck quarantined directory"))?
        != object.identity.into()
        || !current
            .entries()
            .map_err(removal_io("recheck empty quarantined directory"))?
            .is_empty()
    {
        return Err(RemovalError::Conflict(format!(
            "directory {:?} changed immediately before cleanup",
            object.path
        )));
    }
    faults
        .before(RemovalOperation::RemoveDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "directory cleanup failed before mutation ({})",
                error.kind()
            ))
        })?;
    parent
        .remove_directory(&name)
        .map_err(removal_io("remove empty receipt-listed directory"))?;
    faults
        .after(RemovalOperation::RemoveDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "directory was removed but cleanup reported failure ({})",
                error.kind()
            ))
        })?;
    sync_content_directory(&parent, faults)
}

fn open_quarantine_parent(
    skills: &SecureDir,
    record: &RemovalRecord,
    relative_path: &str,
) -> Result<Option<(SecureDir, String)>, RemovalError> {
    if !valid_inventory_path(relative_path) {
        return Err(RemovalError::Conflict(
            "journal contains an invalid relative path".to_owned(),
        ));
    }
    let Some(mut parent) = directory_child(skills, &record.quarantine_name)? else {
        return Ok(None);
    };
    if parent
        .identity()
        .map_err(removal_io("identify quarantine root"))?
        != record.destination_identity.into()
    {
        return Err(RemovalError::Conflict(
            "quarantine root identity changed".to_owned(),
        ));
    }
    let mut prefix = String::new();
    let components = relative_path.split('/').collect::<Vec<_>>();
    for component in &components[..components.len().saturating_sub(1)] {
        prefix = if prefix.is_empty() {
            (*component).to_owned()
        } else {
            format!("{prefix}/{component}")
        };
        let Some(child) = directory_child(&parent, component)? else {
            return Ok(None);
        };
        let expected = record
            .objects
            .iter()
            .find(|object| object.kind == RemovalObjectKind::Directory && object.path == prefix)
            .ok_or_else(|| {
                RemovalError::Conflict("journal omits a receipt-listed parent directory".to_owned())
            })?;
        if child
            .identity()
            .map_err(removal_io("identify quarantined parent"))?
            != expected.identity.into()
        {
            return Err(RemovalError::Conflict(format!(
                "parent directory {prefix:?} identity changed"
            )));
        }
        parent = child;
    }
    Ok(Some((parent, components.last().unwrap_or(&"").to_string())))
}

fn file_matches(
    parent: &SecureDir,
    name: &str,
    expected: &RemovalObject,
) -> Result<bool, RemovalError> {
    let entries = parent
        .entries()
        .map_err(removal_io("inspect receipt-listed file"))?;
    let mut matches = entries.iter().filter(|(entry_name, _)| {
        entry_name
            .to_str()
            .is_some_and(|entry_name| entry_name.eq_ignore_ascii_case(name))
    });
    let Some((entry_name, kind)) = matches.next() else {
        return Ok(false);
    };
    if matches.next().is_some() || entry_name != name || *kind != NodeKind::RegularFile {
        return Err(RemovalError::Conflict(format!(
            "receipt-listed file {name:?} was linked, replaced, or collided by case"
        )));
    }
    let mut file = parent
        .open_regular_file(name)
        .map_err(removal_io("open receipt-listed file"))?;
    if SecureDir::file_identity(&file).map_err(removal_io("identify receipt-listed file"))?
        != expected.identity.into()
        || SecureDir::hard_link_count(&file)
            .map_err(removal_io("inspect receipt-listed hard links"))?
            != 1
    {
        return Ok(false);
    }
    let length = file
        .metadata()
        .map_err(removal_io("inspect receipt-listed file"))?
        .len();
    if length != expected.byte_count
        || length > crate::sources::archive::MAX_FILE_UNCOMPRESSED_BYTES
    {
        return Ok(false);
    }
    let (count, digest) = hash_file(&mut file, length)?;
    Ok(count == expected.byte_count && expected.sha256.as_deref() == Some(digest.as_str()))
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

fn sync_content_directory(
    directory: &SecureDir,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    faults
        .before(RemovalOperation::SyncContentDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "content directory synchronization failed ({})",
                error.kind()
            ))
        })?;
    directory
        .sync_directory()
        .map_err(removal_io("synchronize content directory"))?;
    faults
        .after(RemovalOperation::SyncContentDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "content directory synchronized but reported failure ({})",
                error.kind()
            ))
        })
}

fn verify_quarantine_removed(
    record: &RemovalRecord,
    skills: &SecureDir,
) -> Result<(), RemovalError> {
    check_parent_identities_for_skills(record, skills)?;
    if directory_child(skills, &record.quarantine_name)?.is_some()
        || !record.cleanup_intents.contains("root")
        || record.cleanup_intents.len() != record.objects.len()
    {
        return Err(RemovalError::Conflict(
            "quarantine removal is not established by complete object cleanup evidence".to_owned(),
        ));
    }
    ensure_original_destination_missing(skills, record)
}

fn verify_receipt_exists(
    record: &RemovalRecord,
    receipt_directory: &SecureDir,
    allow_missing: bool,
) -> Result<(), RemovalError> {
    if receipt_directory
        .identity()
        .map_err(removal_io("identify receipt directory"))?
        != record.receipt_directory_identity.into()
    {
        return Err(RemovalError::Conflict(
            "receipt directory identity changed".to_owned(),
        ));
    }
    let name = record.receipt.receipt_filename().map_err(|error| {
        RemovalError::Conflict(format!("recorded receipt identity is invalid: {error}"))
    })?;
    let entries = receipt_directory
        .entries()
        .map_err(removal_io("inspect ownership receipt"))?;
    let exact = entries
        .iter()
        .filter(|(entry, _)| {
            entry
                .to_str()
                .is_some_and(|entry| entry.eq_ignore_ascii_case(&name))
        })
        .collect::<Vec<_>>();
    if exact.is_empty() {
        return if allow_missing {
            Ok(())
        } else {
            Err(RemovalError::Conflict(
                "ownership receipt is missing before removal was authorized".to_owned(),
            ))
        };
    }
    if exact.len() != 1
        || exact[0].0.to_str() != Some(name.as_str())
        || exact[0].1 != NodeKind::RegularFile
    {
        return Err(RemovalError::Conflict(
            "ownership receipt path is ambiguous or is not a regular file".to_owned(),
        ));
    }
    let mut file = receipt_directory
        .open_regular_file(&name)
        .map_err(removal_io("open ownership receipt"))?;
    if SecureDir::file_identity(&file).map_err(removal_io("identify ownership receipt"))?
        != record.receipt_file_identity.into()
        || SecureDir::hard_link_count(&file)
            .map_err(removal_io("inspect ownership receipt links"))?
            != 1
    {
        return Err(RemovalError::Conflict(
            "ownership receipt identity or link count changed".to_owned(),
        ));
    }
    let length = file
        .metadata()
        .map_err(removal_io("inspect ownership receipt"))?
        .len();
    if length > MAX_RECEIPT_BYTES as u64 {
        return Err(RemovalError::Conflict(
            "ownership receipt exceeds the validation limit".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(removal_io("read ownership receipt"))?;
    let parsed = InstallationReceipt::from_json(&bytes).map_err(|error| {
        RemovalError::Conflict(format!("ownership receipt became corrupt: {error}"))
    })?;
    if parsed != record.receipt {
        return Err(RemovalError::Conflict(
            "ownership receipt changed during removal".to_owned(),
        ));
    }
    Ok(())
}

fn retire_ownership_receipt(
    journal: &mut RemovalJournal,
    receipt_directory: &SecureDir,
    writer_lock: &super::receipt_store::WriterLock,
    faults: &dyn RemovalFaultInjector,
) -> Result<(), RemovalError> {
    if journal.record().state == RemovalState::ContentFullyRemoved {
        journal
            .set_state(RemovalState::ReceiptRetirementPending, faults)
            .map_err(|error| RemovalError::Journal(error.to_string()))?;
    }
    if journal.record().state != RemovalState::ReceiptRetirementPending {
        return Err(RemovalError::Conflict(
            "receipt retirement was requested before content removal was recorded".to_owned(),
        ));
    }
    let record = journal.record().clone();
    let (_, _, skills) = open_record_parents(&record)?;
    verify_quarantine_removed(&record, &skills)?;
    verify_receipt_exists(&record, receipt_directory, true)?;
    writer_lock
        .validate()
        .map_err(|error| RemovalError::Store(error.to_string()))?;
    let receipt_name = record.receipt.receipt_filename().map_err(|error| {
        RemovalError::Conflict(format!("recorded receipt identity is invalid: {error}"))
    })?;
    let present = receipt_directory
        .entries()
        .map_err(removal_io("recheck ownership receipt"))?
        .iter()
        .any(|(entry, _)| entry.to_str() == Some(receipt_name.as_str()));
    if present {
        faults
            .before(RemovalOperation::RetireReceipt)
            .map_err(|error| {
                RemovalError::Conflict(format!(
                    "receipt retirement failed before mutation ({})",
                    error.kind()
                ))
            })?;
        receipt_directory
            .remove_regular_file_if_identity(&receipt_name, record.receipt_file_identity.into())
            .map_err(removal_io("retire ownership receipt"))?;
        faults
            .after(RemovalOperation::RetireReceipt)
            .map_err(|error| {
                RemovalError::Conflict(format!(
                    "receipt was retired but retirement reported failure ({})",
                    error.kind()
                ))
            })?;
    }
    faults
        .before(RemovalOperation::SyncReceiptDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "receipt directory synchronization failed ({})",
                error.kind()
            ))
        })?;
    receipt_directory
        .sync_directory()
        .map_err(removal_io("synchronize receipt retirement"))?;
    faults
        .after(RemovalOperation::SyncReceiptDirectory)
        .map_err(|error| {
            RemovalError::Conflict(format!(
                "receipt directory synchronized but reported failure ({})",
                error.kind()
            ))
        })?;
    if directory_child(&skills, record.receipt.skill_name())?.is_some() {
        return Err(RemovalError::Conflict(
            "a new object appeared at the original skill destination after quarantine".to_owned(),
        ));
    }
    verify_receipt_missing(&record, receipt_directory)?;
    journal
        .set_state(RemovalState::ReceiptRetired, faults)
        .map_err(|error| RemovalError::Journal(error.to_string()))
}

fn verify_receipt_missing(
    record: &RemovalRecord,
    receipt_directory: &SecureDir,
) -> Result<(), RemovalError> {
    if receipt_directory
        .identity()
        .map_err(removal_io("identify receipt directory"))?
        != record.receipt_directory_identity.into()
    {
        return Err(RemovalError::Conflict(
            "receipt directory identity changed".to_owned(),
        ));
    }
    let name = record.receipt.receipt_filename().map_err(|error| {
        RemovalError::Conflict(format!("recorded receipt identity is invalid: {error}"))
    })?;
    if receipt_directory
        .entries()
        .map_err(removal_io("verify retired receipt"))?
        .iter()
        .any(|(entry, _)| {
            entry
                .to_str()
                .is_some_and(|entry| entry.eq_ignore_ascii_case(&name))
        })
    {
        return Err(RemovalError::Conflict(
            "ownership receipt remains after retirement was recorded".to_owned(),
        ));
    }
    Ok(())
}

fn incomplete(operation_id: &str, detail: String) -> RemovalError {
    RemovalError::Incomplete {
        operation_id: operation_id.to_owned(),
        detail,
    }
}
