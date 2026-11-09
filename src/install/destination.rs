use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::agents::AgentId;

/// Maximum entries retained from any inspected project or destination directory.
pub const MAX_DESTINATION_DIRECTORY_ENTRIES: usize = 16_384;

/// A no-follow classification of one filesystem object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

/// A directory entry observed without opening or following its contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntrySnapshot {
    name: OsString,
    kind: ObjectKind,
}

/// Stable identity for a directory observed during planning, where the platform exposes one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    volume: u64,
    object: u64,
}

impl FileIdentity {
    pub fn new(volume: u64, object: u64) -> Self {
        Self { volume, object }
    }

    /// Filesystem volume identifier where available (device ID on Unix).
    pub fn volume(&self) -> u64 {
        self.volume
    }

    /// Filesystem object identifier (inode on Unix, file index on Windows).
    pub fn object(&self) -> u64 {
        self.object
    }
}

impl DirectoryEntrySnapshot {
    #[allow(dead_code)]
    pub(crate) fn new(name: std::ffi::OsString, kind: ObjectKind) -> Self {
        Self { name, kind }
    }

    pub fn name(&self) -> &std::ffi::OsStr {
        &self.name
    }

    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
}

/// Read-only filesystem boundary used by the destination inspector and injectable tests.
pub trait DestinationFileSystem: Send + Sync {
    fn object_kind(&self, path: &Path) -> io::Result<ObjectKind>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn read_directory(&self, path: &Path) -> io::Result<Vec<DirectoryEntrySnapshot>>;

    fn object_identity(&self, _path: &Path) -> io::Result<Option<FileIdentity>> {
        Ok(None)
    }
}

/// Standard-library filesystem adapter. All object checks use `symlink_metadata`.
pub struct RealDestinationFileSystem;

impl DestinationFileSystem for RealDestinationFileSystem {
    fn object_kind(&self, path: &Path) -> io::Result<ObjectKind> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(if metadata_is_link_or_reparse(&metadata) {
            ObjectKind::Symlink
        } else if metadata.is_dir() {
            ObjectKind::Directory
        } else if metadata.is_file() {
            ObjectKind::RegularFile
        } else {
            ObjectKind::Other
        })
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn read_directory(&self, path: &Path) -> io::Result<Vec<DirectoryEntrySnapshot>> {
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(path)? {
            if snapshots.len() == MAX_DESTINATION_DIRECTORY_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "destination directory entry limit exceeded",
                ));
            }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            let kind = if metadata_is_link_or_reparse(&metadata) {
                ObjectKind::Symlink
            } else if metadata.is_dir() {
                ObjectKind::Directory
            } else if metadata.is_file() {
                ObjectKind::RegularFile
            } else {
                ObjectKind::Other
            };
            snapshots.push(DirectoryEntrySnapshot {
                name: entry.file_name(),
                kind,
            });
        }
        Ok(snapshots)
    }

    fn object_identity(&self, path: &Path) -> io::Result<Option<FileIdentity>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::symlink_metadata(path)?;
            return Ok(Some(FileIdentity::new(metadata.dev(), metadata.ino())));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            };
            let mut options = fs::OpenOptions::new();
            options
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
            let file = options.open(path)?;
            let opened_metadata = file.metadata()?;
            if metadata_is_link_or_reparse(&opened_metadata) || !opened_metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "directory changed while reading its identity",
                ));
            }
            return windows_file_identity(&file).map(Some);
        }
        #[allow(unreachable_code)]
        Ok(None)
    }
}

#[cfg(windows)]
pub(crate) fn windows_file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::{
        Win32::{Foundation::HANDLE, Storage::FileSystem::GetFileInformationByHandle},
        core::BOOL,
    };
    let mut information =
        windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` is a live OS file handle and `information` is a writable out parameter of
    // the Win32 API's documented structure type.
    let result: BOOL =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(FileIdentity::new(
        u64::from(information.dwVolumeSerialNumber),
        index,
    ))
}

fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes() & 0x400 != 0;
    }
    #[allow(unreachable_code)]
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentState {
    Missing,
    Directory,
    RegularFile,
    Symlink,
    Other,
    NotInspected,
}

impl From<ObjectKind> for ComponentState {
    fn from(value: ObjectKind) -> Self {
        match value {
            ObjectKind::Directory => Self::Directory,
            ObjectKind::RegularFile => Self::RegularFile,
            ObjectKind::Symlink => Self::Symlink,
            ObjectKind::Other => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    ExistingDirectory,
    ExistingFile,
    ExistingSymlink,
    ExistingUnsupportedObject,
    CaseInsensitiveCollision,
    UnsafeDestinationComponent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationConflict {
    path: PathBuf,
    kind: ConflictKind,
    detail: String,
}

impl DestinationConflict {
    pub(crate) fn new(path: PathBuf, kind: ConflictKind, detail: String) -> Self {
        Self { path, kind, detail }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> ConflictKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Read-only assumptions about the selected project and agent destination.
///
/// M2-04 can inspect again and compare this value before any write. It is not an authorization
/// token and cannot prevent changes that occur after reinspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationSnapshot {
    agent: AgentId,
    agent_directory: String,
    project_root: PathBuf,
    project_identity: Option<FileIdentity>,
    agents_state: ComponentState,
    agents_identity: Option<FileIdentity>,
    skills_state: ComponentState,
    skills_identity: Option<FileIdentity>,
    skill_state: ComponentState,
    destination: PathBuf,
    existing_project_entries: Vec<DirectoryEntrySnapshot>,
    existing_agents_entries: Vec<DirectoryEntrySnapshot>,
    existing_skill_entries: Vec<DirectoryEntrySnapshot>,
    conflicts: Vec<DestinationConflict>,
}

impl DestinationSnapshot {
    pub(crate) fn same_destination_state_except_project_entries(&self, other: &Self) -> bool {
        self.agent == other.agent
            && self.agent_directory == other.agent_directory
            && self.project_root == other.project_root
            && self.project_identity == other.project_identity
            && self.agents_state == other.agents_state
            && self.agents_identity == other.agents_identity
            && self.skills_state == other.skills_state
            && self.skills_identity == other.skills_identity
            && self.skill_state == other.skill_state
            && self.destination == other.destination
            && self.existing_agents_entries == other.existing_agents_entries
            && self.existing_skill_entries == other.existing_skill_entries
            && self.conflicts == other.conflicts
    }

    pub(crate) fn same_shared_path_state(&self, other: &Self) -> bool {
        self.agent == AgentId::Codex
            && other.agent == AgentId::Codex
            && self.project_root == other.project_root
            && self.project_identity == other.project_identity
            && self.agents_state == other.agents_state
            && self.agents_identity == other.agents_identity
            && self.skills_state == other.skills_state
            && self.skills_identity == other.skills_identity
            && self.skill_state == other.skill_state
            && self.destination == other.destination
            && self.existing_agents_entries == other.existing_agents_entries
            && self.existing_skill_entries == other.existing_skill_entries
        // Top-level project entries are intentionally excluded: creating the selected agent's
        // native directory does not alter the shared .agents/skills compatibility state.
    }

    pub(crate) fn agent(&self) -> AgentId {
        self.agent
    }

    pub(crate) fn agent_directory(&self) -> &str {
        &self.agent_directory
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_identity(&self) -> Option<FileIdentity> {
        self.project_identity
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn agents_state(&self) -> ComponentState {
        self.agents_state
    }

    pub fn agents_identity(&self) -> Option<FileIdentity> {
        self.agents_identity
    }

    pub fn skills_state(&self) -> ComponentState {
        self.skills_state
    }

    pub fn skills_identity(&self) -> Option<FileIdentity> {
        self.skills_identity
    }

    pub fn skill_state(&self) -> ComponentState {
        self.skill_state
    }

    pub fn existing_skill_entries(&self) -> &[DirectoryEntrySnapshot] {
        &self.existing_skill_entries
    }

    pub fn existing_project_entries(&self) -> &[DirectoryEntrySnapshot] {
        &self.existing_project_entries
    }

    pub fn existing_agents_entries(&self) -> &[DirectoryEntrySnapshot] {
        &self.existing_agents_entries
    }

    pub fn conflicts(&self) -> &[DestinationConflict] {
        &self.conflicts
    }
}

#[derive(Debug, Error)]
pub enum DestinationError {
    #[error("project path must not be empty or contain a parent-directory component (`..`)")]
    AmbiguousProjectPath,
    #[error("project path must be absolute after resolving it from the current directory")]
    ProjectPathNotAbsolute,
    #[error("project directory does not exist: {path:?}")]
    MissingProjectDirectory { path: PathBuf },
    #[error("project path is a symbolic link; select its resolved directory explicitly: {path:?}")]
    ProjectRootSymlink { path: PathBuf },
    #[error("project path is not an existing directory: {path:?}")]
    ProjectRootNotDirectory { path: PathBuf },
    #[error(
        "directory contains more than {maximum} entries and cannot be inspected safely: {path:?}"
    )]
    TooManyEntries { path: PathBuf, maximum: usize },
    #[error("could not {action} filesystem path {path:?} ({kind})")]
    Filesystem {
        action: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
    },
}

/// Resolve an optional project argument, then inspect the exact Codex destination without writes.
pub fn inspect_destination(
    project: Option<&Path>,
    current_directory: &Path,
    skill_name: &str,
) -> Result<DestinationSnapshot, DestinationError> {
    inspect_destination_for_agent(AgentId::Codex, project, current_directory, skill_name)
}

pub(crate) fn inspect_destination_for_agent(
    agent: AgentId,
    project: Option<&Path>,
    current_directory: &Path,
    skill_name: &str,
) -> Result<DestinationSnapshot, DestinationError> {
    inspect_destination_with_file_system_for_agent(
        agent,
        project,
        current_directory,
        skill_name,
        &RealDestinationFileSystem,
    )
}

/// Injectable counterpart used by tests and future destination adapters.
pub fn inspect_destination_with_file_system(
    project: Option<&Path>,
    current_directory: &Path,
    skill_name: &str,
    file_system: &dyn DestinationFileSystem,
) -> Result<DestinationSnapshot, DestinationError> {
    inspect_destination_with_file_system_for_agent(
        AgentId::Codex,
        project,
        current_directory,
        skill_name,
        file_system,
    )
}

pub(crate) fn inspect_destination_with_file_system_for_agent(
    agent: AgentId,
    project: Option<&Path>,
    current_directory: &Path,
    skill_name: &str,
    file_system: &dyn DestinationFileSystem,
) -> Result<DestinationSnapshot, DestinationError> {
    let selected = match project {
        Some(path) if path.as_os_str().is_empty() => {
            return Err(DestinationError::AmbiguousProjectPath);
        }
        Some(path) => {
            if path.components().any(|part| part == Component::ParentDir) {
                return Err(DestinationError::AmbiguousProjectPath);
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
        return Err(DestinationError::ProjectPathNotAbsolute);
    }

    let selected_kind = object_kind(file_system, &selected)?;
    match selected_kind {
        None => {
            return Err(DestinationError::MissingProjectDirectory { path: selected });
        }
        Some(ObjectKind::Symlink) => {
            return Err(DestinationError::ProjectRootSymlink { path: selected });
        }
        Some(ObjectKind::Directory) => {}
        Some(_) => return Err(DestinationError::ProjectRootNotDirectory { path: selected }),
    }
    let project_root = file_system
        .canonicalize(&selected)
        .map_err(|error| filesystem_error("resolve", &selected, error))?;
    match object_kind(file_system, &project_root)? {
        Some(ObjectKind::Directory) => {}
        Some(ObjectKind::Symlink) => {
            return Err(DestinationError::ProjectRootSymlink { path: project_root });
        }
        Some(_) => {
            return Err(DestinationError::ProjectRootNotDirectory { path: project_root });
        }
        None => {
            return Err(DestinationError::MissingProjectDirectory { path: project_root });
        }
    }
    let project_identity = file_system
        .object_identity(&project_root)
        .map_err(|error| filesystem_error("identify", &project_root, error))?;

    let agent_directory = agent.adapter().skill_directory();
    let agents = project_root.join(agent_directory);
    let skills = agents.join("skills");
    let destination = skills.join(skill_name);
    if !destination.starts_with(&project_root) {
        return Err(DestinationError::AmbiguousProjectPath);
    }

    let mut conflicts = Vec::new();
    let existing_project_entries = read_directory_snapshot(file_system, &project_root)?;
    record_component_collisions(
        &existing_project_entries,
        agent_directory,
        &project_root,
        &mut conflicts,
    );
    let agents_state = component_state(file_system, &agents)?;
    let agents_identity = if agents_state == ComponentState::Directory {
        file_system
            .object_identity(&agents)
            .map_err(|error| filesystem_error("identify", &agents, error))?
    } else {
        None
    };
    if is_unsafe_component(agents_state) {
        conflicts.push(unsafe_component_conflict(&agents));
    }

    let mut existing_agents_entries = Vec::new();
    if agents_state == ComponentState::Directory {
        existing_agents_entries = read_directory_snapshot(file_system, &agents)?;
        record_component_collisions(&existing_agents_entries, "skills", &agents, &mut conflicts);
    }
    let skills_state = match agents_state {
        ComponentState::Directory => component_state(file_system, &skills)?,
        ComponentState::Missing => ComponentState::Missing,
        _ => ComponentState::NotInspected,
    };
    let skills_identity = if skills_state == ComponentState::Directory {
        file_system
            .object_identity(&skills)
            .map_err(|error| filesystem_error("identify", &skills, error))?
    } else {
        None
    };
    if is_unsafe_component(skills_state) {
        conflicts.push(unsafe_component_conflict(&skills));
    }

    let mut existing_skill_entries = Vec::new();
    let skill_state = match skills_state {
        ComponentState::Directory => {
            let entries = read_directory_snapshot(file_system, &skills)?;
            for entry in &entries {
                let Some(name) = entry.name.to_str() else {
                    continue;
                };
                if name.eq_ignore_ascii_case(skill_name) {
                    let path = skills.join(&entry.name);
                    let (kind, detail) = if name == skill_name {
                        match entry.kind {
                            ObjectKind::Directory => (
                                ConflictKind::ExistingDirectory,
                                "a skill directory already exists here; it may be manually installed or externally managed, so THRN will not merge, rename, overwrite, or remove it".to_owned(),
                            ),
                            ObjectKind::RegularFile => (
                                ConflictKind::ExistingFile,
                                "a regular file already occupies the skill destination; choose another project or resolve the file manually".to_owned(),
                            ),
                            ObjectKind::Symlink => (
                                ConflictKind::ExistingSymlink,
                                "a symbolic link already occupies the skill destination; THRN will not follow it".to_owned(),
                            ),
                            ObjectKind::Other => (
                                ConflictKind::ExistingUnsupportedObject,
                                "an unsupported filesystem object already occupies the skill destination".to_owned(),
                            ),
                        }
                    } else {
                        (
                            ConflictKind::CaseInsensitiveCollision,
                            format!(
                                "existing entry {:?} collides with skill name {skill_name:?} on case-insensitive filesystems; resolve it manually",
                                entry.name
                            ),
                        )
                    };
                    conflicts.push(DestinationConflict::new(path, kind, detail));
                }
            }
            existing_skill_entries = entries;
            if conflicts.iter().any(|conflict| {
                conflict.path.starts_with(&skills)
                    && conflict.kind != ConflictKind::UnsafeDestinationComponent
            }) {
                ComponentState::NotInspected
            } else {
                ComponentState::Missing
            }
        }
        ComponentState::Missing => ComponentState::Missing,
        _ => ComponentState::NotInspected,
    };

    Ok(DestinationSnapshot {
        agent,
        agent_directory: agent_directory.to_owned(),
        project_root,
        project_identity,
        agents_state,
        agents_identity,
        skills_state,
        skills_identity,
        skill_state,
        destination,
        existing_project_entries,
        existing_agents_entries,
        existing_skill_entries,
        conflicts,
    })
}

fn read_directory_snapshot(
    file_system: &dyn DestinationFileSystem,
    path: &Path,
) -> Result<Vec<DirectoryEntrySnapshot>, DestinationError> {
    let mut entries = file_system.read_directory(path).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            DestinationError::TooManyEntries {
                path: path.to_path_buf(),
                maximum: MAX_DESTINATION_DIRECTORY_ENTRIES,
            }
        } else {
            filesystem_error("read", path, error)
        }
    })?;
    if entries.len() > MAX_DESTINATION_DIRECTORY_ENTRIES {
        return Err(DestinationError::TooManyEntries {
            path: path.to_path_buf(),
            maximum: MAX_DESTINATION_DIRECTORY_ENTRIES,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn record_component_collisions(
    entries: &[DirectoryEntrySnapshot],
    expected_name: &str,
    parent: &Path,
    conflicts: &mut Vec<DestinationConflict>,
) {
    for entry in entries {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if name != expected_name && name.eq_ignore_ascii_case(expected_name) {
            conflicts.push(DestinationConflict::new(
                parent.join(&entry.name),
                ConflictKind::CaseInsensitiveCollision,
                format!(
                    "existing entry {name:?} collides with destination component {expected_name:?} on case-insensitive filesystems; resolve it manually"
                ),
            ));
        }
    }
}

fn object_kind(
    file_system: &dyn DestinationFileSystem,
    path: &Path,
) -> Result<Option<ObjectKind>, DestinationError> {
    match file_system.object_kind(path) {
        Ok(kind) => Ok(Some(kind)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(filesystem_error("inspect", path, error)),
    }
}

fn component_state(
    file_system: &dyn DestinationFileSystem,
    path: &Path,
) -> Result<ComponentState, DestinationError> {
    Ok(match object_kind(file_system, path)? {
        Some(kind) => kind.into(),
        None => ComponentState::Missing,
    })
}

fn is_unsafe_component(state: ComponentState) -> bool {
    matches!(
        state,
        ComponentState::RegularFile | ComponentState::Symlink | ComponentState::Other
    )
}

fn unsafe_component_conflict(path: &Path) -> DestinationConflict {
    DestinationConflict::new(
        path.to_path_buf(),
        ConflictKind::UnsafeDestinationComponent,
        "a destination component is not a real directory; move or resolve it manually before planning an install".to_owned(),
    )
}

fn filesystem_error(action: &'static str, path: &Path, error: io::Error) -> DestinationError {
    DestinationError::Filesystem {
        action,
        path: path.to_path_buf(),
        kind: error.kind(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs, io,
        path::{Path, PathBuf},
    };

    use tempfile::tempdir;

    use crate::agents::AgentId;
    #[cfg(windows)]
    use crate::install::windows_test_support::create_junction;

    use super::{
        ConflictKind, DestinationError, DestinationFileSystem, DirectoryEntrySnapshot,
        MAX_DESTINATION_DIRECTORY_ENTRIES, ObjectKind, RealDestinationFileSystem,
        inspect_destination, inspect_destination_for_agent, inspect_destination_with_file_system,
    };

    #[test]
    fn missing_codex_parent_directories_are_a_read_only_snapshot() {
        let temporary = tempdir().expect("temporary project");
        let project = temporary.path().join("project");
        fs::create_dir(&project).expect("project exists");
        let snapshot = inspect_destination(None, &project, "skill-name").expect("snapshot");
        let resolved = fs::canonicalize(&project).expect("canonical project");

        assert_eq!(snapshot.project_root(), resolved);
        assert_eq!(
            snapshot.destination(),
            resolved.join(".agents/skills/skill-name")
        );
        assert_eq!(snapshot.conflicts().len(), 0);
        assert!(!project.join(".agents").exists());
    }

    #[test]
    fn explicit_project_path_resolves_from_current_directory_and_rejects_parent_segments() {
        let temporary = tempdir().expect("temporary root");
        let current = temporary.path().join("workspace");
        let selected = current.join("project");
        fs::create_dir_all(&selected).expect("project");

        let snapshot = inspect_destination(Some(Path::new("project")), &current, "skill")
            .expect("relative explicit project");
        assert_eq!(
            snapshot.project_root(),
            fs::canonicalize(&selected).expect("canonical")
        );
        assert!(matches!(
            inspect_destination(Some(Path::new("../outside")), &current, "skill"),
            Err(DestinationError::AmbiguousProjectPath)
        ));
    }

    #[test]
    fn missing_file_and_ambiguous_project_roots_are_rejected() {
        let temporary = tempdir().expect("temporary root");
        let missing = temporary.path().join("missing");
        assert!(matches!(
            inspect_destination(Some(&missing), temporary.path(), "skill"),
            Err(DestinationError::MissingProjectDirectory { .. })
        ));
        let file = temporary.path().join("project-file");
        fs::write(&file, b"not a project directory").expect("file");
        assert!(matches!(
            inspect_destination(Some(&file), temporary.path(), "skill"),
            Err(DestinationError::ProjectRootNotDirectory { .. })
        ));
        assert!(matches!(
            inspect_destination(Some(Path::new("")), temporary.path(), "skill"),
            Err(DestinationError::AmbiguousProjectPath)
        ));
    }

    #[test]
    fn manually_installed_skill_and_target_file_are_conflicts() {
        let temporary = tempdir().expect("temporary project");
        let project = temporary.path();
        let target = project.join(".agents/skills/skill-name");
        fs::create_dir_all(&target).expect("manual skill");
        fs::write(target.join("keep.txt"), b"keep").expect("existing content");
        let snapshot = inspect_destination(None, project, "skill-name").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::ExistingDirectory
        );
        assert!(
            snapshot.conflicts()[0]
                .detail()
                .contains("manually installed")
        );
        assert_eq!(
            fs::read(target.join("keep.txt")).expect("preserved"),
            b"keep"
        );

        fs::remove_dir_all(project.join(".agents")).expect("remove test skill");
        fs::create_dir_all(project.join(".agents/skills")).expect("skills");
        fs::write(project.join(".agents/skills/skill-name"), b"blocker").expect("target file");
        let snapshot = inspect_destination(None, project, "skill-name").expect("inspect file");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(snapshot.conflicts()[0].kind(), ConflictKind::ExistingFile);
    }

    #[test]
    fn case_insensitive_skill_name_collisions_fail_even_on_case_sensitive_platforms() {
        let temporary = tempdir().expect("temporary project");
        let skills = temporary.path().join(".agents/skills");
        fs::create_dir_all(&skills).expect("skills");
        fs::create_dir(skills.join("SKILL-NAME")).expect("case collision");
        let snapshot = inspect_destination(None, temporary.path(), "skill-name").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::CaseInsensitiveCollision
        );
    }

    #[test]
    fn case_insensitive_destination_component_collisions_are_detected() {
        let temporary = tempdir().expect("temporary project");
        fs::create_dir(temporary.path().join(".AGENTS")).expect("case collision");
        let snapshot = inspect_destination(None, temporary.path(), "skill").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::CaseInsensitiveCollision
        );

        fs::remove_dir(temporary.path().join(".AGENTS")).expect("remove collision");
        fs::create_dir(temporary.path().join(".agents")).expect("agents");
        fs::create_dir(temporary.path().join(".agents/Skills")).expect("case collision");
        let snapshot = inspect_destination(None, temporary.path(), "skill").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::CaseInsensitiveCollision
        );
    }

    #[test]
    fn file_versus_directory_conflicts_at_destination_components_are_detected() {
        let temporary = tempdir().expect("temporary project");
        fs::write(temporary.path().join(".agents"), b"blocker").expect("agents file");
        let snapshot = inspect_destination(None, temporary.path(), "skill").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );

        fs::remove_file(temporary.path().join(".agents")).expect("remove file");
        fs::create_dir(temporary.path().join(".agents")).expect("agents directory");
        fs::write(temporary.path().join(".agents/skills"), b"blocker").expect("skills file");
        let snapshot = inspect_destination(None, temporary.path(), "skill").expect("inspect");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );
    }

    struct UnsupportedDestinationComponent {
        unsupported_path: PathBuf,
    }

    impl DestinationFileSystem for UnsupportedDestinationComponent {
        fn object_kind(&self, path: &Path) -> io::Result<ObjectKind> {
            if path == self.unsupported_path {
                return Ok(ObjectKind::Other);
            }
            RealDestinationFileSystem.object_kind(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            RealDestinationFileSystem.canonicalize(path)
        }

        fn read_directory(&self, path: &Path) -> io::Result<Vec<DirectoryEntrySnapshot>> {
            RealDestinationFileSystem.read_directory(path)
        }
    }

    #[test]
    fn unsupported_destination_component_is_a_conflict() {
        let temporary = tempdir().expect("temporary project");
        let project = fs::canonicalize(temporary.path()).expect("canonical project");
        fs::create_dir(project.join(".agents")).expect("agents");
        let snapshot = inspect_destination_with_file_system(
            None,
            &project,
            "skill",
            &UnsupportedDestinationComponent {
                unsupported_path: project.join(".agents/skills"),
            },
        )
        .expect("unsafe object is recorded as a conflict");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );
    }

    struct TooManyEntries;

    impl DestinationFileSystem for TooManyEntries {
        fn object_kind(&self, path: &Path) -> io::Result<ObjectKind> {
            RealDestinationFileSystem.object_kind(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            RealDestinationFileSystem.canonicalize(path)
        }

        fn read_directory(&self, _: &Path) -> io::Result<Vec<DirectoryEntrySnapshot>> {
            Ok((0..=MAX_DESTINATION_DIRECTORY_ENTRIES)
                .map(|index| DirectoryEntrySnapshot {
                    name: OsString::from(format!("entry-{index}")),
                    kind: ObjectKind::RegularFile,
                })
                .collect())
        }
    }

    #[test]
    fn destination_directory_entry_inspection_is_bounded() {
        let temporary = tempdir().expect("temporary project");
        let error =
            inspect_destination_with_file_system(None, temporary.path(), "skill", &TooManyEntries)
                .expect_err("excessive entry count must fail closed");
        assert!(matches!(error, DestinationError::TooManyEntries { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_agents_skills_and_skill_destination_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().expect("temporary root");
        let external = temporary.path().join("external");
        fs::create_dir(&external).expect("external");
        let agents_project = temporary.path().join("agents-project");
        fs::create_dir(&agents_project).expect("agents project");
        symlink(&external, agents_project.join(".agents")).expect("agents symlink");
        let snapshot = inspect_destination(None, &agents_project, "skill").expect("inspect");
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );

        let skills_project = temporary.path().join("skills-project");
        fs::create_dir(&skills_project).expect("skills project");
        fs::create_dir(skills_project.join(".agents")).expect("agents");
        symlink(&external, skills_project.join(".agents/skills")).expect("skills symlink");
        let snapshot = inspect_destination(None, &skills_project, "skill").expect("inspect");
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );

        let skill_project = temporary.path().join("skill-project");
        fs::create_dir_all(skill_project.join(".agents/skills")).expect("skills");
        symlink(&external, skill_project.join(".agents/skills/skill")).expect("skill symlink");
        let snapshot = inspect_destination(None, &skill_project, "skill").expect("inspect");
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::ExistingSymlink
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_adapter_rejects_a_symlinked_project_skill_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().expect("temporary root");
        let project = temporary.path().join("project");
        let external = temporary.path().join("external");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&external).expect("external target");
        fs::write(external.join("keep.txt"), b"outside content").expect("external sentinel");
        symlink(&external, project.join(".claude")).expect("Claude root symlink");

        let snapshot =
            inspect_destination_for_agent(AgentId::ClaudeCode, None, &project, "skill-name")
                .expect("unsafe adapter root is a plan conflict");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("external target unchanged"),
            b"outside content"
        );
    }

    #[cfg(windows)]
    #[test]
    fn claude_code_adapter_rejects_a_real_windows_project_junction() {
        let temporary = tempdir().expect("temporary root");
        let project = temporary.path().join("project");
        let external = temporary.path().join("external");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&external).expect("external target");
        fs::write(external.join("keep.txt"), b"outside content").expect("external sentinel");
        create_junction(&project.join(".claude"), &external);

        let snapshot =
            inspect_destination_for_agent(AgentId::ClaudeCode, None, &project, "skill-name")
                .expect("junction is a plan conflict");
        assert_eq!(snapshot.conflicts().len(), 1);
        assert_eq!(
            snapshot.conflicts()[0].kind(),
            ConflictKind::UnsafeDestinationComponent
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("external target unchanged"),
            b"outside content"
        );
    }

    #[cfg(windows)]
    #[test]
    fn cursor_and_gemini_adapters_reject_real_windows_project_junctions() {
        let temporary = tempdir().expect("temporary root");
        let project = temporary.path().join("project");
        let external = temporary.path().join("external");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&external).expect("external target");
        fs::write(external.join("keep.txt"), b"outside content").expect("external sentinel");

        for agent in [AgentId::Cursor, AgentId::Gemini] {
            let adapter_root = project.join(agent.adapter().skill_directory());
            create_junction(&adapter_root, &external);
            let snapshot = inspect_destination_for_agent(agent, None, &project, "skill-name")
                .expect("junction is a plan conflict");
            assert_eq!(snapshot.conflicts().len(), 1, "agent={agent:?}");
            assert_eq!(
                snapshot.conflicts()[0].kind(),
                ConflictKind::UnsafeDestinationComponent,
                "agent={agent:?}"
            );
            assert_eq!(
                fs::read(external.join("keep.txt")).expect("external target unchanged"),
                b"outside content"
            );
            fs::remove_dir(&adapter_root).expect("remove test junction before next adapter");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_project_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().expect("temporary root");
        let real = temporary.path().join("real-project");
        let linked = temporary.path().join("linked-project");
        fs::create_dir(&real).expect("real project");
        symlink(&real, &linked).expect("project symlink");
        assert!(matches!(
            inspect_destination(Some(&linked), temporary.path(), "skill"),
            Err(DestinationError::ProjectRootSymlink { .. })
        ));
    }

    struct InspectionFailure {
        denied_path: PathBuf,
    }

    impl DestinationFileSystem for InspectionFailure {
        fn object_kind(&self, path: &Path) -> io::Result<ObjectKind> {
            if path == self.denied_path {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"));
            }
            RealDestinationFileSystem.object_kind(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            RealDestinationFileSystem.canonicalize(path)
        }

        fn read_directory(&self, path: &Path) -> io::Result<Vec<DirectoryEntrySnapshot>> {
            RealDestinationFileSystem.read_directory(path)
        }
    }

    #[test]
    fn injected_destination_inspection_failures_are_reported() {
        let temporary = tempdir().expect("temporary project");
        let project = temporary.path();
        fs::create_dir(project.join(".agents")).expect("agents");
        let project = fs::canonicalize(project).expect("canonical project");
        let error = inspect_destination_with_file_system(
            None,
            &project,
            "skill",
            &InspectionFailure {
                denied_path: project.join(".agents/skills"),
            },
        )
        .expect_err("filesystem inspection error must not be ignored");
        assert!(matches!(
            error,
            DestinationError::Filesystem {
                action: "inspect",
                kind: io::ErrorKind::PermissionDenied,
                ..
            }
        ));

        let root_error = inspect_destination_with_file_system(
            None,
            &project,
            "skill",
            &InspectionFailure {
                denied_path: project.clone(),
            },
        )
        .expect_err("inaccessible project root must fail");
        assert!(matches!(
            root_error,
            DestinationError::Filesystem {
                action: "inspect",
                kind: io::ErrorKind::PermissionDenied,
                ..
            }
        ));
    }
}
