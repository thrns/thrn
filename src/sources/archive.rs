use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read},
};

use thiserror::Error;
use zip::{CompressionMethod, ExtraField, System, ZipArchive, read::ZipFile};

/// Default maximum size of the complete compressed ZIP archive, including ZIP metadata.
pub const MAX_COMPRESSED_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
/// Default maximum number of explicit ZIP entries and manifest paths.
pub const MAX_ARCHIVE_ENTRIES: usize = 4_096;
/// Default maximum expanded size of one regular file.
pub const MAX_FILE_UNCOMPRESSED_BYTES: u64 = 16 * 1024 * 1024;
/// Default maximum expanded size of all regular files combined.
pub const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
/// Default maximum path length in bytes, excluding a directory's final slash marker.
pub const MAX_ARCHIVE_PATH_BYTES: usize = 1_024;
/// Default maximum path component length in bytes.
pub const MAX_ARCHIVE_COMPONENT_BYTES: usize = 255;
/// Default maximum number of components in an archive path.
pub const MAX_ARCHIVE_PATH_DEPTH: usize = 32;

const READ_CHUNK_BYTES: usize = 8 * 1024;
const UNIX_MODE_TYPE_MASK: u32 = 0o170_000;
const UNIX_MODE_REGULAR: u32 = 0o100_000;
const UNIX_MODE_DIRECTORY: u32 = 0o040_000;
const UNIX_MODE_SYMLINK: u32 = 0o120_000;
const ZIP_FLAG_STRONG_ENCRYPTION: u16 = 1 << 6;
const ZIP_FLAG_MASKED_HEADER: u16 = 1 << 13;
const EXTRA_FIELD_STRONG_ENCRYPTION: u16 = 0x0017;
const EXTRA_FIELD_ENCRYPTION_RECIPIENTS: u16 = 0x0018;
const EXTRA_FIELD_ENCRYPTION_CERTIFICATE: u16 = 0x0019;
const EXTRA_FIELD_ENCRYPTION_POLICY: u16 = 0x0021;
const EXTRA_FIELD_SMARTCRYPT: u16 = 0x0022;
const EXTRA_FIELD_AES_ENCRYPTION: u16 = 0x9901;
const EXTRA_FIELD_ASI_UNIX: u16 = 0x756e;
const EOCD_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
const ZIP64_EOCD_SIGNATURE: [u8; 4] = *b"PK\x06\x06";
const ZIP64_LOCATOR_SIGNATURE: [u8; 4] = *b"PK\x06\x07";
const LOCAL_FILE_HEADER_SIGNATURE: [u8; 4] = *b"PK\x03\x04";
const CENTRAL_DIRECTORY_HEADER_SIGNATURE: [u8; 4] = *b"PK\x01\x02";

/// Resource budgets applied while parsing and reading an archive.
///
/// `max_entries` caps both explicit ZIP entries and the manifest after implied parent
/// directories are added. This keeps manifest memory bounded even when a small number of files
/// have deeply nested paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveLimits {
    pub max_compressed_archive_bytes: u64,
    pub max_entries: usize,
    pub max_file_uncompressed_bytes: u64,
    pub max_total_uncompressed_bytes: u64,
    pub max_path_bytes: usize,
    pub max_component_bytes: usize,
    pub max_path_depth: usize,
}

impl ArchiveLimits {
    /// Conservative M2-01 v1 budgets.
    pub const DEFAULT: Self = Self {
        max_compressed_archive_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
        max_entries: MAX_ARCHIVE_ENTRIES,
        max_file_uncompressed_bytes: MAX_FILE_UNCOMPRESSED_BYTES,
        max_total_uncompressed_bytes: MAX_TOTAL_UNCOMPRESSED_BYTES,
        max_path_bytes: MAX_ARCHIVE_PATH_BYTES,
        max_component_bytes: MAX_ARCHIVE_COMPONENT_BYTES,
        max_path_depth: MAX_ARCHIVE_PATH_DEPTH,
    };
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A lexically validated, slash-separated path relative to the ZIP archive root.
///
/// This is not an operating-system path and does not normalize, trim, or resolve components.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ValidatedArchivePath(String);

impl ValidatedArchivePath {
    /// Validate a path that has no ZIP directory marker.
    pub fn parse(value: &str, limits: &ArchiveLimits) -> Result<Self, ArchivePathError> {
        if value.is_empty() {
            return Err(ArchivePathError::Empty);
        }
        if value.starts_with('/') {
            return Err(ArchivePathError::Absolute);
        }
        if value.starts_with("\\\\") {
            return Err(ArchivePathError::UncPath);
        }
        if is_drive_qualified(value.as_bytes()) {
            return Err(ArchivePathError::DriveQualified);
        }
        if value.contains('\\') {
            return Err(ArchivePathError::Backslash);
        }
        if value.len() > limits.max_path_bytes {
            return Err(ArchivePathError::PathTooLong {
                maximum_bytes: limits.max_path_bytes,
            });
        }
        if !value.is_ascii() {
            return Err(ArchivePathError::NonAscii);
        }

        let components = value.split('/').collect::<Vec<_>>();
        if components.iter().any(|component| component.is_empty()) {
            return Err(ArchivePathError::EmptyComponent);
        }
        if components.len() > limits.max_path_depth {
            return Err(ArchivePathError::TooDeep {
                maximum_components: limits.max_path_depth,
            });
        }

        for component in &components {
            validate_component(component, limits)?;
        }

        Ok(Self(value.to_owned()))
    }

    /// Return the validated path with `/` separators.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the path components from the archive root.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Return the number of path components.
    pub fn depth(&self) -> usize {
        self.0.split('/').count()
    }
}

/// A regular file or directory represented in a validated ZIP manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveEntryKind {
    File,
    Directory,
}

/// One manifest path. Implied parent directories have `is_explicit() == false`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveEntry {
    path: ValidatedArchivePath,
    kind: ArchiveEntryKind,
    explicit: bool,
    uncompressed_bytes: u64,
    compressed_bytes: u64,
    crc32: u32,
}

impl ArchiveEntry {
    pub fn path(&self) -> &ValidatedArchivePath {
        &self.path
    }

    pub fn kind(&self) -> ArchiveEntryKind {
        self.kind
    }

    /// Whether this path has a corresponding ZIP central-directory entry.
    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    /// Verified expanded byte count. Directories and implied directories have size zero.
    pub fn uncompressed_bytes(&self) -> u64 {
        self.uncompressed_bytes
    }

    /// Declared compressed byte count from the validated ZIP entry.
    pub fn compressed_bytes(&self) -> u64 {
        self.compressed_bytes
    }

    /// ZIP CRC-32 metadata. Reading the entire entry verifies it before a manifest is returned.
    pub fn crc32(&self) -> u32 {
        self.crc32
    }
}

/// A fully inspected ZIP archive. It contains metadata only; no entry contents are retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedArchive {
    compressed_archive_bytes: u64,
    explicit_entry_count: usize,
    total_uncompressed_bytes: u64,
    entries: Vec<ArchiveEntry>,
}

impl ValidatedArchive {
    pub fn compressed_archive_bytes(&self) -> u64 {
        self.compressed_archive_bytes
    }

    /// Number of entries physically present in the ZIP central directory.
    pub fn explicit_entry_count(&self) -> usize {
        self.explicit_entry_count
    }

    /// Verified expanded byte count across regular files.
    pub fn total_uncompressed_bytes(&self) -> u64 {
        self.total_uncompressed_bytes
    }

    /// Sorted manifest paths, including implied parent directories.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }
}

/// Rejection categories for path syntax in untrusted archive entry names.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ArchivePathError {
    #[error("path is empty")]
    Empty,
    #[error("absolute paths are not allowed")]
    Absolute,
    #[error("UNC paths are not allowed")]
    UncPath,
    #[error("Windows drive-qualified paths are not allowed")]
    DriveQualified,
    #[error("backslashes are not allowed in archive paths")]
    Backslash,
    #[error("path contains an empty component")]
    EmptyComponent,
    #[error("`.` and `..` path components are not allowed")]
    DotComponent,
    #[error("path contains a NUL byte")]
    NulByte,
    #[error("path contains a control character")]
    ControlCharacter,
    #[error("non-ASCII path syntax is unsupported by the portable v1 policy")]
    NonAscii,
    #[error("path contains a character that is invalid on Windows")]
    UnsupportedWindowsCharacter,
    #[error("colon characters, drive syntax, and alternate data streams are not allowed")]
    Colon,
    #[error("path component is a reserved Windows device name")]
    ReservedDeviceName,
    #[error("path component ends in a space or dot")]
    TrailingSpaceOrDot,
    #[error("path exceeds the {maximum_bytes} byte limit")]
    PathTooLong { maximum_bytes: usize },
    #[error("path component exceeds the {maximum_bytes} byte limit")]
    ComponentTooLong { maximum_bytes: usize },
    #[error("path exceeds the {maximum_components} component depth limit")]
    TooDeep { maximum_components: usize },
}

/// Metadata categories that cannot safely be represented as ordinary files or directories.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EntryMetadataProblem {
    #[error("ZIP entry type is not reliably identified")]
    UnknownType,
    #[error("symbolic links are not accepted")]
    SymbolicLink,
    #[error("special files such as devices, sockets, or FIFOs are not accepted")]
    SpecialFile,
    #[error("entry type metadata conflicts with its trailing-slash directory marker")]
    NameTypeMismatch,
    #[error("a directory entry contains data or a non-empty checksum")]
    DirectoryContainsData,
    #[error("entry name encoding metadata changes the raw archive name")]
    NameEncodingMismatch,
    #[error("entry name encoding metadata is malformed")]
    NameEncodingMalformed,
    #[error("link-capable ASi Unix metadata is not accepted")]
    LinkMetadata,
    #[error("ZIP entry extra-field metadata is malformed")]
    ExtraFieldMalformed,
}

/// Actionable archive validation failures. Invalid archives never return a partial manifest.
#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error(
        "compressed ZIP archive is {actual_bytes} bytes; maximum allowed is {maximum_bytes} bytes"
    )]
    ArchiveTooLarge {
        actual_bytes: u64,
        maximum_bytes: u64,
    },
    #[error("invalid ZIP archive structure: {source}")]
    InvalidArchive {
        #[source]
        source: zip::result::ZipError,
    },
    #[error("ZIP archive has {actual_entries} entries; maximum allowed is {maximum_entries}")]
    TooManyEntries {
        actual_entries: u64,
        maximum_entries: usize,
    },
    #[error("ZIP archive contains duplicate central-directory entry names")]
    DuplicateCentralDirectoryName,
    #[error("ZIP archive contains overlapping entry data or headers")]
    OverlappingEntries,
    #[error("archive entry {entry_index} has an unsafe path: {source}")]
    InvalidPath {
        entry_index: usize,
        #[source]
        source: ArchivePathError,
    },
    #[error("archive entry {entry_index} has inconsistent metadata: {problem}")]
    InvalidEntryMetadata {
        entry_index: usize,
        problem: EntryMetadataProblem,
    },
    #[error("archive entry {entry_index} is encrypted or password-protected")]
    EncryptedEntry { entry_index: usize },
    #[error("archive entry {entry_index} uses unsupported compression method {method}")]
    UnsupportedCompression { entry_index: usize, method: u16 },
    #[error("archive entry {entry_index} duplicates path `{path}`")]
    DuplicatePath { entry_index: usize, path: String },
    #[error("archive paths `{first}` and `{second}` collide case-insensitively")]
    CaseFoldCollision { first: String, second: String },
    #[error("archive path `{path}` is both a file and a directory")]
    FileDirectoryConflict { path: String },
    #[error(
        "archive manifest has more than {maximum_entries} paths after implied directories are counted"
    )]
    TooManyManifestPaths { maximum_entries: usize },
    #[error(
        "archive entry {entry_index} declares {declared_bytes} expanded bytes; maximum allowed is {maximum_bytes}"
    )]
    DeclaredFileTooLarge {
        entry_index: usize,
        declared_bytes: u64,
        maximum_bytes: u64,
    },
    #[error("archive entry {entry_index} expands beyond the {maximum_bytes} byte per-file limit")]
    ActualFileTooLarge {
        entry_index: usize,
        maximum_bytes: u64,
    },
    #[error(
        "archive expands beyond the {maximum_bytes} byte total limit while reading entry {entry_index}"
    )]
    TotalExpandedTooLarge {
        entry_index: usize,
        maximum_bytes: u64,
    },
    #[error(
        "archive entry {entry_index} data is corrupt or failed integrity verification ({kind:?})"
    )]
    CorruptEntry {
        entry_index: usize,
        kind: io::ErrorKind,
    },
    #[error(
        "archive entry {entry_index} expanded to {actual_bytes} bytes but declares {declared_bytes}"
    )]
    ExpandedSizeMismatch {
        entry_index: usize,
        declared_bytes: u64,
        actual_bytes: u64,
    },
    #[error("archive size accounting overflowed a 64-bit byte count")]
    SizeOverflow,
}

/// Validate a ZIP archive using the conservative M2-01 v1 limits.
///
/// This inspects names and metadata, then streams every regular file through bounded reads to
/// verify its actual expanded size and CRC. It never extracts, writes, or returns file contents.
pub fn validate_zip_archive(input: &[u8]) -> Result<ValidatedArchive, ArchiveError> {
    validate_zip_archive_with_limits(input, ArchiveLimits::DEFAULT)
}

/// Validate a ZIP archive using caller-supplied resource budgets.
pub fn validate_zip_archive_with_limits(
    input: &[u8],
    limits: ArchiveLimits,
) -> Result<ValidatedArchive, ArchiveError> {
    let archive_bytes = u64::try_from(input.len()).map_err(|_| ArchiveError::SizeOverflow)?;
    if archive_bytes > limits.max_compressed_archive_bytes {
        return Err(ArchiveError::ArchiveTooLarge {
            actual_bytes: archive_bytes,
            maximum_bytes: limits.max_compressed_archive_bytes,
        });
    }

    preflight_entry_count(input, limits.max_entries)?;

    let mut archive = ZipArchive::new(Cursor::new(input)).map_err(invalid_archive)?;
    let explicit_entry_count = archive.len();
    if explicit_entry_count > limits.max_entries {
        return Err(ArchiveError::TooManyEntries {
            actual_entries: u64::try_from(explicit_entry_count)
                .map_err(|_| ArchiveError::SizeOverflow)?,
            maximum_entries: limits.max_entries,
        });
    }
    if archive.has_duplicate_names() {
        return Err(ArchiveError::DuplicateCentralDirectoryName);
    }
    archive
        .validate_header_consistency()
        .map_err(invalid_archive)?;
    if archive.has_overlapping_entries().map_err(invalid_archive)? {
        return Err(ArchiveError::OverlappingEntries);
    }

    let mut manifest = BTreeMap::<String, ArchiveEntry>::new();
    let mut preflight = Vec::with_capacity(explicit_entry_count);
    let mut declared_total_bytes = 0_u64;
    let mut declared_compressed_total_bytes = 0_u64;

    for entry_index in 0..explicit_entry_count {
        let entry = archive
            .by_index_raw(entry_index)
            .map_err(|source| ArchiveError::InvalidArchive { source })?;
        inspect_extra_fields(input, entry.header_start(), true, entry_index)?;
        inspect_extra_fields(input, entry.central_header_start(), false, entry_index)?;
        let (path, has_directory_marker) =
            archive_path(entry.name_raw(), &limits).map_err(|source| {
                ArchiveError::InvalidPath {
                    entry_index,
                    source,
                }
            })?;
        let raw_name =
            std::str::from_utf8(entry.name_raw()).map_err(|_| ArchiveError::InvalidPath {
                entry_index,
                source: ArchivePathError::NonAscii,
            })?;
        let decoded_name = entry
            .name()
            .map_err(|_| ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::NameEncodingMalformed,
            })?;
        if decoded_name.as_ref() != raw_name {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::NameEncodingMismatch,
            });
        }

        let kind = entry_kind(&entry, entry_index, has_directory_marker)?;
        let compression = entry.compression();
        // zip 9.4 deprecates this conversion but still exposes no replacement for reporting an
        // unknown method's original numeric identifier in a structured error.
        #[allow(deprecated)]
        let method = compression.to_u16();
        if compression != CompressionMethod::STORE && compression != CompressionMethod::DEFLATE {
            return Err(ArchiveError::UnsupportedCompression {
                entry_index,
                method,
            });
        }

        if kind == ArchiveEntryKind::Directory
            && (entry.size() != 0 || entry.compressed_size() != 0 || entry.crc32() != 0)
        {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::DirectoryContainsData,
            });
        }
        if kind == ArchiveEntryKind::File && entry.size() > limits.max_file_uncompressed_bytes {
            return Err(ArchiveError::DeclaredFileTooLarge {
                entry_index,
                declared_bytes: entry.size(),
                maximum_bytes: limits.max_file_uncompressed_bytes,
            });
        }

        declared_total_bytes = checked_byte_add(declared_total_bytes, entry.size())?;
        if declared_total_bytes > limits.max_total_uncompressed_bytes {
            return Err(ArchiveError::TotalExpandedTooLarge {
                entry_index,
                maximum_bytes: limits.max_total_uncompressed_bytes,
            });
        }
        declared_compressed_total_bytes =
            checked_byte_add(declared_compressed_total_bytes, entry.compressed_size())?;
        if declared_compressed_total_bytes > archive_bytes {
            return Err(ArchiveError::InvalidArchive {
                source: zip::result::ZipError::InvalidArchive(
                    "declared compressed entry bytes exceed archive size".into(),
                ),
            });
        }

        insert_manifest_path(
            &mut manifest,
            &path,
            kind,
            true,
            entry_index,
            entry.size(),
            entry.compressed_size(),
            entry.crc32(),
            limits.max_entries,
        )?;

        preflight.push(EntryPreflight {
            path,
            declared_bytes: entry.size(),
            compressed_bytes: entry.compressed_size(),
            crc32: entry.crc32(),
        });
    }

    let mut actual_total_bytes = 0_u64;
    for (entry_index, planned_entry) in preflight.iter().enumerate() {
        let mut entry = archive
            .by_index(entry_index)
            .map_err(|source| ArchiveError::InvalidArchive { source })?;
        let mut actual_file_bytes = 0_u64;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];

        loop {
            let file_remaining = limits
                .max_file_uncompressed_bytes
                .saturating_sub(actual_file_bytes);
            let total_remaining = limits
                .max_total_uncompressed_bytes
                .saturating_sub(actual_total_bytes);
            let remaining = file_remaining.min(total_remaining);
            let chunk_len = if remaining >= READ_CHUNK_BYTES as u64 {
                READ_CHUNK_BYTES
            } else {
                usize::try_from(remaining)
                    .map_err(|_| ArchiveError::SizeOverflow)?
                    .saturating_add(1)
            };
            let read = entry.read(&mut buffer[..chunk_len]).map_err(|error| {
                ArchiveError::CorruptEntry {
                    entry_index,
                    kind: error.kind(),
                }
            })?;
            if read == 0 {
                break;
            }

            let read_bytes = u64::try_from(read).map_err(|_| ArchiveError::SizeOverflow)?;
            actual_file_bytes = checked_byte_add(actual_file_bytes, read_bytes)?;
            actual_total_bytes = checked_byte_add(actual_total_bytes, read_bytes)?;

            if actual_file_bytes > limits.max_file_uncompressed_bytes {
                return Err(ArchiveError::ActualFileTooLarge {
                    entry_index,
                    maximum_bytes: limits.max_file_uncompressed_bytes,
                });
            }
            if actual_total_bytes > limits.max_total_uncompressed_bytes {
                return Err(ArchiveError::TotalExpandedTooLarge {
                    entry_index,
                    maximum_bytes: limits.max_total_uncompressed_bytes,
                });
            }
        }

        if actual_file_bytes != planned_entry.declared_bytes {
            return Err(ArchiveError::ExpandedSizeMismatch {
                entry_index,
                declared_bytes: planned_entry.declared_bytes,
                actual_bytes: actual_file_bytes,
            });
        }

        if let Some(manifest_entry) =
            manifest.get_mut(&planned_entry.path.as_str().to_ascii_lowercase())
            && manifest_entry.explicit
        {
            manifest_entry.uncompressed_bytes = actual_file_bytes;
            manifest_entry.compressed_bytes = planned_entry.compressed_bytes;
            manifest_entry.crc32 = planned_entry.crc32;
        }
    }

    Ok(ValidatedArchive {
        compressed_archive_bytes: archive_bytes,
        explicit_entry_count,
        total_uncompressed_bytes: actual_total_bytes,
        entries: manifest.into_values().collect(),
    })
}

#[derive(Clone, Debug)]
struct EntryPreflight {
    path: ValidatedArchivePath,
    declared_bytes: u64,
    compressed_bytes: u64,
    crc32: u32,
}

fn invalid_archive(source: zip::result::ZipError) -> ArchiveError {
    ArchiveError::InvalidArchive { source }
}

fn checked_byte_add(total: u64, increment: u64) -> Result<u64, ArchiveError> {
    total
        .checked_add(increment)
        .ok_or(ArchiveError::SizeOverflow)
}

fn inspect_extra_fields(
    archive: &[u8],
    header_start: u64,
    local_header: bool,
    entry_index: usize,
) -> Result<(), ArchiveError> {
    let start = usize::try_from(header_start).map_err(|_| ArchiveError::SizeOverflow)?;
    let (signature, fixed_header_bytes, name_length_offset, extra_length_offset) = if local_header {
        (LOCAL_FILE_HEADER_SIGNATURE, 30_usize, 26_usize, 28_usize)
    } else {
        (
            CENTRAL_DIRECTORY_HEADER_SIGNATURE,
            46_usize,
            28_usize,
            30_usize,
        )
    };
    let header_end = start
        .checked_add(fixed_header_bytes)
        .ok_or(ArchiveError::SizeOverflow)?;
    let header = archive.get(start..header_end).ok_or_else(|| {
        invalid_archive(zip::result::ZipError::InvalidArchive(
            "truncated ZIP entry header".into(),
        ))
    })?;
    if header.get(..4) != Some(&signature) {
        return Err(invalid_archive(zip::result::ZipError::InvalidArchive(
            "ZIP entry header signature does not match its directory record".into(),
        )));
    }

    let name_bytes = usize::from(u16::from_le_bytes([
        header[name_length_offset],
        header[name_length_offset + 1],
    ]));
    let extra_bytes = usize::from(u16::from_le_bytes([
        header[extra_length_offset],
        header[extra_length_offset + 1],
    ]));
    let extra_start = header_end
        .checked_add(name_bytes)
        .ok_or(ArchiveError::SizeOverflow)?;
    let extra_end = extra_start
        .checked_add(extra_bytes)
        .ok_or(ArchiveError::SizeOverflow)?;
    let extra_data = archive.get(extra_start..extra_end).ok_or_else(|| {
        invalid_archive(zip::result::ZipError::InvalidArchive(
            "ZIP entry extra fields extend beyond the archive".into(),
        ))
    })?;

    let mut offset = 0_usize;
    while offset < extra_data.len() {
        let field_header_end = offset.checked_add(4).ok_or(ArchiveError::SizeOverflow)?;
        let field_header =
            extra_data
                .get(offset..field_header_end)
                .ok_or(ArchiveError::InvalidEntryMetadata {
                    entry_index,
                    problem: EntryMetadataProblem::ExtraFieldMalformed,
                })?;
        let header_id = u16::from_le_bytes([field_header[0], field_header[1]]);
        let field_bytes = usize::from(u16::from_le_bytes([field_header[2], field_header[3]]));
        let field_end = field_header_end
            .checked_add(field_bytes)
            .ok_or(ArchiveError::SizeOverflow)?;
        if extra_data.get(field_header_end..field_end).is_none() {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::ExtraFieldMalformed,
            });
        }
        if matches!(
            header_id,
            EXTRA_FIELD_STRONG_ENCRYPTION
                | EXTRA_FIELD_ENCRYPTION_RECIPIENTS
                | EXTRA_FIELD_ENCRYPTION_CERTIFICATE
                | EXTRA_FIELD_ENCRYPTION_POLICY
                | EXTRA_FIELD_SMARTCRYPT
                | EXTRA_FIELD_AES_ENCRYPTION
        ) {
            return Err(ArchiveError::EncryptedEntry { entry_index });
        }
        if header_id == EXTRA_FIELD_ASI_UNIX {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::LinkMetadata,
            });
        }
        offset = field_end;
    }
    Ok(())
}

/// Check the directory entry count before `ZipArchive` allocates central-directory indexes.
/// The zip crate bounds its allocation by the compressed central-directory size, but this
/// preflight applies THRN's stricter entry budget before that allocation occurs.
fn preflight_entry_count(input: &[u8], maximum_entries: usize) -> Result<(), ArchiveError> {
    let search_start = input.len().saturating_sub(22 + usize::from(u16::MAX));
    let mut eocd_position = None;
    for (offset, signature) in input[search_start..].windows(4).enumerate() {
        if signature != EOCD_SIGNATURE {
            continue;
        }
        let position = search_start + offset;
        let fixed_end = position.checked_add(22).ok_or(ArchiveError::SizeOverflow)?;
        let Some(record) = input.get(position..fixed_end) else {
            continue;
        };
        let comment_bytes = usize::from(u16::from_le_bytes([record[20], record[21]]));
        if fixed_end.checked_add(comment_bytes) != Some(input.len()) {
            continue;
        }
        if eocd_position.replace(position).is_some() {
            return Err(invalid_archive(zip::result::ZipError::InvalidArchive(
                "ambiguous end-of-central-directory records".into(),
            )));
        }
    }
    let eocd_position = eocd_position.ok_or_else(|| {
        invalid_archive(zip::result::ZipError::InvalidArchive(
            "missing or trailing-data end-of-central-directory record".into(),
        ))
    })?;
    let eocd_end = eocd_position
        .checked_add(22)
        .ok_or(ArchiveError::SizeOverflow)?;
    let eocd = input.get(eocd_position..eocd_end).ok_or_else(|| {
        invalid_archive(zip::result::ZipError::InvalidArchive(
            "truncated end-of-central-directory record".into(),
        ))
    })?;
    let classic_count = u16::from_le_bytes([eocd[10], eocd[11]]);

    let count = if eocd_position >= 20
        && input.get(eocd_position - 20..eocd_position - 16) == Some(&ZIP64_LOCATOR_SIGNATURE)
    {
        let locator_position = eocd_position - 20;
        let mut zip64_position = None;
        for (position, signature) in input[..locator_position].windows(4).enumerate() {
            if signature != ZIP64_EOCD_SIGNATURE {
                continue;
            }
            let Some(size_end) = position.checked_add(12) else {
                continue;
            };
            let Some(size_bytes) = input.get(position + 4..size_end) else {
                continue;
            };
            let Ok(size_bytes) = <[u8; 8]>::try_from(size_bytes) else {
                continue;
            };
            let Ok(record_size) = usize::try_from(u64::from_le_bytes(size_bytes)) else {
                continue;
            };
            if record_size < 44 || size_end.checked_add(record_size) != Some(locator_position) {
                continue;
            }
            if zip64_position.replace(position).is_some() {
                return Err(invalid_archive(zip::result::ZipError::InvalidArchive(
                    "ambiguous ZIP64 end records".into(),
                )));
            }
        }
        let zip64_position = zip64_position.ok_or_else(|| {
            invalid_archive(zip::result::ZipError::InvalidArchive(
                "ZIP64 locator has no matching ZIP64 end record".into(),
            ))
        })?;
        let count_start = zip64_position
            .checked_add(32)
            .ok_or(ArchiveError::SizeOverflow)?;
        let count_end = count_start
            .checked_add(8)
            .ok_or(ArchiveError::SizeOverflow)?;
        let count_bytes: [u8; 8] = input
            .get(count_start..count_end)
            .ok_or_else(|| {
                invalid_archive(zip::result::ZipError::InvalidArchive(
                    "truncated ZIP64 end record".into(),
                ))
            })?
            .try_into()
            .map_err(|_| {
                invalid_archive(zip::result::ZipError::InvalidArchive(
                    "truncated ZIP64 entry count".into(),
                ))
            })?;
        u64::from_le_bytes(count_bytes)
    } else if classic_count == u16::MAX {
        return Err(invalid_archive(zip::result::ZipError::InvalidArchive(
            "ZIP64 entry count requires a ZIP64 locator".into(),
        )));
    } else {
        u64::from(classic_count)
    };

    let maximum_entries = u64::try_from(maximum_entries).unwrap_or(u64::MAX);
    if count > maximum_entries {
        return Err(ArchiveError::TooManyEntries {
            actual_entries: count,
            maximum_entries: usize::try_from(maximum_entries).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

fn archive_path(
    raw_name: &[u8],
    limits: &ArchiveLimits,
) -> Result<(ValidatedArchivePath, bool), ArchivePathError> {
    if raw_name.is_empty() {
        return Err(ArchivePathError::Empty);
    }
    if raw_name[0] == b'/' {
        return Err(ArchivePathError::Absolute);
    }
    if raw_name.starts_with(b"\\\\") {
        return Err(ArchivePathError::UncPath);
    }
    if is_drive_qualified(raw_name) {
        return Err(ArchivePathError::DriveQualified);
    }
    if raw_name.contains(&b'\\') {
        return Err(ArchivePathError::Backslash);
    }
    for byte in raw_name {
        if *byte == 0 {
            return Err(ArchivePathError::NulByte);
        }
        if *byte < 0x20 || *byte == 0x7f {
            return Err(ArchivePathError::ControlCharacter);
        }
        if *byte >= 0x80 {
            return Err(ArchivePathError::NonAscii);
        }
    }

    let has_directory_marker = raw_name.last() == Some(&b'/');
    let path_bytes = if has_directory_marker {
        &raw_name[..raw_name.len() - 1]
    } else {
        raw_name
    };
    if path_bytes.len() > limits.max_path_bytes {
        return Err(ArchivePathError::PathTooLong {
            maximum_bytes: limits.max_path_bytes,
        });
    }
    let path = std::str::from_utf8(path_bytes).map_err(|_| ArchivePathError::NonAscii)?;
    Ok((
        ValidatedArchivePath::parse(path, limits)?,
        has_directory_marker,
    ))
}

fn is_drive_qualified(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn validate_component(component: &str, limits: &ArchiveLimits) -> Result<(), ArchivePathError> {
    if component == "." || component == ".." {
        return Err(ArchivePathError::DotComponent);
    }
    if component.len() > limits.max_component_bytes {
        return Err(ArchivePathError::ComponentTooLong {
            maximum_bytes: limits.max_component_bytes,
        });
    }
    if component.ends_with(' ') || component.ends_with('.') {
        return Err(ArchivePathError::TrailingSpaceOrDot);
    }
    for character in component.chars() {
        if character == ':' {
            return Err(ArchivePathError::Colon);
        }
        if character.is_control() {
            return Err(ArchivePathError::ControlCharacter);
        }
        if matches!(character, '<' | '>' | '"' | '|' | '?' | '*') {
            return Err(ArchivePathError::UnsupportedWindowsCharacter);
        }
    }
    if is_reserved_device_name(component) {
        return Err(ArchivePathError::ReservedDeviceName);
    }
    Ok(())
}

fn is_reserved_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
    })
}

fn entry_kind<R: Read>(
    entry: &ZipFile<'_, R>,
    entry_index: usize,
    has_directory_marker: bool,
) -> Result<ArchiveEntryKind, ArchiveError> {
    let flags = entry.flags().as_u16();
    let encryption_extra_field = entry.extra_data_fields().any(|field| match field {
        ExtraField::Custom(custom) => matches!(
            custom.header_id,
            EXTRA_FIELD_STRONG_ENCRYPTION
                | EXTRA_FIELD_ENCRYPTION_RECIPIENTS
                | EXTRA_FIELD_ENCRYPTION_CERTIFICATE
                | EXTRA_FIELD_ENCRYPTION_POLICY
                | EXTRA_FIELD_SMARTCRYPT
                | EXTRA_FIELD_AES_ENCRYPTION
        ),
        _ => false,
    });
    if entry.encrypted()
        || flags & (ZIP_FLAG_STRONG_ENCRYPTION | ZIP_FLAG_MASKED_HEADER) != 0
        || encryption_extra_field
    {
        return Err(ArchiveError::EncryptedEntry { entry_index });
    }

    if entry.extra_data_fields().any(|field| {
        matches!(field, ExtraField::Custom(custom) if custom.header_id == EXTRA_FIELD_ASI_UNIX)
    }) {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::LinkMetadata,
        });
    }

    let mode = entry
        .unix_mode()
        .ok_or(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::UnknownType,
        })?;
    let interpreted_type = mode & UNIX_MODE_TYPE_MASK;
    let raw_external_type = (entry.external_attributes() >> 16) & UNIX_MODE_TYPE_MASK;
    if raw_external_type == UNIX_MODE_SYMLINK || entry.is_symlink() {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::SymbolicLink,
        });
    }
    if raw_external_type != 0
        && !matches!(raw_external_type, UNIX_MODE_REGULAR | UNIX_MODE_DIRECTORY)
    {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::SpecialFile,
        });
    }
    if raw_external_type != 0 && interpreted_type != 0 && raw_external_type != interpreted_type {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::NameTypeMismatch,
        });
    }
    let file_type = if raw_external_type != 0 {
        raw_external_type
    } else {
        interpreted_type
    };
    if entry.system() == System::Unknown && raw_external_type == 0 {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::UnknownType,
        });
    }
    let kind = match file_type {
        UNIX_MODE_REGULAR => ArchiveEntryKind::File,
        UNIX_MODE_DIRECTORY => ArchiveEntryKind::Directory,
        0 => {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::UnknownType,
            });
        }
        _ => {
            return Err(ArchiveError::InvalidEntryMetadata {
                entry_index,
                problem: EntryMetadataProblem::SpecialFile,
            });
        }
    };
    if entry.is_dir() != has_directory_marker
        || (kind == ArchiveEntryKind::Directory) != has_directory_marker
    {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::NameTypeMismatch,
        });
    }
    if kind == ArchiveEntryKind::File && !entry.is_file() {
        return Err(ArchiveError::InvalidEntryMetadata {
            entry_index,
            problem: EntryMetadataProblem::UnknownType,
        });
    }
    Ok(kind)
}

#[allow(clippy::too_many_arguments)]
fn insert_manifest_path(
    manifest: &mut BTreeMap<String, ArchiveEntry>,
    path: &ValidatedArchivePath,
    kind: ArchiveEntryKind,
    explicit: bool,
    entry_index: usize,
    uncompressed_bytes: u64,
    compressed_bytes: u64,
    crc32: u32,
    maximum_entries: usize,
) -> Result<(), ArchiveError> {
    let mut prefix = String::new();
    let components = path.components().collect::<Vec<_>>();
    for (position, component) in components.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let is_final = position + 1 == components.len();
        let node_kind = if is_final {
            kind
        } else {
            ArchiveEntryKind::Directory
        };
        let node_explicit = is_final && explicit;
        let node_path = ValidatedArchivePath(prefix.clone());
        let key = prefix.to_ascii_lowercase();

        match manifest.get_mut(&key) {
            Some(existing) if existing.path.as_str() != prefix => {
                return Err(ArchiveError::CaseFoldCollision {
                    first: existing.path.as_str().to_owned(),
                    second: prefix,
                });
            }
            Some(existing) if existing.kind != node_kind => {
                return Err(ArchiveError::FileDirectoryConflict { path: prefix });
            }
            Some(existing) if node_explicit && existing.explicit => {
                return Err(ArchiveError::DuplicatePath {
                    entry_index,
                    path: prefix,
                });
            }
            Some(existing) if node_explicit => {
                existing.explicit = true;
                existing.uncompressed_bytes = uncompressed_bytes;
                existing.compressed_bytes = compressed_bytes;
                existing.crc32 = crc32;
            }
            Some(_) => {}
            None => {
                manifest.insert(
                    key,
                    ArchiveEntry {
                        path: node_path,
                        kind: node_kind,
                        explicit: node_explicit,
                        uncompressed_bytes: if node_explicit { uncompressed_bytes } else { 0 },
                        compressed_bytes: if node_explicit { compressed_bytes } else { 0 },
                        crc32: if node_explicit { crc32 } else { 0 },
                    },
                );
                if manifest.len() > maximum_entries {
                    return Err(ArchiveError::TooManyManifestPaths { maximum_entries });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ArchiveError;

    #[test]
    fn checked_byte_accounting_rejects_integer_overflow() {
        let result = super::checked_byte_add(u64::MAX, 1);
        assert!(matches!(result, Err(ArchiveError::SizeOverflow)));
        assert!(matches!(
            super::checked_byte_add(u64::MAX - 1, 1),
            Ok(value) if value == u64::MAX
        ));
    }
}
