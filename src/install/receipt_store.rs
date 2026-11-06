//! Private, cache-independent receipt storage and read-only installed listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
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
    commit::{CommittedInstallation, NodeKind, SecureDir},
    destination::FileIdentity,
    receipt::{
        InstallationReceipt, MAX_RECEIPT_BYTES, MAX_RECEIPTS, ReceiptFile, ReceiptValidationError,
    },
};

const IO_CHUNK_BYTES: usize = 8 * 1024;
const WRITER_LOCK_NAME: &str = ".writer.lock";
const TEMP_NAME_ATTEMPTS: usize = 32;

pub struct ReceiptStore {
    directory: PathBuf,
}

impl ReceiptStore {
    pub fn for_system() -> Result<Self, ReceiptStoreError> {
        let data_directory = if let Some(path) = std::env::var_os("THRN_DATA_DIR") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(ReceiptStoreError::RelativeDirectoryOverride);
            }
            path
        } else {
            directories::ProjectDirs::from("im", "thrn", "thrn")
                .ok_or(ReceiptStoreError::DirectoryUnavailable)?
                .data_dir()
                .to_path_buf()
        };
        let data_directory = resolve_data_directory(&data_directory)?;
        Ok(Self::new(data_directory.join("receipts")))
    }

    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Read and validate all stored receipts without creating or changing any storage.
    pub fn list(&self) -> Result<Vec<InstallationReceipt>, ReceiptStoreError> {
        let Some(directory) = self.open_existing_directory()? else {
            return Ok(Vec::new());
        };
        let receipts = read_receipts(&directory, false)?;
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

    /// Reserve the receipt store before the internal project commit so concurrent processes
    /// cannot claim the same destination under different catalog IDs.
    pub(crate) fn reserve(
        &self,
        receipt: InstallationReceipt,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<ReceiptReservation, ReceiptStoreError> {
        receipt
            .validate()
            .map_err(|source| validation_error("pending receipt", source))?;
        if !self.directory.is_absolute() {
            return Err(ReceiptStoreError::RelativeDirectoryOverride);
        }
        faults
            .before(ReceiptOperation::CreateStore)
            .map_err(|error| map_io("create receipt storage", error))?;
        let directory = SecureDir::open_or_create_absolute(&self.directory)
            .map_err(|error| map_directory_error("create or inspect", error))?;
        faults
            .before(ReceiptOperation::CreateWriterLock)
            .map_err(|error| map_io("reserve receipt storage", error))?;
        let lock_file = directory
            .create_regular_file(WRITER_LOCK_NAME)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    ReceiptStoreError::BusyOrInterrupted
                } else {
                    map_io("reserve receipt storage", error)
                }
            })?;
        let lock_identity = SecureDir::file_identity(&lock_file)
            .map_err(|error| map_io("identify receipt-store lock", error))?;
        let reservation = ReceiptReservation {
            directory,
            receipt,
            lock_file,
            lock_identity,
            released: false,
        };
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

    /// Rehash the exact current Codex project tree. Receipt paths are fully validated before
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
fn resolve_data_directory(path: &std::path::Path) -> Result<PathBuf, ReceiptStoreError> {
    let mut current = path;
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if missing.is_empty() && metadata.file_type().is_symlink() {
                    return Err(ReceiptStoreError::UnsafeDirectory);
                }
                let mut resolved = current
                    .canonicalize()
                    .map_err(|error| map_io("resolve application-data directory", error))?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .ok_or(ReceiptStoreError::UnsafeDirectory)?;
                missing.push(name.to_os_string());
                current = current.parent().ok_or(ReceiptStoreError::UnsafeDirectory)?;
            }
            Err(error) => return Err(map_io("inspect application-data directory", error)),
        }
    }
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
}

pub(crate) struct NoReceiptFaults;
impl ReceiptFaultInjector for NoReceiptFaults {}

pub(crate) struct ReceiptReservation {
    directory: SecureDir,
    receipt: InstallationReceipt,
    lock_file: fs::File,
    lock_identity: FileIdentity,
    released: bool,
}

impl ReceiptReservation {
    pub(crate) fn release(mut self) -> Result<(), ReceiptStoreError> {
        self.release_inner()
    }

    pub(crate) fn publish(
        mut self,
        committed: &CommittedInstallation,
        faults: &dyn ReceiptFaultInjector,
    ) -> Result<InstallationReceipt, ReceiptStoreError> {
        let mut published = false;
        let result = self.publish_inner(committed, faults, &mut published);
        let released = self.release_inner();
        match (result, released) {
            (Ok(receipt), Ok(())) => Ok(receipt),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(ReceiptStoreError::CleanupAfterPublication {
                cleanup: error.to_string(),
            }),
            (Err(error), Err(cleanup)) => Err(ReceiptStoreError::CleanupAfterFailure {
                operation: error.to_string(),
                cleanup: cleanup.to_string(),
                published,
            }),
        }
    }

    fn publish_inner(
        &mut self,
        committed: &CommittedInstallation,
        faults: &dyn ReceiptFaultInjector,
        published: &mut bool,
    ) -> Result<InstallationReceipt, ReceiptStoreError> {
        let expected = InstallationReceipt::from_committed_plan(
            committed.plan(),
            committed.inventory(),
            committed.project_identity(),
            self.receipt.installed_at_unix_seconds(),
        )
        .map_err(|source| validation_error("committed receipt", source))?;
        if expected != self.receipt {
            return Err(ReceiptStoreError::CommittedIdentityMismatch);
        }
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
        let final_name = self
            .receipt
            .receipt_filename()
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
            .before(ReceiptOperation::VerifyTemporaryReceipt)
            .map_err(|error| map_io("verify temporary receipt", error))?;
        let written = read_receipt_file(&self.directory, &temporary_name)?;
        if written != self.receipt {
            return Err(ReceiptStoreError::TemporaryReceiptMismatch);
        }

        faults
            .before(ReceiptOperation::PublishNoReplace)
            .map_err(|error| map_io("publish receipt without replacement", error))?;
        self.directory
            .rename_file_child_noreplace(&temporary_name, &final_name, &temporary_file)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    ReceiptStoreError::DuplicateIdentity
                } else {
                    map_io("publish receipt without replacement", error)
                }
            })?;
        temporary_guard.active = false;
        *published = true;

        faults
            .before(ReceiptOperation::SyncReceiptDirectory)
            .map_err(|error| ReceiptStoreError::Durability {
                kind: error.kind(),
                os_error: error.raw_os_error(),
            })?;
        self.directory
            .sync_directory()
            .map_err(|error| ReceiptStoreError::Durability {
                kind: error.kind(),
                os_error: error.raw_os_error(),
            })?;
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
        let current = self
            .directory
            .open_regular_file(WRITER_LOCK_NAME)
            .map_err(|error| map_io("verify receipt-store lock", error))?;
        if SecureDir::file_identity(&current)
            .map_err(|error| map_io("identify receipt-store lock", error))?
            != self.lock_identity
            || SecureDir::file_identity(&self.lock_file)
                .map_err(|error| map_io("identify receipt-store lock", error))?
                != self.lock_identity
        {
            return Err(ReceiptStoreError::WriterLockChanged);
        }
        self.directory
            .remove_file(WRITER_LOCK_NAME)
            .map_err(|error| map_io("remove receipt-store lock", error))?;
        self.directory
            .sync_directory()
            .map_err(|error| map_io("synchronize receipt-store lock removal", error))?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ReceiptReservation {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Ok(current) = self.directory.open_regular_file(WRITER_LOCK_NAME)
            && SecureDir::file_identity(&current).ok() == Some(self.lock_identity)
        {
            let _ = self.directory.remove_file(WRITER_LOCK_NAME);
            let _ = self.directory.sync_directory();
        }
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
    #[error("receipt filename is not a schema-v1 deterministic identity key")]
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
    #[error("receipt storage contains conflicting claims for the same Codex project destination")]
    DestinationConflict,
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

fn read_receipts(
    directory: &SecureDir,
    allow_writer_lock: bool,
) -> Result<Vec<(String, InstallationReceipt)>, ReceiptStoreError> {
    let mut entries = directory
        .entries()
        .map_err(|error| map_io("read receipt storage", error))?;
    if entries.len() > MAX_RECEIPTS + usize::from(allow_writer_lock) {
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
        if name.starts_with(".pending-") {
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
            return Err(ReceiptStoreError::DestinationConflict);
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
            return Err(ReceiptStoreError::DestinationConflict);
        }
    }
    Ok(())
}

fn valid_receipt_filename(name: &str) -> bool {
    let Some(hex) = name
        .strip_prefix("v1-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
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
        != receipt.project_identity()
    {
        return Ok(ReceiptContentState::Modified);
    }
    let Some(agents) = open_directory_status(&project, ".agents")? else {
        return Ok(ReceiptContentState::Missing);
    };
    let Some(skills) = open_directory_status(&agents, "skills")? else {
        return Ok(ReceiptContentState::Missing);
    };
    let Some(root) = open_directory_status(&skills, receipt.skill_name())? else {
        return Ok(ReceiptContentState::Missing);
    };
    if expected_destination_identity.is_some_and(|identity| root.identity().ok() != Some(identity))
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
        sync::Mutex,
    };

    use std::io::{Cursor, Write};
    use tempfile::tempdir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        install::{
            command::prepare_install_session,
            commit::{InstallFaultInjector, InstallOperation, InstallationSession},
            receipt::InstallationReceipt,
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
        let mut bytes = Cursor::new(Vec::new());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
        let skill = format!("{root}skills/uncharted-ideation/");
        let mut zip = ZipWriter::new(&mut bytes);
        zip.add_directory(root, options).expect("root");
        zip.add_directory(format!("{root}skills/"), options)
            .expect("skills");
        zip.add_directory(&skill, options).expect("skill");
        for (name, contents) in [
            (
                format!("{skill}SKILL.md"),
                b"---\nname: uncharted-ideation\ndescription: Receipt test skill.\n---\n"
                    .as_slice(),
            ),
            (format!("{skill}guide.md"), b"guide".as_slice()),
        ] {
            zip.start_file(name, options).expect("file");
            zip.write_all(contents).expect("content");
        }
        zip.finish().expect("archive");
        bytes.into_inner()
    }

    fn session(project: &Path, stage_parent: &Path) -> InstallationSession {
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
            &FakeTransport(archive()),
            &TestFactory(stage_parent.to_path_buf()),
        )
        .expect("validated session")
    }

    #[derive(Default)]
    struct NoInstallFaults;
    impl InstallFaultInjector for NoInstallFaults {}

    #[derive(Default)]
    struct NoReceiptFaults;
    impl ReceiptFaultInjector for NoReceiptFaults {}

    struct FailReceiptAt {
        operation: ReceiptOperation,
        occurrence: usize,
        seen: Mutex<HashMap<ReceiptOperation, usize>>,
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
    fn receipt_store_preflight_failures_happen_before_project_commit() {
        for operation in [
            ReceiptOperation::CreateStore,
            ReceiptOperation::CreateWriterLock,
        ] {
            let (_parent, project, staging, data) = setup();
            let store = ReceiptStore::new(data.join("receipts"));
            let fail = FailReceiptAt {
                operation,
                occurrence: 1,
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
            Err(ReceiptStoreError::DestinationConflict)
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
        assert!(matches!(
            store.list(),
            Err(ReceiptStoreError::BusyOrInterrupted)
        ));
    }
}
