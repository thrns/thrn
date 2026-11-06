//! Internal M2-04 commit engine. The CLI deliberately does not call this module.
#![allow(dead_code)] // Kept internal until M2-05 receipts and M2-06 recovery authorize public installs.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use ring::{
    digest::{Context, SHA256},
    rand::SecureRandom,
};
use thiserror::Error;

use crate::sources::staging::StagedSkill;

use super::{
    destination::{
        ComponentState, DestinationSnapshot, FileIdentity, ObjectKind, inspect_destination,
    },
    inventory::{StagedInventory, inspect_staged_skill},
    plan::{InstallAgent, InstallPlan, InstallScope},
};

const IO_CHUNK_BYTES: usize = 8 * 1024;
const TEMP_NAME_ATTEMPTS: usize = 32;

/// Owns the validated source for as long as a plan can be committed.
pub(crate) struct InstallationSession {
    staged: StagedSkill,
    inventory: StagedInventory,
    plan: InstallPlan,
}

impl InstallationSession {
    pub(crate) fn new(staged: StagedSkill, inventory: StagedInventory, plan: InstallPlan) -> Self {
        Self {
            staged,
            inventory,
            plan,
        }
    }

    pub(crate) fn plan(&self) -> &InstallPlan {
        &self.plan
    }

    pub(crate) fn into_plan(self) -> InstallPlan {
        self.plan
    }

    pub(crate) fn commit(&self) -> Result<(), CommitError> {
        commit_session(self, &NoFaults)
    }
}

/// Filesystem boundaries that tests can fail at deterministic operation points.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum InstallOperation {
    CreateParentDirectory,
    CreateTemporaryDirectory,
    CreateDirectory,
    CreateFile,
    ReadSource,
    WriteDestination,
    CommitNoReplace,
    RemoveFile,
    RemoveDirectory,
}

pub(crate) trait InstallFaultInjector: Send + Sync {
    fn before(&self, _operation: InstallOperation) -> io::Result<()> {
        Ok(())
    }
}

struct NoFaults;
impl InstallFaultInjector for NoFaults {}

#[derive(Debug, Error)]
pub(crate) enum CommitError {
    #[error("install session identities do not match")]
    IdentityMismatch,
    #[error("the validated staged source changed after planning")]
    SourceChanged,
    #[error("the selected project or Codex destination changed after planning")]
    DestinationChanged,
    #[error("the Codex destination has an existing or case-insensitive collision")]
    DestinationConflict,
    #[error("this platform does not provide the required safe no-replace directory commit")]
    UnsupportedPlatform,
    #[error(
        "installation failed during {operation:?} ({kind}, os_error={raw_os_error:?}); cleanup_complete={cleanup_complete}"
    )]
    Operation {
        operation: InstallOperation,
        kind: io::ErrorKind,
        raw_os_error: Option<i32>,
        cleanup_complete: bool,
        cleanup_errors: Vec<(InstallOperation, io::ErrorKind)>,
    },
}

fn commit_session(
    session: &InstallationSession,
    faults: &dyn InstallFaultInjector,
) -> Result<(), CommitError> {
    validate_session_identity(session)?;
    if !session.plan.conflicts().is_empty() {
        return Err(CommitError::DestinationConflict);
    }

    let current_inventory =
        inspect_staged_skill(&session.staged).map_err(|_| CommitError::SourceChanged)?;
    if current_inventory != session.inventory {
        return Err(CommitError::SourceChanged);
    }

    let snapshot = session.plan.destination_snapshot();
    let current = inspect_destination(
        Some(session.plan.project_root()),
        session.plan.project_root(),
        session.plan.skill_name().as_str(),
    )
    .map_err(|_| CommitError::DestinationChanged)?;
    if &current != snapshot {
        return Err(if current.conflicts().is_empty() {
            CommitError::DestinationChanged
        } else {
            CommitError::DestinationConflict
        });
    }

    let project = SecureDir::open_absolute(session.plan.project_root()).map_err(|error| {
        operation_error(InstallOperation::CreateParentDirectory, error, true, vec![])
    })?;
    ensure_identity(&project, snapshot.project_identity())?;

    let (agents, created_agents) = open_or_create_parent(
        &project,
        ".agents",
        snapshot.agents_state(),
        InstallOperation::CreateParentDirectory,
        faults,
    )?;
    if !created_agents && agents.identity().ok() != snapshot.agents_identity() {
        return Err(CommitError::DestinationChanged);
    }
    let (skills, created_skills) = match open_or_create_parent(
        &agents,
        "skills",
        snapshot.skills_state(),
        InstallOperation::CreateParentDirectory,
        faults,
    ) {
        Ok(result) => result,
        Err(error) => {
            let mut cleanup_errors = Vec::new();
            if created_agents {
                remove_empty_child(
                    &project,
                    ".agents",
                    InstallOperation::RemoveDirectory,
                    faults,
                    &mut cleanup_errors,
                );
            }
            return Err(with_cleanup(error, cleanup_errors));
        }
    };
    if !created_skills && skills.identity().ok() != snapshot.skills_identity() {
        let mut created_parents = CreatedParents {
            project: &project,
            agents: &agents,
            created_agents,
            created_skills,
        };
        let cleanup = cleanup_parents(&mut created_parents, faults);
        return Err(operation_error(
            InstallOperation::CreateParentDirectory,
            io::Error::other("skills directory identity changed"),
            cleanup.is_empty(),
            cleanup,
        ));
    }

    let mut created_parents = CreatedParents {
        project: &project,
        agents: &agents,
        created_agents,
        created_skills,
    };

    verify_parent_names(&project, ".agents")
        .and_then(|()| verify_parent_names(&agents, "skills"))
        .map_err(|error| {
            let cleanup = cleanup_parents(&mut created_parents, faults);
            operation_error(
                InstallOperation::CreateParentDirectory,
                error,
                cleanup.is_empty(),
                cleanup,
            )
        })?;

    let root_identity = project.identity().map_err(|error| {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        operation_error(
            InstallOperation::CreateParentDirectory,
            error,
            cleanup.is_empty(),
            cleanup,
        )
    })?;
    let agents_identity = agents.identity().map_err(|error| {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        operation_error(
            InstallOperation::CreateParentDirectory,
            error,
            cleanup.is_empty(),
            cleanup,
        )
    })?;
    let skills_identity = skills.identity().map_err(|error| {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        operation_error(
            InstallOperation::CreateParentDirectory,
            error,
            cleanup.is_empty(),
            cleanup,
        )
    })?;

    let latest = inspect_destination(
        Some(session.plan.project_root()),
        session.plan.project_root(),
        session.plan.skill_name().as_str(),
    )
    .map_err(|_| {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        operation_error(
            InstallOperation::CreateParentDirectory,
            io::Error::other("destination revalidation failed"),
            cleanup.is_empty(),
            cleanup,
        )
    })?;
    if !snapshot_matches_after_parent_creation(
        snapshot,
        &latest,
        created_agents,
        created_skills,
        None,
    ) || latest.project_identity() != Some(root_identity)
        || latest.agents_identity() != Some(agents_identity)
        || latest.skills_identity() != Some(skills_identity)
    {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        return Err(operation_error(
            InstallOperation::CreateParentDirectory,
            io::Error::other("destination assumptions changed during parent creation"),
            cleanup.is_empty(),
            cleanup,
        ));
    }
    if latest.skill_state() != ComponentState::Missing || !latest.conflicts().is_empty() {
        let cleanup = cleanup_parents(&mut created_parents, faults);
        return Err(operation_error(
            InstallOperation::CommitNoReplace,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "skill destination is occupied",
            ),
            cleanup.is_empty(),
            cleanup,
        ));
    }

    let mut temp = match create_temp_install(&skills, faults) {
        Ok(temp) => temp,
        Err(error) => {
            let cleanup = cleanup_parents(&mut created_parents, faults);
            return Err(with_cleanup(error, cleanup));
        }
    };

    let copy_result = copy_inventory(session, &skills, &mut temp, faults)
        .and_then(|()| verify_prepared_tree(session, &temp));
    if let Err(error) = copy_result {
        let mut cleanup_errors = temp.cleanup(faults);
        cleanup_errors.extend(cleanup_parents(&mut created_parents, faults));
        return Err(with_cleanup(error, cleanup_errors));
    }

    let latest = inspect_destination(
        Some(session.plan.project_root()),
        session.plan.project_root(),
        session.plan.skill_name().as_str(),
    )
    .map_err(|_| {
        let mut cleanup_errors = temp.cleanup(faults);
        cleanup_errors.extend(cleanup_parents(&mut created_parents, faults));
        operation_error(
            InstallOperation::CommitNoReplace,
            io::Error::other("destination revalidation failed"),
            cleanup_errors.is_empty(),
            cleanup_errors,
        )
    })?;
    if !snapshot_matches_after_parent_creation(
        snapshot,
        &latest,
        created_agents,
        created_skills,
        Some(&temp.name),
    ) || latest.project_identity() != Some(root_identity)
        || latest.agents_identity() != Some(agents_identity)
        || latest.skills_identity() != Some(skills_identity)
        || latest.skill_state() != ComponentState::Missing
        || !latest.conflicts().is_empty()
    {
        let mut cleanup_errors = temp.cleanup(faults);
        cleanup_errors.extend(cleanup_parents(&mut created_parents, faults));
        return Err(operation_error(
            InstallOperation::CommitNoReplace,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination changed or became occupied",
            ),
            cleanup_errors.is_empty(),
            cleanup_errors,
        ));
    }

    if let Err(error) = faults
        .before(InstallOperation::CommitNoReplace)
        .and_then(|()| {
            let named_stage = skills.open_directory(std::ffi::OsStr::new(&temp.name))?;
            if named_stage.identity()? != temp.root.identity()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private staging directory was substituted",
                ));
            }
            skills.rename_child_noreplace(
                &temp.name,
                session.plan.skill_name().as_str(),
                &temp.root,
            )
        })
    {
        let mut cleanup_errors = temp.cleanup(faults);
        cleanup_errors.extend(cleanup_parents(&mut created_parents, faults));
        return Err(operation_error(
            InstallOperation::CommitNoReplace,
            error,
            cleanup_errors.is_empty(),
            cleanup_errors,
        ));
    }
    temp.disarm();
    Ok(())
}

fn validate_session_identity(session: &InstallationSession) -> Result<(), CommitError> {
    let staged = &session.staged;
    let inventory = &session.inventory;
    let plan = &session.plan;
    let expected_destination = plan
        .project_root()
        .join(".agents/skills")
        .join(plan.skill_name().as_str());
    if staged.catalog_id() != plan.resource_id()
        || staged.skill_name() != plan.skill_name()
        || staged.revision() != plan.source_commit()
        || staged.source().owner() != plan.source_owner()
        || staged.source().repository() != plan.source_repository()
        || staged.source().commit() != plan.source_commit()
        || staged.source().path() != plan.source_directory()
        || staged.content_directory() != inventory.content_directory()
        || inventory.catalog_id() != plan.resource_id()
        || inventory.skill_name() != plan.skill_name()
        || inventory.revision() != plan.source_commit()
        || plan.agent() != InstallAgent::Codex
        || plan.scope() != InstallScope::Project
        || plan.destination() != expected_destination
        || !plan.destination().starts_with(plan.project_root())
        || inventory.total_bytes() != plan.total_file_bytes()
        || inventory.files().len() != plan.files().len()
    {
        return Err(CommitError::IdentityMismatch);
    }
    let planned_directories = plan
        .directories()
        .iter()
        .filter_map(|directory| directory.path().strip_prefix(plan.destination()).ok())
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>();
    let mut expected_directories = inventory
        .directories()
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if plan.destination_snapshot().skill_state() == ComponentState::Missing {
        expected_directories.insert(PathBuf::new());
    }
    if planned_directories != expected_directories {
        return Err(CommitError::IdentityMismatch);
    }
    for (inventory_file, plan_file) in inventory.files().iter().zip(plan.files()) {
        if inventory_file.relative_path() != plan_file.relative_path()
            || plan_file.destination() != plan.destination().join(inventory_file.relative_path())
            || inventory_file.expected_bytes() != plan_file.expected_bytes()
            || inventory_file.expected_crc32() != plan_file.expected_crc32()
            || inventory_file.expected_sha256() != plan_file.expected_sha256()
        {
            return Err(CommitError::IdentityMismatch);
        }
    }
    Ok(())
}

fn operation_error(
    operation: InstallOperation,
    error: io::Error,
    cleanup_complete: bool,
    cleanup_errors: Vec<(InstallOperation, io::ErrorKind)>,
) -> CommitError {
    CommitError::Operation {
        operation,
        kind: error.kind(),
        raw_os_error: error.raw_os_error(),
        cleanup_complete,
        cleanup_errors,
    }
}

fn with_cleanup(
    mut error: CommitError,
    cleanup_errors: Vec<(InstallOperation, io::ErrorKind)>,
) -> CommitError {
    if let CommitError::Operation {
        cleanup_complete,
        cleanup_errors: current,
        ..
    } = &mut error
    {
        current.extend(cleanup_errors);
        *cleanup_complete = current.is_empty();
    }
    error
}

fn ensure_identity(dir: &SecureDir, expected: Option<FileIdentity>) -> Result<(), CommitError> {
    match (dir.identity(), expected) {
        (Ok(actual), Some(expected)) if actual == expected => Ok(()),
        _ => Err(CommitError::DestinationChanged),
    }
}

fn open_or_create_parent(
    parent: &SecureDir,
    name: &str,
    expected_state: ComponentState,
    operation: InstallOperation,
    faults: &dyn InstallFaultInjector,
) -> Result<(SecureDir, bool), CommitError> {
    match expected_state {
        ComponentState::Directory => parent
            .open_directory(std::ffi::OsStr::new(name))
            .map(|directory| (directory, false))
            .map_err(|error| operation_error(operation, error, true, vec![])),
        ComponentState::Missing => {
            faults
                .before(operation)
                .map_err(|error| operation_error(operation, error, true, vec![]))?;
            match parent.create_directory(name) {
                Ok(()) => match parent.open_directory(std::ffi::OsStr::new(name)) {
                    Ok(directory) => Ok((directory, true)),
                    Err(error) => {
                        let mut cleanup_errors = Vec::new();
                        remove_empty_child(
                            parent,
                            name,
                            InstallOperation::RemoveDirectory,
                            faults,
                            &mut cleanup_errors,
                        );
                        Err(operation_error(
                            operation,
                            error,
                            cleanup_errors.is_empty(),
                            cleanup_errors,
                        ))
                    }
                },
                Err(error) => Err(operation_error(operation, error, true, vec![])),
            }
        }
        _ => Err(CommitError::DestinationConflict),
    }
}

fn verify_parent_names(parent: &SecureDir, expected: &str) -> io::Result<()> {
    if parent.entries()?.iter().any(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected) && name != expected)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "case-insensitive parent collision",
        ));
    }
    Ok(())
}

fn snapshot_matches_after_parent_creation(
    planned: &DestinationSnapshot,
    current: &DestinationSnapshot,
    created_agents: bool,
    created_skills: bool,
    temp_name: Option<&str>,
) -> bool {
    if planned.project_root() != current.project_root()
        || planned.project_identity() != current.project_identity()
        || !current.conflicts().is_empty()
        || current.skill_state() != ComponentState::Missing
    {
        return false;
    }
    let mut expected_project_entries = planned.existing_project_entries().to_vec();
    if created_agents {
        expected_project_entries.push(super::destination::DirectoryEntrySnapshot::new(
            ".agents".into(),
            ObjectKind::Directory,
        ));
        expected_project_entries.sort_by(|a, b| a.name().cmp(b.name()));
    }
    if expected_project_entries != current.existing_project_entries() {
        return false;
    }
    let expected_agents_state = if created_agents {
        ComponentState::Directory
    } else {
        planned.agents_state()
    };
    let expected_agents_identity = if created_agents {
        current.agents_identity()
    } else {
        planned.agents_identity()
    };
    if current.agents_state() != expected_agents_state
        || current.agents_identity() != expected_agents_identity
    {
        return false;
    }
    let mut expected_agents_entries = planned.existing_agents_entries().to_vec();
    if created_skills {
        expected_agents_entries.push(super::destination::DirectoryEntrySnapshot::new(
            "skills".into(),
            ObjectKind::Directory,
        ));
        expected_agents_entries.sort_by(|a, b| a.name().cmp(b.name()));
    }
    if expected_agents_entries != current.existing_agents_entries() {
        return false;
    }
    let expected_skills_state = if created_skills {
        ComponentState::Directory
    } else {
        planned.skills_state()
    };
    let expected_skills_identity = if created_skills {
        current.skills_identity()
    } else {
        planned.skills_identity()
    };
    let mut expected_skill_entries = if created_skills {
        Vec::new()
    } else {
        planned.existing_skill_entries().to_vec()
    };
    if let Some(temp_name) = temp_name {
        expected_skill_entries.push(super::destination::DirectoryEntrySnapshot::new(
            temp_name.into(),
            ObjectKind::Directory,
        ));
        expected_skill_entries.sort_by(|a, b| a.name().cmp(b.name()));
    }
    current.skills_state() == expected_skills_state
        && current.skills_identity() == expected_skills_identity
        && current.existing_skill_entries() == expected_skill_entries
}

struct CreatedParents<'a> {
    project: &'a SecureDir,
    agents: &'a SecureDir,
    created_agents: bool,
    created_skills: bool,
}

fn cleanup_parents(
    created: &mut CreatedParents<'_>,
    faults: &dyn InstallFaultInjector,
) -> Vec<(InstallOperation, io::ErrorKind)> {
    let mut errors = Vec::new();
    if created.created_skills {
        remove_empty_child(
            created.agents,
            "skills",
            InstallOperation::RemoveDirectory,
            faults,
            &mut errors,
        );
    }
    if created.created_agents {
        remove_empty_child(
            created.project,
            ".agents",
            InstallOperation::RemoveDirectory,
            faults,
            &mut errors,
        );
    }
    errors
}

fn remove_empty_child(
    parent: &SecureDir,
    name: &str,
    operation: InstallOperation,
    faults: &dyn InstallFaultInjector,
    errors: &mut Vec<(InstallOperation, io::ErrorKind)>,
) {
    if let Err(error) = faults
        .before(operation)
        .and_then(|()| parent.remove_directory(name))
    {
        errors.push((operation, error.kind()));
    }
}

struct TempInstall {
    parent: SecureDir,
    name: String,
    root: SecureDir,
    created_files: Vec<PathBuf>,
    created_directories: Vec<PathBuf>,
    armed: bool,
}

impl TempInstall {
    fn cleanup(
        &mut self,
        faults: &dyn InstallFaultInjector,
    ) -> Vec<(InstallOperation, io::ErrorKind)> {
        if !self.armed {
            return Vec::new();
        }
        let mut errors = Vec::new();
        for relative in self.created_files.iter().rev() {
            match self.relative_parent(relative) {
                Ok((parent, name)) => {
                    if let Err(error) = faults
                        .before(InstallOperation::RemoveFile)
                        .and_then(|()| parent.remove_file(&name))
                    {
                        errors.push((InstallOperation::RemoveFile, error.kind()));
                    }
                }
                Err(error) => errors.push((InstallOperation::RemoveFile, error.kind())),
            }
        }
        for relative in self.created_directories.iter().rev() {
            match self.relative_parent(relative) {
                Ok((parent, name)) => {
                    if let Err(error) = faults
                        .before(InstallOperation::RemoveDirectory)
                        .and_then(|()| parent.remove_directory(&name))
                    {
                        errors.push((InstallOperation::RemoveDirectory, error.kind()));
                    }
                }
                Err(error) => errors.push((InstallOperation::RemoveDirectory, error.kind())),
            }
        }
        if let Err(error) = faults
            .before(InstallOperation::RemoveDirectory)
            .and_then(|()| self.parent.remove_directory(&self.name))
        {
            errors.push((InstallOperation::RemoveDirectory, error.kind()));
        } else {
            self.armed = false;
        }
        errors
    }

    fn relative_parent(&self, relative: &Path) -> io::Result<(SecureDir, String)> {
        let name = relative
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid cleanup path"))?
            .to_owned();
        let mut parent = self.root.clone();
        if let Some(parent_path) = relative.parent() {
            for component in parent_path.components() {
                let Component::Normal(component) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid cleanup path",
                    ));
                };
                parent = parent.open_directory(component)?;
            }
        }
        Ok((parent, name))
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

fn create_temp_install(
    parent: &SecureDir,
    faults: &dyn InstallFaultInjector,
) -> Result<TempInstall, CommitError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = random_temp_name().map_err(|error| {
            operation_error(
                InstallOperation::CreateTemporaryDirectory,
                error,
                true,
                vec![],
            )
        })?;
        faults
            .before(InstallOperation::CreateTemporaryDirectory)
            .map_err(|error| {
                operation_error(
                    InstallOperation::CreateTemporaryDirectory,
                    error,
                    true,
                    vec![],
                )
            })?;
        match parent.create_directory(&name) {
            Ok(()) => {
                let root = match parent.open_directory(std::ffi::OsStr::new(&name)) {
                    Ok(root) => root,
                    Err(error) => {
                        let mut cleanup_errors = Vec::new();
                        remove_empty_child(
                            parent,
                            &name,
                            InstallOperation::RemoveDirectory,
                            faults,
                            &mut cleanup_errors,
                        );
                        return Err(operation_error(
                            InstallOperation::CreateTemporaryDirectory,
                            error,
                            cleanup_errors.is_empty(),
                            cleanup_errors,
                        ));
                    }
                };
                return Ok(TempInstall {
                    parent: parent.clone(),
                    name,
                    root,
                    created_files: Vec::new(),
                    created_directories: Vec::new(),
                    armed: true,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(operation_error(
                    InstallOperation::CreateTemporaryDirectory,
                    error,
                    true,
                    vec![],
                ));
            }
        }
    }
    Err(operation_error(
        InstallOperation::CreateTemporaryDirectory,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique staging directory",
        ),
        true,
        vec![],
    ))
}

fn random_temp_name() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("operating system random source unavailable"))?;
    let mut name = String::from(".thrn-install-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(name)
}

fn copy_inventory(
    session: &InstallationSession,
    _skills: &SecureDir,
    temp: &mut TempInstall,
    faults: &dyn InstallFaultInjector,
) -> Result<(), CommitError> {
    for directory in session.inventory.directories() {
        let (parent, name) = temp_parent_for(directory, &temp.root).map_err(|error| {
            operation_error(InstallOperation::CreateDirectory, error, true, vec![])
        })?;
        faults
            .before(InstallOperation::CreateDirectory)
            .map_err(|error| {
                operation_error(InstallOperation::CreateDirectory, error, true, vec![])
            })?;
        parent.create_directory(name).map_err(|error| {
            operation_error(InstallOperation::CreateDirectory, error, true, vec![])
        })?;
        temp.created_directories.push(directory.clone());
    }

    let source_root = fs::canonicalize(session.staged.content_directory())
        .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
    let source = SecureDir::open_absolute(&source_root)
        .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
    for file in session.inventory.files() {
        let (source_parent, source_name) = source_parent_for(file.relative_path(), &source)
            .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
        faults
            .before(InstallOperation::ReadSource)
            .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
        let mut input = source_parent
            .open_regular_file(source_name)
            .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
        let (destination_parent, destination_name) =
            temp_parent_for(file.relative_path(), &temp.root).map_err(|error| {
                operation_error(InstallOperation::CreateFile, error, true, vec![])
            })?;
        faults
            .before(InstallOperation::CreateFile)
            .map_err(|error| operation_error(InstallOperation::CreateFile, error, true, vec![]))?;
        let mut output = destination_parent
            .create_regular_file(destination_name)
            .map_err(|error| operation_error(InstallOperation::CreateFile, error, true, vec![]))?;
        temp.created_files.push(file.relative_path().to_path_buf());

        let mut sha = Context::new(&SHA256);
        let mut crc = crc32fast::Hasher::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; IO_CHUNK_BYTES];
        loop {
            let read = input.read(&mut buffer).map_err(|error| {
                operation_error(InstallOperation::ReadSource, error, true, vec![])
            })?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .filter(|bytes| *bytes <= file.expected_bytes())
                .ok_or_else(|| {
                    operation_error(
                        InstallOperation::ReadSource,
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "staged file exceeded its validated size",
                        ),
                        true,
                        vec![],
                    )
                })?;
            sha.update(&buffer[..read]);
            crc.update(&buffer[..read]);
            faults
                .before(InstallOperation::WriteDestination)
                .map_err(|error| {
                    operation_error(InstallOperation::WriteDestination, error, true, vec![])
                })?;
            output.write_all(&buffer[..read]).map_err(|error| {
                operation_error(InstallOperation::WriteDestination, error, true, vec![])
            })?;
        }
        if total != file.expected_bytes() || crc.finalize() != file.expected_crc32() {
            return Err(operation_error(
                InstallOperation::ReadSource,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "staged file no longer matches the validated inventory",
                ),
                true,
                vec![],
            ));
        }
        let source_digest = sha.finish();
        if source_digest.as_ref() != file.expected_sha256() {
            return Err(operation_error(
                InstallOperation::ReadSource,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "staged file SHA-256 changed after planning",
                ),
                true,
                vec![],
            ));
        }
        output.flush().map_err(|error| {
            operation_error(InstallOperation::WriteDestination, error, true, vec![])
        })?;
        drop(output);

        faults
            .before(InstallOperation::ReadSource)
            .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
        let mut copied = destination_parent
            .open_regular_file(destination_name)
            .map_err(|error| operation_error(InstallOperation::ReadSource, error, true, vec![]))?;
        let mut copied_sha = Context::new(&SHA256);
        let mut copied_bytes = 0_u64;
        loop {
            let read = copied.read(&mut buffer).map_err(|error| {
                operation_error(InstallOperation::ReadSource, error, true, vec![])
            })?;
            if read == 0 {
                break;
            }
            copied_bytes = copied_bytes
                .checked_add(read as u64)
                .filter(|bytes| *bytes <= file.expected_bytes())
                .ok_or_else(|| {
                    operation_error(
                        InstallOperation::ReadSource,
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "copied file exceeded its validated size",
                        ),
                        true,
                        vec![],
                    )
                })?;
            copied_sha.update(&buffer[..read]);
        }
        if copied_bytes != file.expected_bytes()
            || source_digest.as_ref() != file.expected_sha256()
            || copied_sha.finish().as_ref() != file.expected_sha256()
        {
            return Err(operation_error(
                InstallOperation::ReadSource,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "destination copy failed SHA-256 verification",
                ),
                true,
                vec![],
            ));
        }
    }
    Ok(())
}

fn digest_file(reader: &mut impl Read, expected: u64) -> io::Result<[u8; 32]> {
    let mut context = Context::new(&SHA256);
    let mut total = 0_u64;
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|bytes| *bytes <= expected)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "file exceeded validated size")
            })?;
        context.update(&buffer[..read]);
    }
    if total != expected {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file size changed",
        ));
    }
    let digest = context.finish();
    let mut bytes = [0; 32];
    bytes.copy_from_slice(digest.as_ref());
    Ok(bytes)
}

fn temp_parent_for<'a>(relative: &'a Path, root: &SecureDir) -> io::Result<(SecureDir, &'a str)> {
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid inventory path"))?;
    let mut parent = root.clone();
    if let Some(ancestor) = relative.parent() {
        for component in ancestor.components() {
            let Component::Normal(component) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid inventory path",
                ));
            };
            parent = parent.open_directory(component)?;
        }
    }
    Ok((parent, name))
}

fn source_parent_for<'a>(relative: &'a Path, root: &SecureDir) -> io::Result<(SecureDir, &'a str)> {
    temp_parent_for(relative, root)
}

fn verify_prepared_tree(
    session: &InstallationSession,
    temp: &TempInstall,
) -> Result<(), CommitError> {
    let mut expected = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    expected.insert(PathBuf::new(), BTreeSet::new());
    for directory in session.inventory.directories() {
        let parent = directory
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        let name = directory
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(CommitError::IdentityMismatch)?
            .to_owned();
        expected.entry(parent).or_default().insert(name);
        expected.entry(directory.clone()).or_default();
    }
    for file in session.inventory.files() {
        let parent = file
            .relative_path()
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        let name = file
            .relative_path()
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(CommitError::IdentityMismatch)?
            .to_owned();
        expected.entry(parent).or_default().insert(name);
    }
    for (relative, names) in expected {
        let directory = if relative.as_os_str().is_empty() {
            temp.root.clone()
        } else {
            temp_parent_for(&relative.join("x"), &temp.root)
                .map_err(|error| {
                    operation_error(InstallOperation::CreateDirectory, error, true, vec![])
                })?
                .0
        };
        let actual = directory.entries().map_err(|error| {
            operation_error(InstallOperation::CreateDirectory, error, true, vec![])
        })?;
        let mut actual_names = BTreeSet::new();
        for (name, kind) in actual {
            let name = name
                .to_str()
                .ok_or(CommitError::IdentityMismatch)?
                .to_owned();
            if !names.contains(&name) {
                return Err(operation_error(
                    InstallOperation::CreateDirectory,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "prepared directory contains an unexpected entry",
                    ),
                    true,
                    vec![],
                ));
            }
            let is_directory = session
                .inventory
                .directories()
                .iter()
                .any(|path| path == &relative.join(&name));
            let valid = if is_directory {
                kind == NodeKind::Directory
            } else {
                kind == NodeKind::RegularFile
            };
            if !valid {
                return Err(operation_error(
                    InstallOperation::CreateDirectory,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "prepared entry has an unsupported object type",
                    ),
                    true,
                    vec![],
                ));
            }
            actual_names.insert(name);
        }
        if actual_names != names {
            return Err(operation_error(
                InstallOperation::CreateDirectory,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prepared directory is missing an expected entry",
                ),
                true,
                vec![],
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone)]
struct SecureDir {
    file: std::sync::Arc<fs::File>,
    path: PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SecureDir {
    fn open_absolute(path: &Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags, open};
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not absolute",
            ));
        }
        let fd = open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rustix_error)?;
        let mut current = Self {
            file: std::sync::Arc::new(fd.into()),
            path: PathBuf::from("/"),
        };
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => current = current.open_directory(name)?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "ambiguous absolute path",
                    ));
                }
            }
        }
        Ok(current)
    }

    fn open_directory(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        self.ensure_current_path()?;
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rustix_error)?;
        Ok(Self {
            file: std::sync::Arc::new(fd.into()),
            path: self.path.join(name),
        })
    }

    fn create_directory(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        use rustix::fs::{Mode, mkdirat};
        mkdirat(&self.file, name, Mode::RWXU).map_err(rustix_error)
    }

    fn create_regular_file(&self, name: &str) -> io::Result<fs::File> {
        self.ensure_current_path()?;
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            &self.file,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(rustix_error)?;
        let file: fs::File = fd.into();
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "created object is not a regular file",
            ));
        }
        Ok(file)
    }

    fn open_regular_file(&self, name: &str) -> io::Result<fs::File> {
        self.ensure_current_path()?;
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rustix_error)?;
        let file: fs::File = fd.into();
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened object is not a regular file",
            ));
        }
        Ok(file)
    }

    fn entries(&self) -> io::Result<Vec<(std::ffi::OsString, NodeKind)>> {
        self.ensure_current_path()?;
        use rustix::fs::{AtFlags, Dir, FileType, statat};
        use std::os::unix::ffi::OsStringExt;
        let mut stream = Dir::read_from(&self.file).map_err(rustix_error)?;
        let mut result = Vec::new();
        for entry in &mut stream {
            let entry = entry.map_err(rustix_error)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if result.len() >= super::destination::MAX_DESTINATION_DIRECTORY_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry limit exceeded",
                ));
            }
            let file_type = statat(&self.file, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                .map(|metadata| FileType::from_raw_mode(metadata.st_mode))
                .map_err(rustix_error)?;
            let kind = match file_type {
                FileType::Directory => NodeKind::Directory,
                FileType::RegularFile => NodeKind::RegularFile,
                FileType::Symlink => NodeKind::Symlink,
                _ => NodeKind::Other,
            };
            result.push((std::ffi::OsString::from_vec(bytes.to_vec()), kind));
        }
        Ok(result)
    }

    fn identity(&self) -> io::Result<FileIdentity> {
        use rustix::fs::fstat;
        let stat = fstat(&self.file).map_err(rustix_error)?;
        Ok(FileIdentity::new(stat.st_dev as u64, stat.st_ino as u64))
    }

    fn remove_file(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        use rustix::fs::{AtFlags, unlinkat};
        unlinkat(&self.file, name, AtFlags::empty()).map_err(rustix_error)
    }

    fn remove_directory(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        use rustix::fs::{AtFlags, unlinkat};
        unlinkat(&self.file, name, AtFlags::REMOVEDIR).map_err(rustix_error)
    }

    fn rename_child_noreplace(
        &self,
        old_name: &str,
        new_name: &str,
        source: &SecureDir,
    ) -> io::Result<()> {
        use rustix::fs::{RenameFlags, renameat_with};
        self.ensure_current_path()?;
        source.ensure_current_path()?;
        renameat_with(
            &self.file,
            old_name,
            &self.file,
            new_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(rustix_error)
    }

    fn ensure_current_path(&self) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory path was substituted",
            ));
        }
        let path_identity = FileIdentity::new(metadata.dev(), metadata.ino());
        if path_identity != self.identity()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory path identity changed",
            ));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rustix_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(windows)]
#[derive(Clone)]
struct SecureDir {
    file: std::sync::Arc<fs::File>,
    path: PathBuf,
}

#[cfg(windows)]
impl SecureDir {
    fn open_absolute(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not absolute",
            ));
        }
        let current = path.to_path_buf();
        let root = current
            .ancestors()
            .last()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing path root"))?;
        let mut checked = Vec::new();
        let mut ancestor = path;
        while let Some(parent) = ancestor.parent() {
            checked.push(ancestor.to_path_buf());
            ancestor = parent;
        }
        checked.push(root.to_path_buf());
        for component in checked.iter().rev() {
            let metadata = fs::symlink_metadata(component)?;
            if windows_reparse(&metadata) || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path contains a reparse point or non-directory",
                ));
            }
        }
        let file = open_windows_directory(&current)?;
        Ok(Self {
            file: std::sync::Arc::new(file),
            path: current,
        })
    }

    fn open_directory(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        self.ensure_current_path()?;
        let path = self.path.join(name);
        let file = open_windows_directory(&path)?;
        Ok(Self {
            file: std::sync::Arc::new(file),
            path,
        })
    }

    fn create_directory(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        fs::create_dir(self.path.join(name))?;
        Ok(())
    }

    fn create_regular_file(&self, name: &str) -> io::Result<fs::File> {
        self.ensure_current_path()?;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(self.path.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "created object is not a regular file",
            ));
        }
        Ok(file)
    }

    fn open_regular_file(&self, name: &str) -> io::Result<fs::File> {
        self.ensure_current_path()?;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(self.path.join(name))?;
        if !file.metadata()?.is_file() || windows_reparse(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened object is not a regular file",
            ));
        }
        Ok(file)
    }

    fn entries(&self) -> io::Result<Vec<(std::ffi::OsString, NodeKind)>> {
        self.ensure_current_path()?;
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            let kind = if windows_reparse(&metadata) {
                NodeKind::Symlink
            } else if metadata.is_dir() {
                NodeKind::Directory
            } else if metadata.is_file() {
                NodeKind::RegularFile
            } else {
                NodeKind::Other
            };
            result.push((entry.file_name(), kind));
            if result.len() > super::destination::MAX_DESTINATION_DIRECTORY_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry limit exceeded",
                ));
            }
        }
        Ok(result)
    }

    fn identity(&self) -> io::Result<FileIdentity> {
        windows_identity(&self.file)
    }

    fn remove_file(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        fs::remove_file(self.path.join(name))
    }
    fn remove_directory(&self, name: &str) -> io::Result<()> {
        self.ensure_current_path()?;
        fs::remove_dir(self.path.join(name))
    }

    fn rename_child_noreplace(
        &self,
        _old_name: &str,
        new_name: &str,
        source: &SecureDir,
    ) -> io::Result<()> {
        use std::{
            mem::{offset_of, size_of},
            os::windows::io::AsRawHandle,
            ptr,
        };
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                FILE_RENAME_INFO, FILE_RENAME_INFO_0, FileRenameInfo, SetFileInformationByHandle,
            },
        };
        self.ensure_current_path()?;
        source.ensure_current_path()?;
        let wide = new_name.encode_utf16().collect::<Vec<_>>();
        let name_bytes = u32::try_from(wide.len().saturating_mul(size_of::<u16>()))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
        let header_size = offset_of!(FILE_RENAME_INFO, FileName);
        // Include the struct's trailing alignment padding as well as its UTF-16 filename.
        let byte_len = size_of::<FILE_RENAME_INFO>()
            .checked_add(name_bytes as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
        let words = byte_len.div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `buffer` is pointer-aligned and sized for the struct plus the complete UTF-16
        // filename. Both raw writes stay inside that allocation, and the handles remain live.
        unsafe {
            ptr::write(
                info,
                FILE_RENAME_INFO {
                    Anonymous: FILE_RENAME_INFO_0 {
                        ReplaceIfExists: false,
                    },
                    RootDirectory: self.file.as_raw_handle() as HANDLE,
                    FileNameLength: name_bytes,
                    FileName: [0],
                },
            );
            ptr::copy_nonoverlapping(
                wide.as_ptr(),
                info.cast::<u8>().add(header_size).cast::<u16>(),
                wide.len(),
            );
            let result = SetFileInformationByHandle(
                source.file.as_raw_handle() as HANDLE,
                FileRenameInfo,
                info.cast(),
                u32::try_from(byte_len).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "rename data too long")
                })?,
            );
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn ensure_current_path(&self) -> io::Result<()> {
        let current = open_windows_directory(&self.path)?;
        if windows_identity(&current)? != windows_identity(&self.file)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory path was substituted",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ADD_SUBDIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, SYNCHRONIZE,
    };
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .access_mode(
            FILE_LIST_DIRECTORY
                | FILE_READ_ATTRIBUTES
                | FILE_ADD_SUBDIRECTORY
                | DELETE
                | SYNCHRONIZE,
        )
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || windows_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a real directory",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(windows)]
fn windows_identity(file: &fs::File) -> io::Result<FileIdentity> {
    super::destination::windows_file_identity(file)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[derive(Clone)]
struct SecureDir;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl SecureDir {
    fn open_absolute(_path: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure directory access is not supported on this platform",
        ))
    }
    fn open_directory(&self, _name: &std::ffi::OsStr) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn create_directory(&self, _name: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn create_regular_file(&self, _name: &str) -> io::Result<fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn open_regular_file(&self, _name: &str) -> io::Result<fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn entries(&self) -> io::Result<Vec<(std::ffi::OsString, NodeKind)>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn identity(&self) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn remove_file(&self, _name: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn remove_directory(&self, _name: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
    fn rename_child_noreplace(
        &self,
        _old_name: &str,
        _new_name: &str,
        _source: &SecureDir,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        io::{self, Cursor, Write},
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use ring::digest::SHA256;
    use tempfile::tempdir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        install::command::prepare_install_session,
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport},
            staging::TempDirFactory,
        },
    };

    use super::{
        CommitError, InstallFaultInjector, InstallOperation, InstallationSession, commit_session,
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
                .prefix("m204-stage-")
                .tempdir_in(&self.0)
        }
    }

    fn archive() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
        let skill = format!("{root}skills/uncharted-ideation/");
        {
            let mut zip = ZipWriter::new(&mut bytes);
            zip.add_directory(root, options).expect("root");
            zip.add_directory(format!("{root}skills/"), options)
                .expect("skills");
            zip.add_directory(&skill, options).expect("skill");
            zip.start_file(format!("{root}README.md"), options)
                .expect("unrelated file");
            zip.write_all(b"must not be copied")
                .expect("unrelated bytes");
            for (name, content) in [
                (
                    format!("{skill}SKILL.md"),
                    b"---\nname: uncharted-ideation\ndescription: A deterministic fixture skill for filesystem installer tests.\n---\n".to_vec(),
                ),
                (
                    format!("{skill}references/guide.md"),
                    b"reference bytes\n".to_vec(),
                ),
                (
                    format!("{skill}scripts/run.sh"),
                    b"#!/bin/sh\necho inert\n".to_vec(),
                ),
                (
                    format!("{skill}scripts/long.sh"),
                    vec![b'x'; 24 * 1024],
                ),
                (
                    format!("{skill}assets/blob.bin"),
                    b"\0\x10\x80\xffbinary\0asset".to_vec(),
                ),
            ] {
                zip.start_file(name, options).expect("file");
                zip.write_all(&content).expect("file bytes");
            }
            zip.finish().expect("archive complete");
        }
        bytes.into_inner()
    }

    fn make_session(project: &Path, stage_parent: &Path) -> InstallationSession {
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");
        prepare_install_session(
            &catalog,
            "uncharted",
            "codex",
            None,
            project,
            &FakeTransport(archive()),
            &TestFactory(stage_parent.to_path_buf()),
        )
        .expect("validated offline source and plan")
    }

    #[derive(Default)]
    struct NoFaults;
    impl InstallFaultInjector for NoFaults {}

    struct FailAt {
        failures: Vec<(InstallOperation, usize, io::ErrorKind)>,
        seen: Mutex<HashMap<InstallOperation, usize>>,
    }

    impl FailAt {
        fn new(operation: InstallOperation, occurrence: usize, kind: io::ErrorKind) -> Self {
            Self {
                failures: vec![(operation, occurrence, kind)],
                seen: Mutex::new(HashMap::new()),
            }
        }
    }

    impl InstallFaultInjector for FailAt {
        fn before(&self, operation: InstallOperation) -> io::Result<()> {
            let mut seen = self.seen.lock().expect("fault counter");
            let count = seen.entry(operation).or_default();
            *count += 1;
            if let Some((_, _, kind)) = self
                .failures
                .iter()
                .find(|(target, occurrence, _)| *target == operation && *occurrence == *count)
            {
                Err(io::Error::new(*kind, "injected filesystem failure"))
            } else {
                Ok(())
            }
        }
    }

    struct OccupyTargetBeforeCommit {
        destination: PathBuf,
        called: Mutex<bool>,
    }

    impl InstallFaultInjector for OccupyTargetBeforeCommit {
        fn before(&self, operation: InstallOperation) -> io::Result<()> {
            if operation == InstallOperation::CommitNoReplace {
                let mut called = self.called.lock().expect("one shot");
                if !*called {
                    fs::create_dir(&self.destination)?;
                    fs::write(self.destination.join("user.txt"), b"user-owned")?;
                    *called = true;
                }
            }
            Ok(())
        }
    }

    fn project_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let parent = tempdir().expect("temporary parent");
        let project = parent.path().join("project");
        let stage_parent = parent.path().join("stage");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&stage_parent).expect("stage parent");
        (parent, project, stage_parent)
    }

    fn assert_no_receipts(project: &Path) {
        assert!(!project.join(".thrn").exists());
    }

    #[test]
    fn installs_only_validated_skill_files_with_sha256_and_inert_permissions() {
        let (_parent, project, stage_parent) = project_fixture();
        fs::write(project.join("README.md"), b"unrelated project file").expect("sentinel");
        let session = make_session(&project, &stage_parent);
        let staged_source = session.staged.content_directory().to_path_buf();
        assert!(
            staged_source.is_dir(),
            "session retains StagedSkill until commit"
        );
        let plan = session.plan().clone();

        session.commit().expect("filesystem commit");

        let destination = project.join(".agents/skills/uncharted-ideation");
        assert_eq!(
            plan.destination(),
            fs::canonicalize(&project)
                .unwrap()
                .join(".agents/skills/uncharted-ideation")
        );
        assert_eq!(
            fs::read(destination.join("SKILL.md")).unwrap(),
            fs::read(staged_source.join("SKILL.md")).unwrap()
        );
        assert_eq!(
            fs::read(destination.join("assets/blob.bin")).unwrap(),
            b"\0\x10\x80\xffbinary\0asset"
        );
        assert_eq!(
            fs::read(destination.join("references/guide.md")).unwrap(),
            b"reference bytes\n"
        );
        assert_eq!(
            fs::read(destination.join("scripts/run.sh")).unwrap(),
            b"#!/bin/sh\necho inert\n"
        );
        assert_eq!(
            fs::read(destination.join("scripts/long.sh")).unwrap(),
            vec![b'x'; 24 * 1024]
        );
        assert!(!destination.join("README.md").exists());
        assert!(!project.join(".agents/skills/README.md").exists());
        assert_eq!(
            fs::read(project.join("README.md")).unwrap(),
            b"unrelated project file"
        );
        assert_eq!(
            fs::read_dir(project.join(".agents/skills"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 1);
        assert_no_receipts(&project);
        drop(session);
        assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 0);

        for file in plan.files() {
            let copied = fs::read(destination.join(file.relative_path())).expect("copied file");
            let digest = ring::digest::digest(&SHA256, &copied);
            assert_eq!(digest.as_ref(), file.expected_sha256());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(destination.join(file.relative_path()))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(
                    mode & 0o111,
                    0,
                    "scripts are stored without executable bits"
                );
            }
        }
    }

    #[test]
    fn rejects_a_manually_installed_skill_without_changing_its_content() {
        let (_parent, project, stage_parent) = project_fixture();
        let destination = project.join(".agents/skills/uncharted-ideation");
        fs::create_dir_all(&destination).expect("manual skill");
        fs::write(destination.join("keep.txt"), b"manual bytes").expect("sentinel");
        let session = make_session(&project, &stage_parent);
        let error = session.commit().expect_err("external skill is rejected");
        assert!(
            matches!(error, CommitError::DestinationConflict),
            "{error:?}"
        );
        assert_eq!(
            fs::read(destination.join("keep.txt")).unwrap(),
            b"manual bytes"
        );
        assert_no_receipts(&project);
    }

    #[test]
    fn rejects_existing_file_symlink_and_casefold_collision() {
        let (_parent, project, stage_parent) = project_fixture();
        let skills = project.join(".agents/skills");
        fs::create_dir_all(&skills).unwrap();
        let destination = skills.join("uncharted-ideation");
        fs::write(&destination, b"user file").unwrap();
        let session = make_session(&project, &stage_parent);
        let error = session.commit().expect_err("existing file is rejected");
        assert!(
            matches!(error, CommitError::DestinationConflict),
            "{error:?}"
        );
        assert_eq!(fs::read(&destination).unwrap(), b"user file");
        drop(session);

        fs::remove_file(&destination).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = project.join("external");
            fs::create_dir(&external).unwrap();
            fs::write(external.join("keep.txt"), b"external").unwrap();
            symlink(&external, &destination).unwrap();
            let symlink_session = make_session(&project, &stage_parent);
            assert!(matches!(
                symlink_session.commit(),
                Err(CommitError::DestinationConflict)
            ));
            assert_eq!(fs::read(external.join("keep.txt")).unwrap(), b"external");
            drop(symlink_session);
            fs::remove_file(&destination).unwrap();
        }

        let collision = skills.join("UNCHARTED-IDEATION");
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("keep.txt"), b"case collision").unwrap();
        let collision_session = make_session(&project, &stage_parent);
        assert!(matches!(
            collision_session.commit(),
            Err(CommitError::DestinationConflict)
        ));
        assert_eq!(
            fs::read(collision.join("keep.txt")).unwrap(),
            b"case collision"
        );
        drop(collision_session);
        fs::remove_dir_all(&collision).unwrap();

        assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 0);
        assert_no_receipts(&project);
    }

    #[test]
    fn rejects_destination_or_source_mutations_after_planning() {
        let (_parent, project, stage_parent) = project_fixture();
        let session = make_session(&project, &stage_parent);
        let unrelated = project.join(".agents/skills/other-skill");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.txt"), b"new external skill").unwrap();
        assert!(matches!(
            session.commit(),
            Err(CommitError::DestinationChanged)
        ));
        assert_eq!(
            fs::read(unrelated.join("keep.txt")).unwrap(),
            b"new external skill"
        );

        let (_parent2, project2, stage_parent2) = project_fixture();
        let session2 = make_session(&project2, &stage_parent2);
        fs::write(
            session2
                .staged
                .content_directory()
                .join("references/guide.md"),
            b"changed after planning",
        )
        .unwrap();
        assert!(matches!(session2.commit(), Err(CommitError::SourceChanged)));
        assert!(!session2.plan.destination().exists());
        assert_no_receipts(&project2);

        let (_parent3, project3, stage_parent3) = project_fixture();
        let session3 = make_session(&project3, &stage_parent3);
        fs::remove_file(session3.staged.content_directory().join("assets/blob.bin")).unwrap();
        assert!(matches!(session3.commit(), Err(CommitError::SourceChanged)));
        assert!(!session3.plan.destination().exists());
        assert_no_receipts(&project3);
    }

    #[test]
    fn rejects_corrupted_plan_checksum_and_project_root_substitution() {
        let (_parent, project, stage_parent) = project_fixture();
        let mut session = make_session(&project, &stage_parent);
        session.inventory.corrupt_sha256_for_test();
        assert!(matches!(
            session.commit(),
            Err(CommitError::IdentityMismatch)
        ));
        assert!(!session.plan.destination().exists());

        let (_parent2, project2, stage_parent2) = project_fixture();
        let session2 = make_session(&project2, &stage_parent2);
        let moved = project2.with_extension("moved");
        fs::rename(&project2, &moved).unwrap();
        fs::create_dir(&project2).unwrap();
        assert!(matches!(
            session2.commit(),
            Err(CommitError::DestinationChanged)
        ));
        assert!(!project2.join(".agents").exists());
        assert_no_receipts(&project2);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_parent_symlink_substitution_and_preserves_external_files() {
        use std::os::unix::fs::symlink;

        let (_parent, project, stage_parent) = project_fixture();
        let session = make_session(&project, &stage_parent);
        let external = project.with_extension("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("keep.txt"), b"outside selected destination").unwrap();
        symlink(&external, project.join(".agents")).unwrap();
        assert!(matches!(
            session.commit(),
            Err(CommitError::DestinationConflict)
        ));
        assert_eq!(
            fs::read(external.join("keep.txt")).unwrap(),
            b"outside selected destination"
        );
        assert!(!external.join("skills").exists());
        assert_no_receipts(&project);
    }

    #[test]
    fn rejects_real_directory_substitution_with_matching_visible_entries() {
        let (_parent, project, stage_parent) = project_fixture();
        fs::create_dir_all(project.join(".agents/skills")).unwrap();
        let session = make_session(&project, &stage_parent);
        let old_agents = project.join(".agents-old");
        fs::rename(project.join(".agents"), &old_agents).unwrap();
        fs::create_dir_all(project.join(".agents/skills")).unwrap();
        assert!(matches!(
            session.commit(),
            Err(CommitError::DestinationChanged)
        ));
        assert!(old_agents.is_dir());
        assert!(project.join(".agents/skills").is_dir());
        assert!(!session.plan.destination().exists());
        assert_no_receipts(&project);
    }

    #[test]
    fn rejects_replacement_of_existing_skills_directory_identity() {
        let (_parent, project, stage_parent) = project_fixture();
        fs::create_dir_all(project.join(".agents/skills")).unwrap();
        let session = make_session(&project, &stage_parent);
        let old_skills = project.join(".agents/skills-old");
        fs::rename(project.join(".agents/skills"), &old_skills).unwrap();
        fs::create_dir(project.join(".agents/skills")).unwrap();
        assert!(matches!(
            session.commit(),
            Err(CommitError::DestinationChanged)
        ));
        assert!(old_skills.is_dir());
        assert!(project.join(".agents/skills").is_dir());
        assert!(!session.plan.destination().exists());
        assert_no_receipts(&project);
    }

    #[test]
    fn target_created_immediately_before_commit_is_never_replaced() {
        let (_parent, project, stage_parent) = project_fixture();
        let session = make_session(&project, &stage_parent);
        let destination = session.plan.destination().to_path_buf();
        let fault = OccupyTargetBeforeCommit {
            destination: destination.clone(),
            called: Mutex::new(false),
        };
        let error = commit_session(&session, &fault).expect_err("no-replace commit must fail");
        assert!(matches!(
            error,
            CommitError::Operation {
                operation: InstallOperation::CommitNoReplace,
                ..
            }
        ));
        assert_eq!(
            fs::read(destination.join("user.txt")).unwrap(),
            b"user-owned"
        );
        assert_no_receipts(&project);
    }

    #[test]
    fn injected_parent_directory_file_and_partial_write_failures_never_install() {
        for (operation, occurrence) in [
            (InstallOperation::CreateParentDirectory, 1),
            (InstallOperation::CreateParentDirectory, 2),
            (InstallOperation::CreateTemporaryDirectory, 1),
            (InstallOperation::CreateDirectory, 1),
            (InstallOperation::CreateFile, 2),
            (InstallOperation::WriteDestination, 5),
            (InstallOperation::ReadSource, 1),
            (InstallOperation::CommitNoReplace, 1),
        ] {
            let (_parent, project, stage_parent) = project_fixture();
            fs::write(project.join("keep.txt"), b"unrelated sentinel").unwrap();
            let session = make_session(&project, &stage_parent);
            let error = commit_session(
                &session,
                &FailAt::new(operation, occurrence, io::ErrorKind::PermissionDenied),
            )
            .expect_err("injected operation failure");
            assert!(
                matches!(error, CommitError::Operation { .. }),
                "{operation:?}: {error:?}"
            );
            assert!(!session.plan.destination().exists(), "{operation:?}");
            assert!(!project.join(".thrn").exists());
            assert_eq!(
                fs::read(project.join("keep.txt")).unwrap(),
                b"unrelated sentinel"
            );
            assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 1);
            drop(session);
            assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 0);
        }
    }

    #[test]
    fn reports_incomplete_cleanup_without_removing_unrelated_project_content() {
        for cleanup_operation in [
            InstallOperation::RemoveFile,
            InstallOperation::RemoveDirectory,
        ] {
            let (_parent, project, stage_parent) = project_fixture();
            fs::write(project.join("keep.txt"), b"project content").unwrap();
            let session = make_session(&project, &stage_parent);
            let error = commit_session(
                &session,
                &FailAt::new(
                    InstallOperation::WriteDestination,
                    1,
                    io::ErrorKind::WriteZero,
                )
                .with_failure(
                    cleanup_operation,
                    1,
                    io::ErrorKind::PermissionDenied,
                ),
            )
            .expect_err("write and cleanup fail");
            match error {
                CommitError::Operation {
                    cleanup_complete,
                    cleanup_errors,
                    ..
                } => {
                    assert!(!cleanup_complete);
                    assert!(!cleanup_errors.is_empty());
                }
                other => panic!("unexpected error: {other:?}"),
            }
            assert_eq!(
                fs::read(project.join("keep.txt")).unwrap(),
                b"project content"
            );
            assert!(!session.plan.destination().exists());
            assert_no_receipts(&project);
            drop(session);
            assert_eq!(fs::read_dir(&stage_parent).unwrap().count(), 0);
        }
    }

    impl FailAt {
        fn with_failure(
            mut self,
            operation: InstallOperation,
            occurrence: usize,
            kind: io::ErrorKind,
        ) -> Self {
            self.failures.push((operation, occurrence, kind));
            self
        }
    }
}
