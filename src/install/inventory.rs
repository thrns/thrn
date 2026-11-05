use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use crc32fast::Hasher;
use thiserror::Error;

use crate::models::catalog::{CommitSha, SkillId, SkillName};
use crate::sources::{
    archive::{ArchiveLimits, ValidatedArchivePath},
    staging::{StagedEntryKind, StagedSkill},
};

const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileInventoryEntry {
    relative_path: PathBuf,
    expected_bytes: u64,
    expected_crc32: u32,
}

impl FileInventoryEntry {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn expected_bytes(&self) -> u64 {
        self.expected_bytes
    }

    pub fn expected_crc32(&self) -> u32 {
        self.expected_crc32
    }
}

/// Exact staged file and directory inventory rechecked against the validated archive manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedInventory {
    catalog_id: SkillId,
    skill_name: SkillName,
    revision: CommitSha,
    content_directory: PathBuf,
    files: Vec<FileInventoryEntry>,
    directories: Vec<PathBuf>,
    total_bytes: u64,
}

impl StagedInventory {
    pub fn catalog_id(&self) -> &SkillId {
        &self.catalog_id
    }

    pub fn skill_name(&self) -> &SkillName {
        &self.skill_name
    }

    pub fn revision(&self) -> &CommitSha {
        &self.revision
    }

    pub fn content_directory(&self) -> &Path {
        &self.content_directory
    }

    pub fn files(&self) -> &[FileInventoryEntry] {
        &self.files
    }

    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("the staged skill root is missing, changed, or is not a real directory")]
    UnsafeRoot,
    #[error("could not {action} staged path {path:?} ({kind})")]
    Filesystem {
        action: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
    },
    #[error("staged content contains a symbolic link or unsupported filesystem object at {path:?}")]
    UnsupportedObject { path: PathBuf },
    #[error("staged content contains a path that fails the validated archive path rules: {path:?}")]
    UnsafePath { path: PathBuf },
    #[error("staged content exceeds the validated archive inventory limits")]
    LimitExceeded,
    #[error("staged content has an unexpected or missing path: {path:?}")]
    ManifestPathMismatch { path: PathBuf },
    #[error("staged entry changed file/directory kind: {path:?}")]
    KindMismatch { path: PathBuf },
    #[error("staged file size changed from {expected} to {actual} bytes: {path:?}")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("staged file content failed its archive CRC check: {path:?}")]
    ChecksumMismatch { path: PathBuf },
    #[error("staged content contains a case-insensitive path collision: {first:?} and {second:?}")]
    CaseCollision { first: PathBuf, second: PathBuf },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActualEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
struct ActualEntry {
    kind: ActualEntryKind,
    bytes: u64,
    crc32: u32,
}

/// Walk the private stage without following links and require an exact match with M2-02's
/// validated archive manifest. No staged or project files are changed.
pub fn inspect_staged_skill(staged: &StagedSkill) -> Result<StagedInventory, InventoryError> {
    let root = staged.content_directory();
    let root_metadata = fs::symlink_metadata(root).map_err(|error| InventoryError::Filesystem {
        action: "inspect",
        path: root.to_path_buf(),
        kind: error.kind(),
    })?;
    if !root_metadata.file_type().is_dir() || root_metadata.file_type().is_symlink() {
        return Err(InventoryError::UnsafeRoot);
    }

    let mut expected = BTreeMap::new();
    let mut expected_names = BTreeMap::<String, PathBuf>::new();
    if staged.expected_entries().len() > ArchiveLimits::DEFAULT.max_entries {
        return Err(InventoryError::LimitExceeded);
    }
    for entry in staged.expected_entries() {
        let path = entry.relative_path().to_path_buf();
        validate_relative_path(&path)?;
        let collision_key = path_key(&path)?;
        if let Some(first) = expected_names.insert(collision_key, path.clone()) {
            return Err(InventoryError::CaseCollision {
                first,
                second: path,
            });
        }
        expected.insert(path, entry);
    }

    let mut actual = BTreeMap::new();
    let mut actual_names = BTreeMap::<String, PathBuf>::new();
    let mut directory_paths = BTreeSet::new();
    let mut stack = vec![(PathBuf::new(), 0_usize)];
    let mut total_bytes = 0_u64;

    while let Some((relative_directory, depth)) = stack.pop() {
        let directory = root.join(&relative_directory);
        let directory_metadata =
            fs::symlink_metadata(&directory).map_err(|error| InventoryError::Filesystem {
                action: "inspect",
                path: directory.clone(),
                kind: error.kind(),
            })?;
        if !directory_metadata.file_type().is_dir() || directory_metadata.file_type().is_symlink() {
            return Err(InventoryError::UnsupportedObject { path: directory });
        }
        if depth > ArchiveLimits::DEFAULT.max_path_depth {
            return Err(InventoryError::LimitExceeded);
        }

        let entries = fs::read_dir(&directory).map_err(|error| InventoryError::Filesystem {
            action: "read",
            path: directory.clone(),
            kind: error.kind(),
        })?;
        let mut entries =
            entries
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| InventoryError::Filesystem {
                    action: "read",
                    path: directory.clone(),
                    kind: error.kind(),
                })?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(InventoryError::UnsafePath {
                    path: relative_directory.join(name),
                });
            };
            let relative = relative_directory.join(name);
            validate_relative_path(&relative)?;
            let path = root.join(&relative);
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| InventoryError::Filesystem {
                    action: "inspect",
                    path: path.clone(),
                    kind: error.kind(),
                })?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(InventoryError::UnsupportedObject { path });
            }
            if file_type.is_dir() {
                if depth + 1 > ArchiveLimits::DEFAULT.max_path_depth {
                    return Err(InventoryError::LimitExceeded);
                }
                add_actual_entry(
                    &mut actual,
                    &mut directory_paths,
                    &mut actual_names,
                    relative.clone(),
                    ActualEntry {
                        kind: ActualEntryKind::Directory,
                        bytes: 0,
                        crc32: 0,
                    },
                )?;
                stack.push((relative, depth + 1));
            } else if file_type.is_file() {
                let size = metadata.len();
                if size > ArchiveLimits::DEFAULT.max_file_uncompressed_bytes {
                    return Err(InventoryError::LimitExceeded);
                }
                total_bytes = total_bytes
                    .checked_add(size)
                    .filter(|bytes| *bytes <= ArchiveLimits::DEFAULT.max_total_uncompressed_bytes)
                    .ok_or(InventoryError::LimitExceeded)?;
                let (read_bytes, crc32) = read_file_crc32(&path, size)?;
                if read_bytes != size {
                    return Err(InventoryError::SizeMismatch {
                        path: relative,
                        expected: size,
                        actual: read_bytes,
                    });
                }
                add_actual_entry(
                    &mut actual,
                    &mut directory_paths,
                    &mut actual_names,
                    relative.clone(),
                    ActualEntry {
                        kind: ActualEntryKind::File,
                        bytes: size,
                        crc32,
                    },
                )?;
            } else {
                return Err(InventoryError::UnsupportedObject { path });
            }
            if actual.len() > ArchiveLimits::DEFAULT.max_entries {
                return Err(InventoryError::LimitExceeded);
            }
        }
    }

    if actual.len() != expected.len() {
        let mismatch = expected
            .keys()
            .find(|path| !actual.contains_key(*path))
            .or_else(|| actual.keys().find(|path| !expected.contains_key(*path)))
            .cloned()
            .unwrap_or_default();
        return Err(InventoryError::ManifestPathMismatch { path: mismatch });
    }

    let mut files = Vec::new();
    for (path, actual_entry) in &actual {
        let Some(expected_entry) = expected.get(path) else {
            return Err(InventoryError::ManifestPathMismatch { path: path.clone() });
        };
        let expected_kind = match expected_entry.kind() {
            StagedEntryKind::File => ActualEntryKind::File,
            StagedEntryKind::Directory => ActualEntryKind::Directory,
        };
        if actual_entry.kind != expected_kind {
            return Err(InventoryError::KindMismatch { path: path.clone() });
        }
        if expected_kind == ActualEntryKind::File {
            if actual_entry.bytes != expected_entry.expected_bytes() {
                return Err(InventoryError::SizeMismatch {
                    path: path.clone(),
                    expected: expected_entry.expected_bytes(),
                    actual: actual_entry.bytes,
                });
            }
            if actual_entry.crc32 != expected_entry.expected_crc32() {
                return Err(InventoryError::ChecksumMismatch { path: path.clone() });
            }
            files.push(FileInventoryEntry {
                relative_path: path.clone(),
                expected_bytes: actual_entry.bytes,
                expected_crc32: actual_entry.crc32,
            });
        }
    }

    let mut directories = directory_paths.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    Ok(StagedInventory {
        catalog_id: staged.catalog_id().clone(),
        skill_name: staged.skill_name().clone(),
        revision: staged.revision().clone(),
        content_directory: staged.content_directory().to_path_buf(),
        files,
        directories,
        total_bytes,
    })
}

fn add_actual_entry(
    actual: &mut BTreeMap<PathBuf, ActualEntry>,
    directories: &mut BTreeSet<PathBuf>,
    names: &mut BTreeMap<String, PathBuf>,
    path: PathBuf,
    entry: ActualEntry,
) -> Result<(), InventoryError> {
    let key = path_key(&path)?;
    if let Some(first) = names.insert(key, path.clone()) {
        return Err(InventoryError::CaseCollision {
            first,
            second: path,
        });
    }
    if entry.kind == ActualEntryKind::Directory {
        directories.insert(path.clone());
    }
    if actual.insert(path.clone(), entry).is_some() {
        return Err(InventoryError::ManifestPathMismatch { path });
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), InventoryError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(InventoryError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(InventoryError::UnsafePath {
                path: path.to_path_buf(),
            });
        };
        let Some(value) = value.to_str() else {
            return Err(InventoryError::UnsafePath {
                path: path.to_path_buf(),
            });
        };
        components.push(value);
    }
    let lexical = components.join("/");
    ValidatedArchivePath::parse(&lexical, &ArchiveLimits::DEFAULT).map_err(|_| {
        InventoryError::UnsafePath {
            path: path.to_path_buf(),
        }
    })?;
    Ok(())
}

fn path_key(path: &Path) -> Result<String, InventoryError> {
    validate_relative_path(path)?;
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(InventoryError::UnsafePath {
                path: path.to_path_buf(),
            });
        };
        let Some(value) = value.to_str() else {
            return Err(InventoryError::UnsafePath {
                path: path.to_path_buf(),
            });
        };
        components.push(value.to_ascii_lowercase());
    }
    Ok(components.join("/"))
}

fn read_file_crc32(path: &Path, expected_size: u64) -> Result<(u64, u32), InventoryError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options
        .open(path)
        .map_err(|error| InventoryError::Filesystem {
            action: "open",
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| InventoryError::Filesystem {
            action: "inspect opened",
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(InventoryError::UnsupportedObject {
            path: path.to_path_buf(),
        });
    }

    let mut hasher = Hasher::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| InventoryError::Filesystem {
                action: "read",
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|bytes| *bytes <= expected_size)
            .ok_or(InventoryError::LimitExceeded)?;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Cursor, Write},
        path::{Path, PathBuf},
    };

    use tempfile::tempdir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, fetch_and_stage_skill_with_factory},
            staging::TempDirFactory,
        },
    };

    use super::{InventoryError, inspect_staged_skill};

    struct FakeTransport(Vec<u8>);

    impl GithubTransport for FakeTransport {
        fn get(&self, _: &str) -> Result<GithubResponse, crate::sources::github::TransportError> {
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
                .prefix("inventory-test-")
                .tempdir_in(&self.0)
        }
    }

    fn fixture_zip() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut bytes);
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .large_file(false);
            let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
            let skill = format!("{root}skills/uncharted-ideation/");
            zip.add_directory(root, options).expect("root directory");
            zip.add_directory(format!("{root}skills/"), options)
                .expect("skills directory");
            zip.add_directory(&skill, options).expect("skill directory");
            zip.start_file(format!("{root}README.md"), options)
                .expect("unrelated repository file");
            zip.write_all(b"not part of the selected skill")
                .expect("unrelated contents");
            for (name, contents) in [
                (format!("{skill}SKILL.md"), b"---\nname: uncharted-ideation\ndescription: A fixture skill for planning tests.\n---\n\n# Skill\n".as_slice()),
                (format!("{skill}references/guide.md"), b"reference".as_slice()),
                (format!("{skill}scripts/run.sh"), b"echo inert".as_slice()),
                (format!("{skill}assets/icon.bin"), b"\0\x01\x02\xff".as_slice()),
            ] {
                zip.start_file(name, options).expect("file entry");
                zip.write_all(contents).expect("file contents");
            }
            zip.finish().expect("zip finishes");
        }
        bytes.into_inner()
    }

    fn make_staged(parent: &Path) -> crate::sources::staging::StagedSkill {
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture validates");
        let resource = &catalog.resources()[0];
        let archive = fixture_zip();
        fetch_and_stage_skill_with_factory(
            resource,
            &FakeTransport(archive),
            &TestFactory(parent.to_path_buf()),
        )
        .expect("fixture source stages")
    }

    #[test]
    fn inventory_is_complete_sorted_bounded_and_matches_manifest_sizes() {
        let parent = tempdir().expect("temporary parent");
        let staged = make_staged(parent.path());
        let inventory = inspect_staged_skill(&staged).expect("stage inventory validates");
        let paths = inventory
            .files()
            .iter()
            .map(|file| file.relative_path())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                Path::new("SKILL.md"),
                Path::new("assets/icon.bin"),
                Path::new("references/guide.md"),
                Path::new("scripts/run.sh"),
            ]
        );
        assert_eq!(inventory.files()[1].expected_bytes(), 4);
        assert_eq!(
            inventory.total_bytes(),
            inventory
                .files()
                .iter()
                .map(|file| file.expected_bytes())
                .sum::<u64>()
        );
        assert_eq!(
            inventory.directories(),
            [
                Path::new("assets"),
                Path::new("references"),
                Path::new("scripts")
            ]
        );
    }

    #[test]
    fn missing_changed_and_extra_staged_files_fail_closed() {
        let parent = tempdir().expect("temporary parent");
        let staged = make_staged(parent.path());
        let path = staged.content_directory().join("references/guide.md");
        fs::remove_file(&path).expect("remove staged reference");
        assert!(matches!(
            inspect_staged_skill(&staged),
            Err(InventoryError::ManifestPathMismatch { .. })
        ));

        let parent = tempdir().expect("temporary parent");
        let staged = make_staged(parent.path());
        let path = staged.content_directory().join("references/guide.md");
        fs::write(&path, b"changed length").expect("change staged file");
        assert!(matches!(
            inspect_staged_skill(&staged),
            Err(InventoryError::SizeMismatch { .. })
        ));

        let parent = tempdir().expect("temporary parent");
        let staged = make_staged(parent.path());
        let path = staged.content_directory().join("references/guide.md");
        fs::write(&path, b"REFerence").expect("change staged file without changing its size");
        assert!(matches!(
            inspect_staged_skill(&staged),
            Err(InventoryError::ChecksumMismatch { .. })
        ));

        let parent = tempdir().expect("temporary parent");
        let staged = make_staged(parent.path());
        let path = staged.content_directory().join("unexpected.txt");
        fs::write(&path, b"unexpected").expect("add staged file");
        assert!(matches!(
            inspect_staged_skill(&staged),
            Err(InventoryError::ManifestPathMismatch { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn staged_symlink_is_rejected_without_following_it() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().expect("temporary parent");
        let outside = tempdir().expect("outside");
        fs::write(outside.path().join("data"), b"outside").expect("outside file");
        let staged = make_staged(parent.path());
        let reference = staged.content_directory().join("references/guide.md");
        fs::remove_file(&reference).expect("remove stage file");
        symlink(outside.path().join("data"), &reference).expect("replace with symlink");
        assert!(matches!(
            inspect_staged_skill(&staged),
            Err(InventoryError::UnsupportedObject { .. })
        ));
    }
}
