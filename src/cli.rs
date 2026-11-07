use std::{
    io::{self, Write},
    path::Path,
};

use clap::{Parser, Subcommand};

use crate::{
    doctor,
    error::Result,
    install::{
        command::{
            InstallCommandError, format_dry_run, prepare_install_plan, prepare_install_session,
        },
        commit::{InstallationCompletionError, InstallationSession},
        destination::ConflictKind,
        journal::NoJournalFaults,
        receipt::InstallationReceipt,
        receipt_store::{NoReceiptFaults, ReceiptStore, ReceiptStoreError},
    },
    registry::{
        catalog_service::{CatalogSelection, CatalogService, Freshness},
        discovery::CatalogResolver,
    },
    skills::validator::validate_skill_directory,
};

#[derive(Debug, Parser)]
#[command(
    name = "thrn",
    version,
    about = "A local-first developer CLI with offline skill discovery."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report the CLI build metadata without changing files or using the network.
    Doctor,
    /// Search and inspect curated Agent Skills catalog metadata.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    /// List all skills available through the local curated catalog.
    List {
        /// List only installations recorded by THRN.
        #[arg(long)]
        installed: bool,
    },
    /// Search locally available curated Agent Skills metadata.
    Search {
        query: Option<String>,
        /// Fetch the fixed trusted HTTPS catalog and replace the offline cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Show curated catalog metadata for an exact ID or explicit alias.
    Info { id: String },
    /// Validate a local skill directory without modifying or executing its contents.
    Validate { path: std::path::PathBuf },
    /// Inspect a read-only plan or install a pinned Codex project skill.
    Install {
        /// Exact catalog resource ID or explicit alias.
        id: String,
        /// Select the only currently supported agent (codex).
        #[arg(long, required = true, value_name = "AGENT")]
        agent: String,
        /// Select an existing project directory (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        project: Option<std::path::PathBuf>,
        /// Print the complete plan without changing the project.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run() -> Result<()> {
    match Cli::try_parse()
        .map_err(crate::error::ThrnError::from)?
        .command
    {
        Command::Doctor => doctor::run(),
        Command::Skills { command } => match command {
            SkillsCommand::List { installed } => list_skills(installed),
            SkillsCommand::Search { query, refresh } => search_skills(query.as_deref(), refresh),
            SkillsCommand::Info { id } => show_skill_info(&id),
            SkillsCommand::Validate { path } => validate_skill(&path),
            SkillsCommand::Install {
                id,
                agent,
                project,
                dry_run,
            } => install_skill(&id, &agent, project.as_deref(), dry_run),
        },
    }
}

fn install_skill(
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    dry_run: bool,
) -> Result<()> {
    if agent != "codex" {
        return Err(InstallCommandError::UnsupportedAgent.into());
    }
    let current_directory = std::env::current_dir()
        .map_err(|error| InstallCommandError::CurrentDirectory { kind: error.kind() })?;
    let selection = CatalogService::for_system().discover()?;
    let transport = crate::sources::github::UreqGithubTransport::new();
    let output = if dry_run {
        let plan = prepare_install_plan(
            &selection.catalog,
            identifier,
            agent,
            project,
            &current_directory,
            &transport,
            &crate::sources::staging::SystemTempDirFactory,
        )?;
        format_dry_run(&plan)?
    } else {
        execute_install_with(
            identifier,
            agent,
            project,
            InstallContext {
                catalog: &selection.catalog,
                current_directory: &current_directory,
                transport: &transport,
                tempdir_factory: &crate::sources::staging::SystemTempDirFactory,
                receipt_store: ReceiptStore::for_system,
                complete: InstallationSession::commit_and_record,
            },
        )?
    };
    report_cache_warning(selection.cache_warning.as_deref())?;
    write_stdout(&output)
}

/// Run the non-dry-run CLI flow with injectable source, staging, receipt-store, and completion
/// boundaries. Production uses the fixed GitHub transport, the system staging directory, the
/// system receipt store, and the recovery-aware session completion method.
struct InstallContext<'a, T, S, C> {
    catalog: &'a crate::models::catalog::Catalog,
    current_directory: &'a Path,
    transport: &'a T,
    tempdir_factory: &'a dyn crate::sources::staging::TempDirFactory,
    receipt_store: S,
    complete: C,
}

fn execute_install_with<T, S, C>(
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    context: InstallContext<'_, T, S, C>,
) -> std::result::Result<String, InstallCommandError>
where
    T: crate::sources::github::GithubTransport,
    S: FnOnce() -> std::result::Result<ReceiptStore, ReceiptStoreError>,
    C: FnOnce(
        &InstallationSession,
        &ReceiptStore,
    ) -> std::result::Result<InstallationReceipt, InstallationCompletionError>,
{
    if agent != "codex" {
        return Err(InstallCommandError::UnsupportedAgent);
    }

    let session = prepare_install_session(
        context.catalog,
        identifier,
        agent,
        project,
        context.current_directory,
        context.transport,
        context.tempdir_factory,
    )?;
    if !session.plan().conflicts().is_empty() {
        // A read-only receipt lookup lets the CLI distinguish an existing THRN-owned install
        // from a manually installed directory. Reconcile a valid interrupted operation first;
        // the unchanged destination will then be recognized as a managed duplicate.
        let store = (context.receipt_store)().map_err(InstallCommandError::ReceiptStore)?;
        store
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .map_err(InstallCommandError::Recovery)?;
        let receipts = store.list().map_err(InstallCommandError::ReceiptStore)?;
        if receipts.iter().any(|receipt| {
            receipt
                .destination_path()
                .to_string_lossy()
                .eq_ignore_ascii_case(&session.plan().destination().to_string_lossy())
        }) {
            return Err(InstallCommandError::DuplicateManagedInstallation);
        }
        if session.plan().conflicts().iter().any(|conflict| {
            conflict.path() == session.plan().destination()
                && conflict.kind() == ConflictKind::ExistingDirectory
        }) {
            return Err(InstallCommandError::ExistingUnmanagedSkill);
        }
        return Err(InstallCommandError::InstallConflicts {
            conflicts: session.plan().conflicts().to_vec(),
        });
    }

    let store = (context.receipt_store)().map_err(InstallCommandError::ReceiptStore)?;
    let receipt = (context.complete)(&session, &store)?;
    Ok(format_install_success(&receipt))
}

fn format_install_success(receipt: &InstallationReceipt) -> String {
    let destination = safe_path_display(&receipt.destination_path());
    format!(
        "Installed THRN skill\nResource: {}\nSkill name: {}\nAgent: {}\nScope: {}\nDestination: {}\nPinned revision: {}/{}@{}\nFiles installed: {}\nBytes installed: {}\nOwnership receipt: recorded\n",
        receipt.catalog_resource_id(),
        receipt.skill_name(),
        receipt.agent(),
        receipt.scope(),
        destination,
        receipt.source_owner(),
        receipt.source_repository(),
        receipt.source_commit(),
        receipt.file_count(),
        receipt.byte_count(),
    )
}

fn safe_path_display(path: &Path) -> String {
    let mut output = String::new();
    for character in path.to_string_lossy().chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

fn list_skills(installed: bool) -> Result<()> {
    if installed {
        let store = ReceiptStore::for_system()?;
        let output = installed_listing_with(&store)?;
        return write_stdout(&output);
    }

    let selection = CatalogService::for_system().discover()?;
    report_cache_warning(selection.cache_warning.as_deref())?;
    let resolver = CatalogResolver::new(&selection.catalog);
    let resources = resolver.search(None);
    if resources.is_empty() {
        return write_stdout(&format!(
            "{}No catalog skills are available in the selected catalog.\n",
            catalog_heading(&selection)
        ));
    }

    let mut output = format!(
        "{}Curated skill catalog (schema v{})\n",
        catalog_heading(&selection),
        selection.catalog.schema_version()
    );
    for resource in resources {
        output.push_str(resource.id().as_str());
        output.push_str("  ");
        output.push_str(resource.display_name());
        output.push('\n');
        output.push_str("  ");
        output.push_str(resource.description());
        output.push('\n');
    }
    write_stdout(&output)
}

fn installed_listing_with(store: &ReceiptStore) -> std::result::Result<String, ReceiptStoreError> {
    let receipts = store.list()?;
    Ok(format_installed_list(&receipts))
}

fn format_installed_list(receipts: &[InstallationReceipt]) -> String {
    if receipts.is_empty() {
        return "THRN-managed installations\nNo THRN-managed installations are recorded.\nSkills installed outside THRN are not included.\n".to_owned();
    }
    let mut output = String::from("THRN-managed installations\n");
    for receipt in receipts {
        output.push_str(receipt.catalog_resource_id());
        output.push_str("  ");
        output.push_str(receipt.skill_name());
        output.push('\n');
        output.push_str(&format!("  Agent: {}\n", receipt.agent()));
        output.push_str(&format!("  Scope: {}\n", receipt.scope()));
        output.push_str(&format!(
            "  Project destination: {}\n",
            receipt.destination_path().display()
        ));
        output.push_str(&format!(
            "  Pinned revision: {}/{}@{}\n",
            receipt.source_owner(),
            receipt.source_repository(),
            receipt.source_commit()
        ));
        output.push_str("  Ownership: recorded by THRN; current content integrity: not checked.\n");
    }
    output.push_str("Skills installed outside THRN are not included.\n");
    output
}

fn search_skills(query: Option<&str>, refresh: bool) -> Result<()> {
    let service = CatalogService::for_system();
    let selection = if refresh {
        service.refresh()?
    } else {
        service.discover()?
    };
    report_cache_warning(selection.cache_warning.as_deref())?;
    let resolver = CatalogResolver::new(&selection.catalog);
    let resources = resolver.search(query);

    let output = if resources.is_empty() {
        if selection.catalog.resources().is_empty() {
            format!(
                "{}No skills are available in the selected catalog.\n",
                catalog_heading(&selection)
            )
        } else if let Some(query) = query {
            format!(
                "{}No skills matched query {query:?}.\nTry `thrn skills search` to list available skills.\n",
                catalog_heading(&selection)
            )
        } else {
            format!(
                "{}No skills are available in the selected catalog.\n",
                catalog_heading(&selection)
            )
        }
    } else {
        let mut output = format!(
            "{}Curated skill catalog (schema v1)\n",
            catalog_heading(&selection)
        );
        for resource in resources {
            output.push_str(resource.id().as_str());
            output.push_str("  ");
            output.push_str(resource.display_name());
            output.push('\n');
            output.push_str("  ");
            output.push_str(resource.description());
            output.push('\n');
        }
        output
    };
    write_stdout(&output)
}

fn show_skill_info(identifier: &str) -> Result<()> {
    let selection = CatalogService::for_system().discover()?;
    report_cache_warning(selection.cache_warning.as_deref())?;
    let resolver = CatalogResolver::new(&selection.catalog);
    let resource = resolver.resolve(identifier)?;
    let aliases = if resource.aliases().is_empty() {
        "(none)".to_owned()
    } else {
        resource
            .aliases()
            .iter()
            .map(|alias| alias.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let source = resource.source();
    let output = format!(
        "Curated catalog information (schema v{})\nCatalog source: {}\nCatalog freshness: {}\nCanonical catalog ID: {}\nDisplay name: {}\nDescription: {}\nSkill directory name: {}\nCatalog version: {}\nExplicit aliases: {}\nGitHub owner: {}\nGitHub repository: {}\nSource directory: {}\nPinned commit: {}\nPublisher authenticity: not guaranteed by this commit pin.\n",
        selection.catalog.schema_version(),
        selection.source_label(),
        freshness_label(selection.freshness),
        resource.id().as_str(),
        resource.display_name(),
        resource.description(),
        resource.skill_name().as_str(),
        resource.version().as_str(),
        aliases,
        source.owner().as_str(),
        source.repository().as_str(),
        source.path().as_str(),
        source.commit().as_str(),
    );
    write_stdout(&output)
}

fn catalog_heading(selection: &CatalogSelection) -> String {
    format!(
        "Catalog source: {}; freshness: {}.\n",
        selection.source_label(),
        freshness_label(selection.freshness)
    )
}

fn freshness_label(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Fresh => "fresh (last successful refresh was within 24 hours)",
        Freshness::Stale => "stale (last successful refresh was more than 24 hours ago)",
        Freshness::Unknown => "unknown (no validated cache is available)",
    }
}

fn report_cache_warning(warning: Option<&str>) -> Result<()> {
    if let Some(warning) = warning {
        let mut stderr = io::stderr().lock();
        writeln!(
            stderr,
            "thrn: {warning}; using bundled metadata. Run `thrn skills search --refresh` to recover the cache."
        )?;
        stderr.flush()?;
    }
    Ok(())
}

fn validate_skill(path: &Path) -> Result<()> {
    let metadata = validate_skill_directory(path)
        .map_err(|errors| crate::error::ThrnError::SkillValidation { errors })?;
    let license = metadata
        .license
        .as_deref()
        .map(safe_display)
        .unwrap_or_else(|| "(not specified)".to_owned());
    let compatibility = metadata
        .compatibility
        .as_deref()
        .map(safe_display)
        .unwrap_or_else(|| "(not specified)".to_owned());
    let allowed_tools = metadata
        .allowed_tools
        .as_deref()
        .map(safe_display)
        .unwrap_or_else(|| "(not specified)".to_owned());
    let output = format!(
        "Skill validation passed\nName: {}\nDescription: {}\nLicense: {}\nCompatibility: {}\nAllowed tools: {}\nAdditional metadata entries: {}\n",
        safe_display(&metadata.name),
        safe_display(&metadata.description),
        license,
        compatibility,
        allowed_tools,
        metadata.metadata.len(),
    );
    write_stdout(&output)
}

fn safe_display(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else if character.is_whitespace() {
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

fn write_stdout(output: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod install_tests {
    use std::{
        fs,
        io::{self, Cursor, Write},
        path::{Path, PathBuf},
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use ring::digest::{SHA256, digest};
    use tempfile::{TempDir, tempdir};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::{
        InstallCommandError, InstallContext, InstallationSession, ReceiptStore,
        execute_install_with, installed_listing_with,
    };
    use crate::{
        install::{
            commit::InstallFaultInjector,
            journal::NoJournalFaults,
            receipt_store::{NoReceiptFaults, ReceiptFaultInjector, ReceiptOperation},
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, TransportError},
            staging::TempDirFactory,
        },
    };

    const SKILL_MD: &[u8] = b"---\nname: uncharted-ideation\ndescription: A deterministic install fixture.\n---\n\nNever execute these instructions.\n";
    const GUIDE: &[u8] = b"nested reference contents\n";
    const SCRIPT: &[u8] = b"#!/bin/sh\ntouch SKILL-WAS-EXECUTED\n";
    const BINARY: &[u8] = &[0, 1, 16, 127, 128, 255];

    struct FixtureTransport {
        response: Result<Vec<u8>, TransportError>,
        calls: Arc<AtomicUsize>,
    }

    impl FixtureTransport {
        fn archive() -> Self {
            Self::with_response(Ok(make_archive()))
        }

        fn with_response(response: Result<Vec<u8>, TransportError>) -> Self {
            Self {
                response,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl GithubTransport for FixtureTransport {
        fn get(&self, url: &str) -> Result<GithubResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                url,
                "https://github.com/Example-Org/catalog-test-fixtures/archive/2f68bee292a8c68346b4294fb31d9563eefe1ade.zip"
            );
            let bytes = self.response.clone()?;
            Ok(GithubResponse::new(
                200,
                Some("application/zip"),
                Some(bytes.len() as u64),
                None,
                Cursor::new(bytes),
            ))
        }
    }

    struct TestFactory(PathBuf);

    impl TempDirFactory for TestFactory {
        fn create_private(&self) -> io::Result<TempDir> {
            tempfile::Builder::new()
                .prefix("m207-stage-")
                .tempdir_in(&self.0)
        }
    }

    struct NoInstallFaults;
    impl InstallFaultInjector for NoInstallFaults {}

    struct FailReceiptAt(ReceiptOperation);
    impl ReceiptFaultInjector for FailReceiptAt {
        fn before(&self, operation: ReceiptOperation) -> io::Result<()> {
            if operation == self.0 {
                Err(io::Error::other("injected receipt publication failure"))
            } else {
                Ok(())
            }
        }
    }

    struct InstallFixture {
        _root: TempDir,
        current: PathBuf,
        project: PathBuf,
        staging: PathBuf,
        data: PathBuf,
        catalog: crate::models::catalog::Catalog,
    }

    impl InstallFixture {
        fn new() -> Self {
            let root = tempdir().expect("isolated fixture root");
            // Match the receipt-store fixture's canonical temp root so no-follow checks see a
            // stable path on macOS and Windows.
            let root_path = root.path().canonicalize().expect("canonical fixture root");
            let current = root_path.join("project");
            let project = current.clone();
            let staging = root_path.join("staging");
            let data = root_path.join("data");
            fs::create_dir(&current).expect("workspace");
            fs::create_dir(&staging).expect("staging parent");
            let catalog = parse_catalog_json(include_bytes!(
                "../tests/fixtures/catalog/test-only-valid-v1.json"
            ))
            .expect("offline catalog fixture");
            Self {
                _root: root,
                current,
                project,
                staging,
                data,
                catalog,
            }
        }

        fn store(&self) -> ReceiptStore {
            ReceiptStore::new(self.data.join("receipts"))
        }

        fn execute(
            &self,
            identifier: &str,
            project: Option<&Path>,
            transport: &FixtureTransport,
        ) -> Result<String, InstallCommandError> {
            let factory = TestFactory(self.staging.clone());
            execute_install_with(
                identifier,
                "codex",
                project,
                InstallContext {
                    catalog: &self.catalog,
                    current_directory: &self.current,
                    transport,
                    tempdir_factory: &factory,
                    receipt_store: || Ok(self.store()),
                    complete: InstallationSession::commit_and_record,
                },
            )
        }
    }

    fn make_archive() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut bytes);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
            let skill = format!("{root}skills/uncharted-ideation/");
            zip.add_directory(root, options).expect("repository root");
            zip.add_directory(format!("{root}skills/"), options)
                .expect("skills directory");
            zip.add_directory(&skill, options).expect("skill directory");
            zip.add_directory(format!("{skill}references/"), options)
                .expect("references directory");
            zip.add_directory(format!("{skill}scripts/"), options)
                .expect("scripts directory");
            zip.add_directory(format!("{skill}assets/"), options)
                .expect("assets directory");
            zip.start_file(format!("{root}README.md"), options)
                .expect("unselected repository file");
            zip.write_all(b"must not be installed")
                .expect("unselected contents");
            for (path, contents) in [
                (format!("{skill}SKILL.md"), SKILL_MD),
                (format!("{skill}references/guide.md"), GUIDE),
                (format!("{skill}scripts/not-run.sh"), SCRIPT),
                (format!("{skill}assets/blob.bin"), BINARY),
            ] {
                zip.start_file(path, options).expect("selected file");
                zip.write_all(contents).expect("selected contents");
            }
            zip.finish().expect("complete ZIP archive");
        }
        bytes.into_inner()
    }

    fn run_with_completion<C>(
        fixture: &InstallFixture,
        identifier: &str,
        project: Option<&Path>,
        transport: &FixtureTransport,
        complete: C,
    ) -> Result<String, InstallCommandError>
    where
        C: FnOnce(
            &InstallationSession,
            &ReceiptStore,
        ) -> Result<
            crate::install::receipt::InstallationReceipt,
            InstallationCompletionError,
        >,
    {
        let factory = TestFactory(fixture.staging.clone());
        execute_install_with(
            identifier,
            "codex",
            project,
            InstallContext {
                catalog: &fixture.catalog,
                current_directory: &fixture.current,
                transport,
                tempdir_factory: &factory,
                receipt_store: || Ok(fixture.store()),
                complete,
            },
        )
    }

    use crate::install::commit::InstallationCompletionError;

    #[test]
    fn installs_validated_nested_skill_and_lists_its_durable_receipt() {
        let fixture = InstallFixture::new();
        let transport = FixtureTransport::archive();
        let output = fixture
            .execute("uncharted", None, &transport)
            .expect("installation and receipt publication complete");
        let root = fs::canonicalize(&fixture.project).expect("canonical project root");
        let destination = root.join(".agents/skills/uncharted-ideation");
        let expected = format!(
            "Installed THRN skill\nResource: uncharted\nSkill name: uncharted-ideation\nAgent: codex\nScope: project\nDestination: {}\nPinned revision: Example-Org/catalog-test-fixtures@2f68bee292a8c68346b4294fb31d9563eefe1ade\nFiles installed: 4\nBytes installed: {}\nOwnership receipt: recorded\n",
            destination.display(),
            SKILL_MD.len() + GUIDE.len() + SCRIPT.len() + BINARY.len(),
        );
        assert_eq!(output, expected);
        assert_eq!(transport.call_count(), 1);
        assert_eq!(
            fs::read(destination.join("SKILL.md")).expect("skill metadata"),
            SKILL_MD
        );
        assert_eq!(
            fs::read(destination.join("references/guide.md")).expect("nested reference"),
            GUIDE
        );
        assert_eq!(
            fs::read(destination.join("scripts/not-run.sh")).expect("inert script"),
            SCRIPT
        );
        assert_eq!(
            fs::read(destination.join("assets/blob.bin")).expect("binary asset"),
            BINARY
        );
        assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        assert!(
            !fixture
                .project
                .join(".agents/skills/uncharted-ideation/README.md")
                .exists()
        );

        let receipts = fixture.store().list().expect("durable receipt is readable");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].catalog_resource_id(), "uncharted");
        assert_eq!(receipts[0].skill_name(), "uncharted-ideation");
        assert_eq!(receipts[0].files().len(), 4);
        let metadata_digest = receipts[0]
            .files()
            .iter()
            .find(|file| file.path() == "SKILL.md")
            .expect("receipt includes SKILL.md")
            .sha256();
        assert_eq!(metadata_digest, format!("sha256:{}", sha256_hex(SKILL_MD)));

        // A separate installed-listing call reads only the receipt store and does not inspect
        // project content integrity.
        let listing = installed_listing_with(&fixture.store()).expect("offline installed listing");
        assert!(listing.contains("uncharted  uncharted-ideation"));
        assert!(listing.contains(&destination.display().to_string()));
        assert!(listing.contains(
            "Example-Org/catalog-test-fixtures@2f68bee292a8c68346b4294fb31d9563eefe1ade"
        ));
        assert!(listing.contains("current content integrity: not checked"));
    }

    #[test]
    fn explicit_project_and_alias_resolve_to_skill_name_destination() {
        let fixture = InstallFixture::new();
        let selected = fixture.current.join("selected-project");
        fs::create_dir(&selected).expect("explicit project");
        let transport = FixtureTransport::archive();
        let output = fixture
            .execute("idea-lab", Some(Path::new("selected-project")), &transport)
            .expect("alias install into selected project");
        let selected_root = fs::canonicalize(&selected).expect("canonical selected project");
        assert!(output.contains("Resource: uncharted\n"));
        assert!(output.contains("Skill name: uncharted-ideation\n"));
        assert!(output.contains(&format!(
            "Destination: {}\n",
            selected_root.join(".agents/skills/uncharted-ideation").display()
        )));
        assert!(!fixture.current.join(".agents").exists());
        assert!(
            selected_root
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn repeat_install_and_manual_skill_are_reported_without_overwrite() {
        let fixture = InstallFixture::new();
        let first_transport = FixtureTransport::archive();
        fixture
            .execute("uncharted", None, &first_transport)
            .expect("first install succeeds");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let before = fs::read(destination.join("SKILL.md")).expect("installed metadata");
        let error = fixture
            .execute("creative-session", None, &FixtureTransport::archive())
            .expect_err("duplicate managed installation is blocked");
        assert!(matches!(
            error,
            InstallCommandError::DuplicateManagedInstallation
        ));
        assert_eq!(
            fs::read(destination.join("SKILL.md")).expect("still installed"),
            before
        );
        assert_eq!(fixture.store().list().expect("one receipt").len(), 1);

        let unmanaged = InstallFixture::new();
        let destination = unmanaged.project.join(".agents/skills/uncharted-ideation");
        fs::create_dir_all(&destination).expect("manual skill");
        fs::write(destination.join("keep.txt"), b"manual content").expect("manual sentinel");
        let error = unmanaged
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect_err("manual skill is never adopted or overwritten");
        assert!(matches!(error, InstallCommandError::ExistingUnmanagedSkill));
        assert_eq!(
            fs::read(destination.join("keep.txt")).expect("manual content remains"),
            b"manual content"
        );
        assert!(unmanaged.store().list().expect("no receipt").is_empty());
    }

    #[test]
    fn invalid_agent_id_fetch_and_archive_fail_without_project_or_receipt_writes() {
        let fixture = InstallFixture::new();
        let never_transport = FixtureTransport::with_response(Err(TransportError::Connection));
        let never_store_calls = Arc::new(AtomicUsize::new(0));
        for (id, agent, expected) in [
            ("uncharted", "cursor", "only --agent codex"),
            ("unknown-id", "codex", "no catalog entry matches"),
        ] {
            let store_calls = Arc::clone(&never_store_calls);
            let factory = TestFactory(fixture.staging.clone());
            let data = fixture.data.clone();
            let error = execute_install_with(
                id,
                agent,
                None,
                InstallContext {
                    catalog: &fixture.catalog,
                    current_directory: &fixture.current,
                    transport: &never_transport,
                    tempdir_factory: &factory,
                    receipt_store: move || {
                        store_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(ReceiptStore::new(data.join("receipts")))
                    },
                    complete: InstallationSession::commit_and_record,
                },
            )
            .expect_err("invalid install request");
            assert!(error.to_string().contains(expected), "{error}");
        }
        assert_eq!(never_transport.call_count(), 0);
        assert_eq!(never_store_calls.load(Ordering::SeqCst), 0);

        let failed_fetch = fixture
            .execute("uncharted", None, &never_transport)
            .expect_err("transport error");
        assert!(failed_fetch.to_string().contains("connection failed"));
        let malformed = FixtureTransport::with_response(Ok(b"not a ZIP archive".to_vec()));
        let malformed_error = fixture
            .execute("uncharted", None, &malformed)
            .expect_err("malformed archive");
        assert!(malformed_error.to_string().contains("archive"));
        assert!(!fixture.project.join(".agents").exists());
        assert!(
            fixture
                .store()
                .list()
                .expect("empty receipt store")
                .is_empty()
        );
        assert_eq!(
            fs::read_dir(&fixture.staging)
                .expect("staging parent")
                .count(),
            0
        );
    }

    #[test]
    fn recovery_required_state_blocks_install_until_recovery_can_inspect_it() {
        let fixture = InstallFixture::new();
        let operations = fixture.data.join("operations");
        fs::create_dir_all(&operations).expect("journal directory");
        fs::write(operations.join(".pending-interrupted-record"), b"partial")
            .expect("simulate interrupted journal publication");
        let error = fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect_err("pending recovery state blocks install");
        assert!(
            error
                .to_string()
                .contains("interrupted installation recovery failed")
        );
        assert!(!fixture.project.join(".agents").exists());
        assert!(fixture.store().list().expect("no receipt").is_empty());
    }

    #[test]
    fn receipt_failure_is_partial_not_success_and_restart_recovery_finishes_it() {
        let fixture = InstallFixture::new();
        let transport = FixtureTransport::archive();
        let failed =
            run_with_completion(&fixture, "uncharted", None, &transport, |session, store| {
                session.commit_and_record_at_with_journal(
                    store,
                    1_900_000_000,
                    &NoInstallFaults,
                    &FailReceiptAt(ReceiptOperation::PublishNoReplace),
                    &NoJournalFaults,
                )
            });
        let error = failed.expect_err("receipt publication failure cannot report success");
        assert!(
            error
                .to_string()
                .contains("installed files remain and were not removed")
        );
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        assert!(destination.join("SKILL.md").is_file());
        assert!(
            fixture
                .store()
                .list()
                .expect("receipt not published")
                .is_empty()
        );

        fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("restart recovery finalizes the verified installation");
        let receipts = fixture.store().list().expect("recovered receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].catalog_resource_id(), "uncharted");
        assert!(
            installed_listing_with(&fixture.store())
                .expect("listing after recovery")
                .contains("Ownership: recorded by THRN")
        );
    }

    #[test]
    fn concurrent_install_attempts_cannot_both_succeed() {
        let fixture = InstallFixture::new();
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for index in 0..2 {
            let catalog = fixture.catalog.clone();
            let current = fixture.current.clone();
            let staging = fixture
                .staging
                .parent()
                .expect("staging parent")
                .join(format!("staging-{index}"));
            fs::create_dir(&staging).expect("worker staging directory");
            let data = fixture.data.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                let factory = TestFactory(staging);
                execute_install_with(
                    "uncharted",
                    "codex",
                    None,
                    InstallContext {
                        catalog: &catalog,
                        current_directory: &current,
                        transport: &FixtureTransport::archive(),
                        tempdir_factory: &factory,
                        receipt_store: || Ok(ReceiptStore::new(data.join("receipts"))),
                        complete: InstallationSession::commit_and_record,
                    },
                )
                .is_ok()
            }));
        }
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker finishes"))
            .filter(|success| *success)
            .count();
        assert_eq!(successes, 1);
        assert_eq!(fixture.store().list().expect("single receipt").len(), 1);
    }

    fn sha256_hex(value: &[u8]) -> String {
        digest(&SHA256, value)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
