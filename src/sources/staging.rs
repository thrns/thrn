use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
};

use tempfile::{Builder, TempDir};
use thiserror::Error;
use zip::{ZipArchive, read::ZipFile};

use crate::{
    models::catalog::{CommitSha, Resource, SkillId, SkillName, SourceIdentity},
    skills::validator::{SkillMetadata, ValidationError, validate_skill_directory},
};

use super::{
    archive::{ArchiveEntry, ArchiveEntryKind, ValidatedArchive},
    selection::SelectedSkill,
};

/// Supplies an unpredictable, exclusively-created private directory for a staging operation.
pub trait TempDirFactory: Send + Sync {
    fn create_private(&self) -> io::Result<TempDir>;
}

/// Default OS temporary-directory factory. `tempfile` creates the directory exclusively with
/// owner-only access on Unix; its `TempDir` guard owns cleanup and staged-content lifetime.
pub struct SystemTempDirFactory;

impl TempDirFactory for SystemTempDirFactory {
    fn create_private(&self) -> io::Result<TempDir> {
        Builder::new().prefix("thrn-skill-").tempdir()
    }
}

/// A complete skill whose contents live in a private temporary directory.
///
/// The directory is owned by this value and is removed when the value is dropped. Callers that
/// need the staged content longer must keep this value alive. Its `TempDir` guard attempts to
/// remove the owned directory when dropped. Files remain inert; this API never executes
/// `SKILL.md`, scripts, or other content.
#[derive(Debug)]
pub struct StagedSkill {
    catalog_id: SkillId,
    skill_name: SkillName,
    revision: CommitSha,
    source: SourceIdentity,
    metadata: SkillMetadata,
    expected_entries: Vec<StagedEntry>,
    content_directory: PathBuf,
    _temporary_directory: TempDir,
}

/// A selected path and kind from the already validated archive manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedEntry {
    relative_path: PathBuf,
    kind: StagedEntryKind,
    expected_bytes: u64,
    expected_crc32: u32,
}

impl StagedEntry {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn kind(&self) -> StagedEntryKind {
        self.kind
    }

    pub fn expected_bytes(&self) -> u64 {
        self.expected_bytes
    }

    pub fn expected_crc32(&self) -> u32 {
        self.expected_crc32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagedEntryKind {
    File,
    Directory,
}

impl StagedSkill {
    pub fn catalog_id(&self) -> &SkillId {
        &self.catalog_id
    }

    pub fn skill_name(&self) -> &SkillName {
        &self.skill_name
    }

    pub fn revision(&self) -> &CommitSha {
        &self.revision
    }

    pub fn source(&self) -> &SourceIdentity {
        &self.source
    }

    pub fn metadata(&self) -> &SkillMetadata {
        &self.metadata
    }

    pub fn content_directory(&self) -> &Path {
        &self.content_directory
    }

    /// Exact selected file and directory inventory from the validated archive manifest.
    pub fn expected_entries(&self) -> &[StagedEntry] {
        &self.expected_entries
    }
}

#[derive(Debug, Error)]
pub enum StageError {
    #[error("could not create an isolated temporary staging directory ({kind})")]
    CreateTemporaryDirectory { kind: io::ErrorKind },
    #[error("temporary staging root is not a real directory")]
    UnsafeTemporaryDirectory,
    #[error("could not {action} staged content ({kind})")]
    Filesystem {
        action: &'static str,
        kind: io::ErrorKind,
    },
    #[error("validated archive could not be reopened for extraction")]
    ReopenArchive,
    #[error("validated archive manifest and ZIP entry disagree during extraction")]
    ManifestMismatch,
    #[error("archive entry could not be extracted ({kind})")]
    ExtractEntry { kind: io::ErrorKind },
    #[error("archive entry expanded size did not match the validated manifest")]
    ExpandedSizeMismatch,
    #[error("staged SKILL.md failed local metadata validation")]
    InvalidSkillMetadata { errors: Vec<ValidationError> },
    #[error("staged skill metadata name does not match the catalog skillName")]
    CatalogSkillNameMismatch,
    #[error("staging failed and isolated temporary cleanup also failed ({kind})")]
    CleanupFailed {
        operation: Box<StageError>,
        kind: io::ErrorKind,
    },
}

/// Extract only manifest-authorized regular files and directories below the selected source path.
pub(crate) fn stage_selected_skill(
    resource: &Resource,
    archive_bytes: &[u8],
    manifest: &ValidatedArchive,
    selection: &SelectedSkill,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<StagedSkill, StageError> {
    stage_selected_skill_with_file_system(
        resource,
        archive_bytes,
        manifest,
        selection,
        tempdir_factory,
        &FileSystem,
    )
}

fn stage_selected_skill_with_file_system(
    resource: &Resource,
    archive_bytes: &[u8],
    manifest: &ValidatedArchive,
    selection: &SelectedSkill,
    tempdir_factory: &dyn TempDirFactory,
    file_system: &dyn StageFileSystem,
) -> Result<StagedSkill, StageError> {
    let temporary_directory = tempdir_factory
        .create_private()
        .map_err(|error| StageError::CreateTemporaryDirectory { kind: error.kind() })?;
    let root = temporary_directory.path().to_path_buf();
    let expected_entries = selected_manifest_entries(manifest, selection)?;
    let result = stage_contents(
        resource,
        archive_bytes,
        manifest,
        selection,
        &root,
        file_system,
    );
    match result {
        Ok((content_directory, metadata)) => Ok(StagedSkill {
            catalog_id: resource.id().clone(),
            skill_name: resource.skill_name().clone(),
            revision: resource.source().commit().clone(),
            source: resource.source().clone(),
            metadata,
            expected_entries,
            content_directory,
            _temporary_directory: temporary_directory,
        }),
        Err(operation) => match temporary_directory.close() {
            Ok(()) => Err(operation),
            Err(error) => Err(StageError::CleanupFailed {
                operation: Box::new(operation),
                kind: error.kind(),
            }),
        },
    }
}

fn selected_manifest_entries(
    manifest: &ValidatedArchive,
    selection: &SelectedSkill,
) -> Result<Vec<StagedEntry>, StageError> {
    let prefix = format!("{}/", selection.source_directory());
    let mut entries = Vec::new();
    for entry in manifest.entries() {
        let Some(relative) = entry.path().as_str().strip_prefix(&prefix) else {
            continue;
        };
        let relative_path = checked_relative_path(relative)?;
        entries.push(StagedEntry {
            relative_path,
            kind: match entry.kind() {
                ArchiveEntryKind::File => StagedEntryKind::File,
                ArchiveEntryKind::Directory => StagedEntryKind::Directory,
            },
            expected_bytes: entry.uncompressed_bytes(),
            expected_crc32: entry.crc32(),
        });
    }
    Ok(entries)
}

trait StageFileSystem {
    fn create_directory(&self, path: &Path) -> io::Result<()>;
    fn create_file(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
}

struct FileSystem;

impl StageFileSystem for FileSystem {
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(path)
        }
    }

    fn create_file(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "created staging object is not a regular file",
            ));
        }
        Ok(Box::new(file))
    }
}

fn stage_contents(
    resource: &Resource,
    archive_bytes: &[u8],
    manifest: &ValidatedArchive,
    selection: &SelectedSkill,
    temporary_root: &Path,
    file_system: &dyn StageFileSystem,
) -> Result<(PathBuf, SkillMetadata), StageError> {
    require_real_directory(temporary_root)?;

    let selected_name = selection
        .source_directory()
        .rsplit('/')
        .next()
        .ok_or(StageError::ManifestMismatch)?;
    let content_directory = temporary_root.join(selected_name);
    file_system
        .create_directory(&content_directory)
        .map_err(|error| StageError::Filesystem {
            action: "create the selected skill directory",
            kind: error.kind(),
        })?;
    require_directory_chain(temporary_root, &content_directory)?;

    let archive =
        ZipArchive::new(Cursor::new(archive_bytes)).map_err(|_| StageError::ReopenArchive)?;
    let mut archive = archive;
    let prefix = format!("{}/", selection.source_directory());
    let mut created_directories = BTreeSet::new();
    created_directories.insert(PathBuf::new());

    for entry in manifest.entries() {
        let path = entry.path().as_str();
        if path == selection.source_directory() {
            if entry.kind() != ArchiveEntryKind::Directory {
                return Err(StageError::ManifestMismatch);
            }
            continue;
        }
        let Some(relative_path) = path.strip_prefix(&prefix) else {
            continue;
        };
        let target = checked_relative_path(relative_path)?;
        match entry.kind() {
            ArchiveEntryKind::Directory => {
                create_manifest_directory(
                    file_system,
                    &content_directory,
                    &target,
                    &mut created_directories,
                )?;
            }
            ArchiveEntryKind::File => {
                let parent = target.parent().ok_or(StageError::ManifestMismatch)?;
                create_manifest_directory(
                    file_system,
                    &content_directory,
                    parent,
                    &mut created_directories,
                )?;
                let output_path = content_directory.join(&target);
                require_directory_chain(
                    &content_directory,
                    output_path.parent().ok_or(StageError::ManifestMismatch)?,
                )?;
                let mut output = file_system.create_file(&output_path).map_err(|error| {
                    StageError::Filesystem {
                        action: "create a staged file",
                        kind: error.kind(),
                    }
                })?;
                extract_manifest_file(&mut archive, entry, &mut output)?;
                output
                    .flush()
                    .map_err(|error| StageError::ExtractEntry { kind: error.kind() })?;
            }
        }
    }

    let metadata = validate_skill_directory(&content_directory)
        .map_err(|errors| StageError::InvalidSkillMetadata { errors })?;
    if metadata.name != resource.skill_name().as_str() {
        return Err(StageError::CatalogSkillNameMismatch);
    }
    Ok((content_directory, metadata))
}

fn checked_relative_path(value: &str) -> Result<PathBuf, StageError> {
    if value.is_empty() {
        return Err(StageError::ManifestMismatch);
    }
    let mut result = PathBuf::new();
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." || component.contains('\\')
        {
            return Err(StageError::ManifestMismatch);
        }
        result.push(component);
    }
    Ok(result)
}

fn create_manifest_directory(
    file_system: &dyn StageFileSystem,
    content_root: &Path,
    relative: &Path,
    created: &mut BTreeSet<PathBuf>,
) -> Result<(), StageError> {
    if relative.as_os_str().is_empty() || created.contains(relative) {
        return Ok(());
    }
    let parent = relative.parent().ok_or(StageError::ManifestMismatch)?;
    create_manifest_directory(file_system, content_root, parent, created)?;

    let full_path = content_root.join(relative);
    require_directory_chain(
        content_root,
        full_path.parent().ok_or(StageError::ManifestMismatch)?,
    )?;
    file_system
        .create_directory(&full_path)
        .map_err(|error| StageError::Filesystem {
            action: "create a staged directory",
            kind: error.kind(),
        })?;
    require_directory_chain(content_root, &full_path)?;
    created.insert(relative.to_path_buf());
    Ok(())
}

fn require_real_directory(path: &Path) -> Result<(), StageError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| StageError::Filesystem {
        action: "inspect a staging directory",
        kind: error.kind(),
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(StageError::UnsafeTemporaryDirectory);
    }
    Ok(())
}

fn require_directory_chain(root: &Path, directory: &Path) -> Result<(), StageError> {
    require_real_directory(root)?;
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| StageError::ManifestMismatch)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(StageError::ManifestMismatch);
        };
        current.push(component);
        require_real_directory(&current)?;
    }
    Ok(())
}

fn extract_manifest_file(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    manifest_entry: &ArchiveEntry,
    output: &mut dyn Write,
) -> Result<(), StageError> {
    let mut zip_file = archive
        .by_name(manifest_entry.path().as_str())
        .map_err(|_| StageError::ManifestMismatch)?;
    if zip_file.is_dir()
        || zip_file.size() != manifest_entry.uncompressed_bytes()
        || zip_file.compressed_size() != manifest_entry.compressed_bytes()
        || zip_file.crc32() != manifest_entry.crc32()
    {
        return Err(StageError::ManifestMismatch);
    }
    copy_verified_entry(&mut zip_file, manifest_entry.uncompressed_bytes(), output)
}

fn copy_verified_entry(
    zip_file: &mut ZipFile<'_, Cursor<&[u8]>>,
    expected_bytes: u64,
    output: &mut dyn Write,
) -> Result<(), StageError> {
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = zip_file
            .read(&mut buffer)
            .map_err(|error| StageError::ExtractEntry { kind: error.kind() })?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(u64::try_from(read).map_err(|_| StageError::ExpandedSizeMismatch)?)
            .ok_or(StageError::ExpandedSizeMismatch)?;
        if copied > expected_bytes {
            return Err(StageError::ExpandedSizeMismatch);
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| StageError::ExtractEntry { kind: error.kind() })?;
    }
    if copied != expected_bytes {
        return Err(StageError::ExpandedSizeMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Cursor, Write},
        path::{Path, PathBuf},
    };

    use zip::{CompressionMethod, System, ZipWriter, write::SimpleFileOptions};

    use crate::{
        registry::catalog::parse_catalog_json,
        sources::{archive::validate_zip_archive, selection::select_skill_directory},
    };

    use super::{
        FileSystem, StageError, StageFileSystem, TempDirFactory,
        stage_selected_skill_with_file_system,
    };

    struct TestFactory(PathBuf);

    impl TempDirFactory for TestFactory {
        fn create_private(&self) -> std::io::Result<tempfile::TempDir> {
            tempfile::Builder::new()
                .prefix("stage-test-")
                .tempdir_in(&self.0)
        }
    }

    struct FailedFactory;

    impl TempDirFactory for FailedFactory {
        fn create_private(&self) -> std::io::Result<tempfile::TempDir> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected temp directory failure",
            ))
        }
    }

    struct FailDirectoryCreation;

    impl StageFileSystem for FailDirectoryCreation {
        fn create_directory(&self, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected directory creation failure",
            ))
        }

        fn create_file(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
            FileSystem.create_file(path)
        }
    }

    struct FailWrite;

    impl Write for FailWrite {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "injected file write failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FailFileWrite;

    impl StageFileSystem for FailFileWrite {
        fn create_directory(&self, path: &Path) -> std::io::Result<()> {
            FileSystem.create_directory(path)
        }

        fn create_file(&self, _: &Path) -> std::io::Result<Box<dyn Write + Send>> {
            Ok(Box::new(FailWrite))
        }
    }

    struct MoveRootAndFail(PathBuf);

    impl StageFileSystem for MoveRootAndFail {
        fn create_directory(&self, path: &Path) -> std::io::Result<()> {
            let root = path.parent().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing test root")
            })?;
            fs::rename(root, &self.0)?;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected operation failure after moving the temporary root",
            ))
        }

        fn create_file(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
            FileSystem.create_file(path)
        }
    }

    #[cfg(unix)]
    struct SymlinkAssetDirectory(PathBuf);

    #[cfg(unix)]
    impl StageFileSystem for SymlinkAssetDirectory {
        fn create_directory(&self, path: &Path) -> std::io::Result<()> {
            use std::os::unix::fs::symlink;
            if path.file_name().is_some_and(|name| name == "assets") {
                symlink(&self.0, path)
            } else {
                FileSystem.create_directory(path)
            }
        }

        fn create_file(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
            FileSystem.create_file(path)
        }
    }

    fn fixture() -> (
        crate::models::catalog::Resource,
        Vec<u8>,
        crate::sources::archive::ValidatedArchive,
        crate::sources::selection::SelectedSkill,
    ) {
        let catalog = parse_catalog_json(include_bytes!("../registry/builtin-v1.json"))
            .expect("built-in source fixture is validated");
        let resource = catalog.resources()[0].clone();
        let root = format!(
            "{}-{}",
            resource.source().repository().as_str(),
            resource.source().commit().as_str()
        );
        let skill_directory = format!("{root}/{}", resource.source().path().as_str());
        let skill = format!(
            "---\nname: {}\ndescription: staging test skill\n---\nbody\n",
            resource.skill_name().as_str()
        );
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_directory(
                format!("{root}/"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::STORE)
                    .system(System::Unix)
                    .unix_permissions(0o040755),
            )
            .expect("root directory");
        writer
            .add_directory(
                format!("{skill_directory}/"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::STORE)
                    .system(System::Unix)
                    .unix_permissions(0o040755),
            )
            .expect("skill directory");
        writer
            .start_file(
                format!("{skill_directory}/SKILL.md"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::STORE)
                    .system(System::Unix)
                    .unix_permissions(0o100644),
            )
            .expect("metadata file");
        writer.write_all(skill.as_bytes()).expect("metadata bytes");
        writer
            .add_directory(
                format!("{skill_directory}/assets/"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::STORE)
                    .system(System::Unix)
                    .unix_permissions(0o040755),
            )
            .expect("assets directory");
        writer
            .start_file(
                format!("{skill_directory}/assets/blob.bin"),
                SimpleFileOptions::default()
                    .compression_method(CompressionMethod::STORE)
                    .system(System::Unix)
                    .unix_permissions(0o100644),
            )
            .expect("asset file");
        writer.write_all(&[0, 1, 2, 255]).expect("asset bytes");
        let bytes = writer.finish().expect("archive finish").into_inner();
        let manifest = validate_zip_archive(&bytes).expect("archive validates");
        let selection = select_skill_directory(&resource, &manifest).expect("source selection");
        (resource, bytes, manifest, selection)
    }

    fn stage(
        resource: &crate::models::catalog::Resource,
        bytes: &[u8],
        manifest: &crate::sources::archive::ValidatedArchive,
        selection: &crate::sources::selection::SelectedSkill,
        parent: &Path,
        filesystem: &dyn StageFileSystem,
    ) -> Result<super::StagedSkill, StageError> {
        stage_selected_skill_with_file_system(
            resource,
            bytes,
            manifest,
            selection,
            &TestFactory(parent.to_path_buf()),
            filesystem,
        )
    }

    #[test]
    fn temporary_creation_failure_does_not_return_staged_content() {
        let (resource, bytes, manifest, selection) = fixture();
        let error = stage_selected_skill_with_file_system(
            &resource,
            &bytes,
            &manifest,
            &selection,
            &FailedFactory,
            &FileSystem,
        )
        .expect_err("injected temp directory failure must fail");
        assert!(matches!(error, StageError::CreateTemporaryDirectory { .. }));
    }

    #[test]
    fn directory_creation_and_file_write_failures_clean_the_partial_temp_root() {
        let (resource, bytes, manifest, selection) = fixture();
        for filesystem in [
            &FailDirectoryCreation as &dyn StageFileSystem,
            &FailFileWrite as &dyn StageFileSystem,
        ] {
            let parent = tempfile::tempdir().expect("isolated parent");
            let result = stage(
                &resource,
                &bytes,
                &manifest,
                &selection,
                parent.path(),
                filesystem,
            );
            assert!(result.is_err());
            assert_eq!(fs::read_dir(parent.path()).expect("parent").count(), 0);
        }
    }

    #[test]
    fn cleanup_failure_is_reported_and_never_returns_staged_content() {
        let (resource, bytes, manifest, selection) = fixture();
        let parent = tempfile::tempdir().expect("isolated parent");
        let moved_root = parent.path().join("moved-stage-root");
        let result = stage(
            &resource,
            &bytes,
            &manifest,
            &selection,
            parent.path(),
            &MoveRootAndFail(moved_root.clone()),
        );
        assert!(matches!(result, Err(StageError::CleanupFailed { .. })));
        assert!(moved_root.is_dir());
        fs::remove_dir_all(&moved_root).expect("remove intentionally moved test root");
        assert_eq!(fs::read_dir(parent.path()).expect("parent").count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn nested_symlink_is_detected_before_any_file_is_written_outside_the_root() {
        let (resource, bytes, manifest, selection) = fixture();
        let parent = tempfile::tempdir().expect("isolated parent");
        let outside = tempfile::tempdir().expect("outside sentinel");
        let result = stage(
            &resource,
            &bytes,
            &manifest,
            &selection,
            parent.path(),
            &SymlinkAssetDirectory(outside.path().to_path_buf()),
        );
        assert!(matches!(result, Err(StageError::UnsafeTemporaryDirectory)));
        assert_eq!(
            fs::read_dir(parent.path()).expect("staging parent").count(),
            0
        );
        assert_eq!(fs::read_dir(outside.path()).expect("outside").count(), 0);
    }
}
