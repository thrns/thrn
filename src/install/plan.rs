use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    models::catalog::{
        CommitSha, GithubOwner, GithubRepository, ResourceVersion, SkillId, SkillName, SourcePath,
    },
    sources::staging::StagedSkill,
};

use super::{
    destination::{ComponentState, DestinationConflict, DestinationSnapshot},
    inventory::StagedInventory,
};

pub use crate::agents::AgentId as InstallAgent;

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum InstallScope {
    Project,
    User,
}

impl InstallScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// A directory whose creation is part of the proposed plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedDirectory {
    path: PathBuf,
}

impl PlannedDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A validated regular file the proposed plan would copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedFile {
    relative_path: PathBuf,
    destination: PathBuf,
    expected_bytes: u64,
    expected_crc32: u32,
    expected_sha256: [u8; 32],
}

impl PlannedFile {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn expected_bytes(&self) -> u64 {
        self.expected_bytes
    }

    /// CRC-32 from the already validated archive manifest, for later stage revalidation.
    pub fn expected_crc32(&self) -> u32 {
        self.expected_crc32
    }

    pub fn expected_sha256(&self) -> &[u8; 32] {
        &self.expected_sha256
    }
}

/// A read-only, immutable proposal. It records the destination assumptions and source inventory
/// required for a later operation to revalidate before it writes anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallPlan {
    resource_id: SkillId,
    skill_name: SkillName,
    resource_version: ResourceVersion,
    source_owner: GithubOwner,
    source_repository: GithubRepository,
    source_commit: CommitSha,
    source_directory: SourcePath,
    agent: InstallAgent,
    scope: InstallScope,
    project_root: PathBuf,
    destination: PathBuf,
    directories: Vec<PlannedDirectory>,
    files: Vec<PlannedFile>,
    total_file_bytes: u64,
    conflicts: Vec<DestinationConflict>,
    destination_snapshot: DestinationSnapshot,
}

impl InstallPlan {
    pub fn resource_id(&self) -> &SkillId {
        &self.resource_id
    }

    pub fn skill_name(&self) -> &SkillName {
        &self.skill_name
    }

    pub fn resource_version(&self) -> &ResourceVersion {
        &self.resource_version
    }

    pub fn source_owner(&self) -> &GithubOwner {
        &self.source_owner
    }

    pub fn source_repository(&self) -> &GithubRepository {
        &self.source_repository
    }

    pub fn source_commit(&self) -> &CommitSha {
        &self.source_commit
    }

    pub fn source_directory(&self) -> &SourcePath {
        &self.source_directory
    }

    pub fn agent(&self) -> InstallAgent {
        self.agent
    }

    pub fn scope(&self) -> InstallScope {
        self.scope
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub(crate) fn scope_root(&self) -> &Path {
        &self.project_root
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn directories(&self) -> &[PlannedDirectory] {
        &self.directories
    }

    pub fn files(&self) -> &[PlannedFile] {
        &self.files
    }

    pub fn total_file_bytes(&self) -> u64 {
        self.total_file_bytes
    }

    pub fn conflicts(&self) -> &[DestinationConflict] {
        &self.conflicts
    }

    pub fn destination_snapshot(&self) -> &DestinationSnapshot {
        &self.destination_snapshot
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PlanError {
    #[error("staged inventory does not belong to the selected catalog resource")]
    InventoryIdentityMismatch,
    #[error("destination snapshot does not describe a path below its project root")]
    DestinationOutsideProject,
    #[error("staged file path would escape the intended skill destination")]
    FileOutsideDestination,
    #[error("planned byte total exceeds the validated source limit")]
    ByteTotalOverflow,
    #[error("planned file sizes do not match the validated staged inventory total")]
    InventoryTotalMismatch,
    #[error("destination snapshot uses a different agent adapter")]
    AgentMismatch,
    #[error("selected agent does not support this skill scope")]
    UnsupportedScope,
    #[error("skill name is reserved by the selected agent")]
    ReservedSkillName,
}

/// Pure builder: it consumes M2-02 validated staged metadata, a read-only staged inventory, and
/// a read-only destination snapshot. It performs no filesystem access or writes.
pub fn build_install_plan(
    staged: &StagedSkill,
    inventory: &StagedInventory,
    destination: DestinationSnapshot,
) -> Result<InstallPlan, PlanError> {
    build_install_plan_for_agent_scope(
        staged,
        inventory,
        destination,
        InstallAgent::Codex,
        InstallScope::Project,
    )
}

#[cfg(test)]
pub(crate) fn build_install_plan_for_scope(
    staged: &StagedSkill,
    inventory: &StagedInventory,
    destination: DestinationSnapshot,
    scope: InstallScope,
) -> Result<InstallPlan, PlanError> {
    build_install_plan_for_agent_scope(staged, inventory, destination, InstallAgent::Codex, scope)
}

pub(crate) fn build_install_plan_for_agent_scope(
    staged: &StagedSkill,
    inventory: &StagedInventory,
    destination: DestinationSnapshot,
    agent: InstallAgent,
    scope: InstallScope,
) -> Result<InstallPlan, PlanError> {
    if destination.agent() != agent {
        return Err(PlanError::AgentMismatch);
    }
    if agent
        .adapter()
        .rejects_skill_name(staged.skill_name().as_str())
    {
        return Err(PlanError::ReservedSkillName);
    }
    if inventory.catalog_id() != staged.catalog_id()
        || inventory.skill_name() != staged.skill_name()
        || inventory.revision() != staged.revision()
        || inventory.content_directory() != staged.content_directory()
    {
        return Err(PlanError::InventoryIdentityMismatch);
    }

    let project_root = destination.project_root().to_path_buf();
    let intended_destination = destination.destination().to_path_buf();
    let expected_destination = agent
        .adapter()
        .destination_path(scope, project_root.clone(), staged.skill_name().as_str())
        .ok_or(PlanError::UnsupportedScope)?;
    if intended_destination != expected_destination
        || !intended_destination.starts_with(&project_root)
    {
        return Err(PlanError::DestinationOutsideProject);
    }

    let mut directory_paths = BTreeSet::new();
    if destination.agents_state() == ComponentState::Missing {
        directory_paths.insert(project_root.join(agent.adapter().skill_directory()));
    }
    if destination.skills_state() == ComponentState::Missing {
        directory_paths.insert(agent.adapter().skills_root(project_root.clone()));
    }
    if destination.skill_state() == ComponentState::Missing {
        directory_paths.insert(intended_destination.clone());
    }
    for relative in inventory.directories() {
        let path = intended_destination.join(relative);
        if !path.starts_with(&intended_destination) {
            return Err(PlanError::FileOutsideDestination);
        }
        directory_paths.insert(path);
    }
    let mut directory_paths = directory_paths.into_iter().collect::<Vec<_>>();
    directory_paths.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let directories = directory_paths
        .into_iter()
        .map(|path| PlannedDirectory { path })
        .collect::<Vec<_>>();

    let mut files = Vec::with_capacity(inventory.files().len());
    let mut total_file_bytes = 0_u64;
    for file in inventory.files() {
        let file_destination = intended_destination.join(file.relative_path());
        if !file_destination.starts_with(&intended_destination) {
            return Err(PlanError::FileOutsideDestination);
        }
        total_file_bytes = total_file_bytes
            .checked_add(file.expected_bytes())
            .ok_or(PlanError::ByteTotalOverflow)?;
        files.push(PlannedFile {
            relative_path: file.relative_path().to_path_buf(),
            destination: file_destination,
            expected_bytes: file.expected_bytes(),
            expected_crc32: file.expected_crc32(),
            expected_sha256: *file.expected_sha256(),
        });
    }
    if total_file_bytes != inventory.total_bytes() {
        return Err(PlanError::InventoryTotalMismatch);
    }

    let source = staged.source();
    Ok(InstallPlan {
        resource_id: staged.catalog_id().clone(),
        skill_name: staged.skill_name().clone(),
        resource_version: staged.resource_version().clone(),
        source_owner: source.owner().clone(),
        source_repository: source.repository().clone(),
        source_commit: staged.revision().clone(),
        source_directory: source.path().clone(),
        agent,
        scope,
        project_root,
        destination: intended_destination,
        directories,
        files,
        total_file_bytes,
        conflicts: destination.conflicts().to_vec(),
        destination_snapshot: destination,
    })
}

/// Build the replacement copy plan for a receipt-verified managed directory. The caller must
/// independently prove ownership and revalidate the destination before commitment; this builder
/// only removes the expected existing-directory conflict at that exact destination.
pub(crate) fn build_update_install_plan_for_agent_scope(
    staged: &StagedSkill,
    inventory: &StagedInventory,
    destination: DestinationSnapshot,
    agent: InstallAgent,
    scope: InstallScope,
) -> Result<InstallPlan, PlanError> {
    let mut plan =
        build_install_plan_for_agent_scope(staged, inventory, destination, agent, scope)?;
    plan.conflicts.retain(|conflict| {
        conflict.path() != plan.destination
            || conflict.kind() != super::destination::ConflictKind::ExistingDirectory
    });
    Ok(plan)
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
        install::{destination::inspect_destination, inventory::inspect_staged_skill},
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, fetch_and_stage_skill_with_factory},
            staging::TempDirFactory,
        },
    };

    use super::{InstallAgent, InstallScope, build_install_plan};

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
                .prefix("plan-test-")
                .tempdir_in(&self.0)
        }
    }

    fn archive() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut bytes);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
            let skill = format!("{root}skills/uncharted-ideation/");
            zip.add_directory(root, options).expect("root");
            zip.add_directory(format!("{root}skills/"), options)
                .expect("skills");
            zip.add_directory(&skill, options).expect("skill");
            zip.start_file(format!("{root}README.md"), options)
                .expect("unrelated repository file");
            zip.write_all(b"not selected").expect("unrelated contents");
            for (name, contents) in [
                (format!("{skill}SKILL.md"), b"---\nname: uncharted-ideation\ndescription: A test skill for the immutable installation planner.\n---\n".as_slice()),
                (format!("{skill}references/guide.md"), b"reference".as_slice()),
                (format!("{skill}scripts/run.sh"), b"echo inert".as_slice()),
                (format!("{skill}assets/blob.bin"), b"\0\x10\x80\xff".as_slice()),
            ] {
                zip.start_file(name, options).expect("file");
                zip.write_all(contents).expect("contents");
            }
            zip.finish().expect("finish");
        }
        bytes.into_inner()
    }

    fn plan_for(
        identifier: &str,
        project: &Path,
        staging: &Path,
    ) -> crate::install::plan::InstallPlan {
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");
        let resource = crate::registry::discovery::CatalogResolver::new(&catalog)
            .resolve(identifier)
            .expect("catalog ID or alias resolves");
        let staged = fetch_and_stage_skill_with_factory(
            resource,
            &FakeTransport(archive()),
            &TestFactory(staging.to_path_buf()),
        )
        .expect("fixture source stages");
        let inventory = inspect_staged_skill(&staged).expect("inventory validates");
        let destination = inspect_destination(None, project, staged.skill_name().as_str())
            .expect("project destination inspects");
        build_install_plan(&staged, &inventory, destination).expect("plan builds")
    }

    #[test]
    fn canonical_id_and_alias_produce_the_same_plan_with_actual_skill_name_destination() {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let by_id = plan_for("uncharted", &project, &staging);
        let by_alias = plan_for("idea-lab", &project, &staging);

        assert_eq!(by_id, by_alias);
        assert_eq!(by_id.resource_id().as_str(), "uncharted");
        assert_ne!(by_id.resource_id().as_str(), by_id.skill_name().as_str());
        assert_eq!(by_id.skill_name().as_str(), "uncharted-ideation");
        assert_eq!(by_id.agent(), InstallAgent::Codex);
        assert_eq!(by_id.scope(), InstallScope::Project);
        let resolved_project = fs::canonicalize(&project).expect("resolve project root");
        assert_eq!(
            by_id.destination(),
            resolved_project.join(".agents/skills/uncharted-ideation")
        );
        assert_eq!(by_id.files().len(), 4);
        assert_eq!(
            by_id.total_file_bytes(),
            by_id
                .files()
                .iter()
                .map(|file| file.expected_bytes())
                .sum::<u64>()
        );
        assert_eq!(by_id.directories().len(), 6);
        assert!(!project.join(".agents").exists());
        assert_eq!(fs::read_dir(&staging).expect("staging parent").count(), 0);
    }

    #[test]
    fn builder_preserves_case_insensitive_conflicts_without_touching_the_destination() {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let skills = project.join(".agents/skills");
        fs::create_dir_all(&skills).expect("skills directory");
        let collision = skills.join("UNCHARTED-IDEATION");
        fs::create_dir(&collision).expect("manually installed case collision");
        fs::write(collision.join("keep.txt"), b"keep").expect("sentinel content");

        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog");
        let resource = &catalog.resources()[0];
        let staged = fetch_and_stage_skill_with_factory(
            resource,
            &FakeTransport(archive()),
            &TestFactory(staging.clone()),
        )
        .expect("stage");
        let inventory = inspect_staged_skill(&staged).expect("inventory");
        let destination =
            inspect_destination(None, &project, staged.skill_name().as_str()).expect("destination");
        let planned = build_install_plan(&staged, &inventory, destination)
            .expect("plan builds with conflict data");

        assert_eq!(planned.conflicts().len(), 1);
        assert_eq!(
            planned.conflicts()[0].kind(),
            crate::install::destination::ConflictKind::CaseInsensitiveCollision
        );
        assert_eq!(
            fs::read(collision.join("keep.txt")).expect("sentinel remains"),
            b"keep"
        );
        assert_eq!(fs::read_dir(&skills).expect("skills unchanged").count(), 1);
    }

    #[test]
    fn builder_includes_all_missing_destination_directories_in_order() {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let plan = plan_for("uncharted", &project, &staging);
        let resolved_project = fs::canonicalize(&project).expect("resolve project root");
        let paths = plan
            .directories()
            .iter()
            .map(|directory| directory.path())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                resolved_project.join(".agents"),
                resolved_project.join(".agents/skills"),
                resolved_project.join(".agents/skills/uncharted-ideation"),
                resolved_project.join(".agents/skills/uncharted-ideation/assets"),
                resolved_project.join(".agents/skills/uncharted-ideation/references"),
                resolved_project.join(".agents/skills/uncharted-ideation/scripts"),
            ]
        );
    }
}
