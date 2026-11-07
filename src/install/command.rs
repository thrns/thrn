use std::{io, path::Path};

use thiserror::Error;

use crate::{
    models::catalog::Catalog,
    registry::discovery::{CatalogResolver, ResolutionError},
    sources::{
        github::{FetchError, GithubTransport, fetch_and_stage_skill_with_factory},
        staging::{StageError, TempDirFactory},
    },
};

use super::{
    commit::InstallationSession,
    destination::{DestinationConflict, DestinationError, inspect_destination},
    inventory::{InventoryError, inspect_staged_skill},
    plan::{InstallPlan, PlanError, build_install_plan},
};

#[derive(Debug, Error)]
pub(crate) enum InstallCommandError {
    #[error("M2-03 supports only --agent codex")]
    UnsupportedAgent,
    #[error("could not determine the current project directory ({kind})")]
    CurrentDirectory { kind: io::ErrorKind },
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
    #[error("installation dry-run is blocked by destination conflicts")]
    Conflicts { conflicts: Vec<DestinationConflict> },
    #[error("installation is blocked by destination conflicts")]
    InstallConflicts { conflicts: Vec<DestinationConflict> },
    #[error(
        "a THRN-managed installation already exists at this destination; it was left unchanged"
    )]
    DuplicateManagedInstallation,
    #[error(
        "an existing skill at this destination is not recorded as THRN-managed; it was left unchanged"
    )]
    ExistingUnmanagedSkill,
    #[error("THRN receipt store operation failed: {0}")]
    ReceiptStore(#[source] super::receipt_store::ReceiptStoreError),
    #[error(transparent)]
    Recovery(#[from] super::receipt_store::RecoveryError),
    #[error(transparent)]
    Completion(#[from] super::commit::InstallationCompletionError),
    #[error("could not write command output: {0}")]
    Output(#[from] io::Error),
}

/// Build a plan through the existing validated catalog and source pipeline. Transport and
/// temporary-directory creation are injected so all orchestration tests remain offline.
pub(crate) fn prepare_install_plan<T: GithubTransport + ?Sized>(
    catalog: &Catalog,
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    current_directory: &Path,
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<InstallPlan, InstallCommandError> {
    Ok(prepare_install_session(
        catalog,
        identifier,
        agent,
        project,
        current_directory,
        transport,
        tempdir_factory,
    )?
    .into_plan())
}

/// Prepare a commit-capable internal session that keeps the validated source alive alongside
/// its immutable plan and inventory. Real installation keeps this session through receipt-backed
/// completion; dry-run callers convert it into a read-only plan.
pub(crate) fn prepare_install_session<T: GithubTransport + ?Sized>(
    catalog: &Catalog,
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    current_directory: &Path,
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<InstallationSession, InstallCommandError> {
    if agent != "codex" {
        return Err(InstallCommandError::UnsupportedAgent);
    }

    let resource = CatalogResolver::new(catalog).resolve(identifier)?;
    let staged = fetch_and_stage_skill_with_factory(resource, transport, tempdir_factory)?;
    let inventory = inspect_staged_skill(&staged)?;
    let destination =
        inspect_destination(project, current_directory, staged.skill_name().as_str())?;
    let plan = build_install_plan(&staged, &inventory, destination)?;
    Ok(InstallationSession::new(staged, inventory, plan))
}

/// Render a complete successful plan into memory before the CLI writes one byte to stdout.
pub(crate) fn format_dry_run(plan: &InstallPlan) -> Result<String, InstallCommandError> {
    if !plan.conflicts().is_empty() {
        return Err(InstallCommandError::Conflicts {
            conflicts: plan.conflicts().to_vec(),
        });
    }

    let mut output = String::new();
    output.push_str("DRY RUN — no project files were modified\n");
    output.push_str(&format!("Resource: {}\n", plan.resource_id().as_str()));
    output.push_str(&format!("Skill name: {}\n", plan.skill_name().as_str()));
    output.push_str(&format!(
        "GitHub source: {}/{}\n",
        plan.source_owner().as_str(),
        plan.source_repository().as_str()
    ));
    output.push_str(&format!(
        "Pinned commit: {}\n",
        plan.source_commit().as_str()
    ));
    output.push_str(&format!(
        "Source directory: {}\n",
        plan.source_directory().as_str()
    ));
    output.push_str(&format!("Agent: {}\n", plan.agent().as_str()));
    output.push_str(&format!("Scope: {}\n", plan.scope().as_str()));
    output.push_str(&format!(
        "Project root: {}\n",
        safe_path_display(plan.project_root())
    ));
    output.push_str(&format!(
        "Destination: {}\n",
        safe_path_display(plan.destination())
    ));

    output.push_str(&format!(
        "Directories that would be created ({}):\n",
        plan.directories().len()
    ));
    if plan.directories().is_empty() {
        output.push_str("  (none)\n");
    } else {
        for directory in plan.directories() {
            output.push_str(&format!("  + {}\n", safe_path_display(directory.path())));
        }
    }

    output.push_str(&format!(
        "Regular files that would be copied ({}; {} bytes):\n",
        plan.files().len(),
        plan.total_file_bytes()
    ));
    for file in plan.files() {
        output.push_str(&format!(
            "  {} ({} bytes) -> {}\n",
            safe_path_display(file.relative_path()),
            file.expected_bytes(),
            safe_path_display(file.destination())
        ));
    }
    output.push_str(&format!(
        "Total: {} files, {} bytes\n",
        plan.files().len(),
        plan.total_file_bytes()
    ));
    output.push_str("No project files were modified.\n");
    Ok(output)
}

pub(crate) fn format_conflicts(conflicts: &[DestinationConflict]) -> String {
    let mut output = String::from("thrn: safe installation plan could not be built:\n");
    for conflict in conflicts {
        output.push_str(&format!(
            "  - {}: {}\n",
            safe_path_display(conflict.path()),
            conflict.detail()
        ));
    }
    output
}

fn safe_path_display(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .flat_map(char::escape_debug)
        .collect()
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

    use super::safe_path_display;
    use crate::{
        install::{
            command::{format_conflicts, format_dry_run, prepare_install_plan},
            destination::{ConflictKind, DestinationConflict},
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport},
            staging::TempDirFactory,
        },
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
                .prefix("cli-plan-test-")
                .tempdir_in(&self.0)
        }
    }

    const VALID_SKILL_MD: &[u8] = b"---\nname: uncharted-ideation\ndescription: A deterministic fixture skill for dry-run output tests.\n---\n";

    fn archive() -> Vec<u8> {
        archive_with_skill(VALID_SKILL_MD)
    }

    fn archive_with_skill(skill_md: &[u8]) -> Vec<u8> {
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
            zip.write_all(b"not selected")
                .expect("unrelated repository contents");
            for (name, contents) in [
                (format!("{skill}SKILL.md"), skill_md),
                (
                    format!("{skill}references/guide.md"),
                    b"reference".as_slice(),
                ),
                (format!("{skill}scripts/run.sh"), b"echo inert".as_slice()),
                (
                    format!("{skill}assets/blob.bin"),
                    b"\0\x10\x80\xff".as_slice(),
                ),
            ] {
                zip.start_file(name, options).expect("file");
                zip.write_all(contents).expect("contents");
            }
            zip.finish().expect("finish");
        }
        bytes.into_inner()
    }

    #[test]
    fn injectable_successful_orchestration_emits_complete_plan_and_cleans_stage() {
        let parent = tempdir().expect("temporary parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");
        let plan = prepare_install_plan(
            &catalog,
            "uncharted",
            "codex",
            None,
            &project,
            &FakeTransport(archive()),
            &TestFactory(staging.clone()),
        )
        .expect("offline fixture orchestration builds plan");
        let output = format_dry_run(&plan).expect("plan formats");
        let project_root_path = fs::canonicalize(&project).expect("resolve project root");
        let project_root = safe_path_display(&project_root_path);
        let destination_path = project_root_path
            .join(".agents")
            .join("skills")
            .join("uncharted-ideation");
        let destination = safe_path_display(&destination_path);
        let agents_directory = project_root_path.join(".agents");
        let skills_directory = agents_directory.join("skills");
        let asset_directory = destination_path.join("assets");
        let references_directory = destination_path.join("references");
        let scripts_directory = destination_path.join("scripts");
        let asset_file = asset_directory.join("blob.bin");
        let reference_file = references_directory.join("guide.md");
        let script_file = scripts_directory.join("run.sh");
        let asset_relative = PathBuf::from("assets").join("blob.bin");
        let reference_relative = PathBuf::from("references").join("guide.md");
        let script_relative = PathBuf::from("scripts").join("run.sh");
        let skill_md_bytes = VALID_SKILL_MD.len();
        let total_bytes = skill_md_bytes + b"reference".len() + b"echo inert".len() + 4;
        let expected = format!(
            "DRY RUN — no project files were modified\nResource: uncharted\nSkill name: uncharted-ideation\nGitHub source: Example-Org/catalog-test-fixtures\nPinned commit: 2f68bee292a8c68346b4294fb31d9563eefe1ade\nSource directory: skills/uncharted-ideation\nAgent: codex\nScope: project\nProject root: {project_root}\nDestination: {destination}\nDirectories that would be created (6):\n  + {}\n  + {}\n  + {}\n  + {}\n  + {}\n  + {}\nRegular files that would be copied (4; {total_bytes} bytes):\n  SKILL.md ({skill_md_bytes} bytes) -> {}\n  {} (4 bytes) -> {}\n  {} (9 bytes) -> {}\n  {} (10 bytes) -> {}\nTotal: 4 files, {total_bytes} bytes\nNo project files were modified.\n",
            safe_path_display(&agents_directory),
            safe_path_display(&skills_directory),
            safe_path_display(&destination_path),
            safe_path_display(&asset_directory),
            safe_path_display(&references_directory),
            safe_path_display(&scripts_directory),
            safe_path_display(&destination_path.join("SKILL.md")),
            safe_path_display(&asset_relative),
            safe_path_display(&asset_file),
            safe_path_display(&reference_relative),
            safe_path_display(&reference_file),
            safe_path_display(&script_relative),
            safe_path_display(&script_file),
        );
        assert_eq!(output, expected);
        assert!(!project.join(".agents").exists());
        assert_eq!(
            fs::read_dir(&staging)
                .expect("staging parent remains")
                .count(),
            0
        );
        assert!(!project.join(".thrn/receipts").exists());
    }

    #[test]
    fn no_partial_plan_text_is_produced_when_the_destination_conflicts() {
        let parent = tempdir().expect("temporary parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let existing = project.join(".agents/skills/uncharted-ideation");
        fs::create_dir_all(&existing).expect("existing skill");
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");
        let plan = prepare_install_plan(
            &catalog,
            "uncharted",
            "codex",
            None,
            &project,
            &FakeTransport(archive()),
            &TestFactory(staging.clone()),
        )
        .expect("conflicting plan retains conflict data");
        let error =
            format_dry_run(&plan).expect_err("conflicts cannot produce successful plan text");
        assert_eq!(
            error.to_string(),
            "installation dry-run is blocked by destination conflicts"
        );
        assert_eq!(
            fs::read_dir(existing)
                .expect("existing skill untouched")
                .count(),
            0
        );
        assert_eq!(fs::read_dir(&staging).expect("stage cleans up").count(), 0);
    }

    #[test]
    fn invalid_resource_and_agent_fail_before_any_transport_request() {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog");
        for (id, agent, expected) in [
            ("uncharted", "cursor", "only --agent codex"),
            ("bad--id", "codex", "invalid skill identifier"),
        ] {
            let error = prepare_install_plan(
                &catalog,
                id,
                agent,
                None,
                &project,
                &NeverTransport,
                &TestFactory(staging.clone()),
            )
            .expect_err("invalid request fails");
            assert!(error.to_string().contains(expected));
        }
        assert_eq!(fs::read_dir(&staging).expect("no stage created").count(), 0);
        assert!(!project.join(".agents").exists());
    }

    #[test]
    fn conflict_diagnostics_escape_control_characters_in_untrusted_paths() {
        let conflict = DestinationConflict::new(
            PathBuf::from("skill\n\u{1b}[31m"),
            ConflictKind::ExistingDirectory,
            "existing content".to_owned(),
        );
        let message = format_conflicts(&[conflict]);
        assert!(message.contains("skill\\n\\u{1b}[31m"));
        assert!(!message.contains("skill\n\u{1b}"));
    }

    #[test]
    fn explicit_project_option_is_resolved_and_used_as_the_plan_root() {
        let parent = tempdir().expect("parent");
        let current = parent.path().join("workspace");
        let project = current.join("selected-project");
        let staging = parent.path().join("staging");
        fs::create_dir(&current).expect("workspace");
        fs::create_dir(&project).expect("selected project");
        fs::create_dir(&staging).expect("staging");
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");
        let plan = prepare_install_plan(
            &catalog,
            "idea-lab",
            "codex",
            Some(Path::new("selected-project")),
            &current,
            &FakeTransport(archive()),
            &TestFactory(staging.clone()),
        )
        .expect("explicit project plan");
        assert_eq!(
            plan.project_root(),
            fs::canonicalize(&project).expect("canonical selected project")
        );
        assert!(!current.join(".agents").exists());
        assert!(!project.join(".agents").exists());
        assert_eq!(fs::read_dir(staging).expect("stage cleaned").count(), 0);
    }

    #[test]
    fn fetch_archive_and_skill_metadata_failures_never_return_a_plan() {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        let staging = parent.path().join("staging");
        fs::create_dir(&project).expect("project");
        fs::create_dir(&staging).expect("staging");
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog fixture");

        let fetch_error = prepare_install_plan(
            &catalog,
            "uncharted",
            "codex",
            None,
            &project,
            &StaticResponseTransport {
                status: 404,
                contents: Vec::new(),
            },
            &TestFactory(staging.clone()),
        )
        .expect_err("HTTP error must fail");
        assert!(fetch_error.to_string().contains("HTTP status 404"));
        assert_eq!(
            fs::read_dir(&staging).expect("no temporary stage").count(),
            0
        );

        let archive_error = prepare_install_plan(
            &catalog,
            "uncharted",
            "codex",
            None,
            &project,
            &StaticResponseTransport {
                status: 200,
                contents: b"not a ZIP archive".to_vec(),
            },
            &TestFactory(staging.clone()),
        )
        .expect_err("invalid ZIP must fail");
        assert!(archive_error.to_string().contains("archive"));
        assert_eq!(
            fs::read_dir(&staging).expect("no temporary stage").count(),
            0
        );

        let metadata_error = prepare_install_plan(
            &catalog,
            "uncharted",
            "codex",
            None,
            &project,
            &FakeTransport(archive_with_skill(b"metadata is invalid")),
            &TestFactory(staging.clone()),
        )
        .expect_err("invalid skill metadata must fail");
        assert!(
            metadata_error
                .to_string()
                .contains("staged SKILL.md failed local metadata validation")
        );
        assert_eq!(
            fs::read_dir(&staging)
                .expect("failed stage cleaned")
                .count(),
            0
        );
        assert!(!project.join(".agents").exists());
    }

    struct NeverTransport;
    impl GithubTransport for NeverTransport {
        fn get(&self, _: &str) -> Result<GithubResponse, crate::sources::github::TransportError> {
            panic!("transport must not be called for validation failures")
        }
    }

    struct StaticResponseTransport {
        status: u16,
        contents: Vec<u8>,
    }

    impl GithubTransport for StaticResponseTransport {
        fn get(&self, _: &str) -> Result<GithubResponse, crate::sources::github::TransportError> {
            Ok(GithubResponse::new(
                self.status,
                Some("application/zip"),
                Some(self.contents.len() as u64),
                None,
                Cursor::new(self.contents.clone()),
            ))
        }
    }
}
