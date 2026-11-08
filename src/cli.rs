use std::{
    io::{self, Write},
    path::Path,
};

use clap::{Parser, Subcommand};

use crate::{
    doctor,
    error::Result,
    install::{
        check::{self, CheckFinding},
        command::{InstallCommandError, format_dry_run, prepare_install_session_in_scope},
        commit::{InstallationCompletionError, InstallationSession},
        destination::ConflictKind,
        journal::NoJournalFaults,
        plan::InstallScope,
        receipt::InstallationReceipt,
        receipt_store::{NoReceiptFaults, ReceiptStore, ReceiptStoreError},
        removal::{NoRemovalFaults, RemovalError, RemovalPlan, RemovalSummary},
        scope::{ResolvedScope, ScopeResolver},
        update::{
            NoUpdateFaults, UpdateError, UpdateFaultInjector, UpdatePreparation, UpdateSummary,
            format_dry_run as format_update_dry_run, prepare_update_in_scope,
        },
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
        /// Select project (default) or user skill scope.
        #[arg(long, value_enum, default_value = "project")]
        scope: InstallScope,
        /// Select an existing project directory (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        project: Option<std::path::PathBuf>,
        /// Print the complete plan without changing the project.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove one exactly verified THRN-managed Codex project skill.
    Remove {
        /// Exact canonical resource ID recorded in a THRN ownership receipt.
        id: String,
        /// Select the only currently supported agent (codex).
        #[arg(long, required = true, value_name = "AGENT")]
        agent: String,
        /// Select project (default) or user skill scope.
        #[arg(long, value_enum, default_value = "project")]
        scope: InstallScope,
        /// Select an existing project directory (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        project: Option<std::path::PathBuf>,
        /// Print the verified removal plan without changing the project or receipt store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Update one exactly verified THRN-managed Codex project skill to the local catalog pin.
    Update {
        /// Exact canonical resource ID recorded in a THRN ownership receipt.
        id: String,
        /// Select the only currently supported agent (codex).
        #[arg(long, required = true, value_name = "AGENT")]
        agent: String,
        /// Select project (default) or user skill scope.
        #[arg(long, value_enum, default_value = "project")]
        scope: InstallScope,
        /// Select an existing project directory (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        project: Option<std::path::PathBuf>,
        /// Print a file-level replacement plan without changing the project or receipt store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare current-project Codex installations with local catalog pins without network access.
    Check {
        /// Select project (default) or user skill scope.
        #[arg(long, value_enum, default_value = "project")]
        scope: InstallScope,
        /// Select an existing project directory (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        project: Option<std::path::PathBuf>,
        /// Select the only currently supported agent (codex).
        #[arg(long, value_name = "AGENT", value_parser = ["codex"])]
        agent: Option<String>,
    },
}

pub fn run() -> Result<()> {
    let command = Cli::try_parse()
        .map_err(crate::error::ThrnError::from)?
        .command;
    let has_user_project_conflict = matches!(
        &command,
        Command::Skills {
            command: SkillsCommand::Install {
                scope: InstallScope::User,
                project: Some(_),
                ..
            } | SkillsCommand::Remove {
                scope: InstallScope::User,
                project: Some(_),
                ..
            } | SkillsCommand::Update {
                scope: InstallScope::User,
                project: Some(_),
                ..
            } | SkillsCommand::Check {
                scope: InstallScope::User,
                project: Some(_),
                ..
            },
        }
    );
    if has_user_project_conflict {
        return Err(crate::error::ThrnError::Cli(clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "--project can only be used with --scope project",
        )));
    }
    match command {
        Command::Doctor => doctor::run(),
        Command::Skills { command } => match command {
            SkillsCommand::List { installed } => list_skills(installed),
            SkillsCommand::Search { query, refresh } => search_skills(query.as_deref(), refresh),
            SkillsCommand::Info { id } => show_skill_info(&id),
            SkillsCommand::Validate { path } => validate_skill(&path),
            SkillsCommand::Install {
                id,
                agent,
                scope,
                project,
                dry_run,
            } => install_skill(&id, &agent, scope, project.as_deref(), dry_run),
            SkillsCommand::Remove {
                id,
                agent,
                scope,
                project,
                dry_run,
            } => remove_skill(&id, &agent, scope, project.as_deref(), dry_run),
            SkillsCommand::Update {
                id,
                agent,
                scope,
                project,
                dry_run,
            } => update_skill(&id, &agent, scope, project.as_deref(), dry_run),
            SkillsCommand::Check {
                scope,
                project,
                agent: _,
            } => check_skills(scope, project.as_deref()),
        },
    }
}

fn check_skills(scope: InstallScope, project: Option<&Path>) -> Result<()> {
    let resolved_scope = resolve_scope(scope, project)?;
    let selection = CatalogService::for_system().discover()?;
    let store = ReceiptStore::for_system()?;
    let snapshot = store.inspect_scope(&resolved_scope)?;
    let findings = check::classify(&selection.catalog, snapshot.installations);
    report_cache_warning(selection.cache_warning.as_deref())?;
    write_stdout(&format_skill_check(
        &selection,
        scope,
        &snapshot.project_root,
        &findings,
    ))
}

fn resolve_scope(
    scope: InstallScope,
    project: Option<&Path>,
) -> std::result::Result<ResolvedScope, crate::install::scope::ScopeResolutionError> {
    let current_directory = if scope == InstallScope::Project {
        Some(
            std::env::current_dir()
                .map_err(|_| crate::install::scope::ScopeResolutionError::InvalidProjectRoot)?,
        )
    } else {
        None
    };
    ScopeResolver::for_system()?.resolve(scope, project, current_directory.as_deref())
}

fn format_skill_check(
    selection: &CatalogSelection,
    scope: InstallScope,
    scope_root: &Path,
    findings: &[CheckFinding],
) -> String {
    let mut output = if scope == InstallScope::Project {
        format!(
            "Offline skill update check\nProject: {}\n{}Catalog comparison uses the selected local metadata and installation receipts only; no network request was made.\n",
            safe_path_display(scope_root),
            catalog_heading(selection),
        )
    } else {
        format!(
            "Offline skill update check\nScope: user\nUser root: {}\n{}Catalog comparison uses the selected local metadata and installation receipts only; no network request was made.\n",
            safe_path_display(scope_root),
            catalog_heading(selection),
        )
    };
    if findings.is_empty() {
        output.push_str(if scope == InstallScope::Project {
            "No THRN-managed Codex project skills are recorded in this project.\n"
        } else {
            "No THRN-managed Codex user skills are recorded in this user scope.\n"
        });
        return output;
    }

    if scope == InstallScope::Project {
        output.push_str(&format!(
            "THRN-managed Codex project skills checked: {}\n",
            findings.len()
        ));
    } else {
        output.push_str(&format!(
            "THRN-managed Codex user skills checked: {}\n",
            findings.len()
        ));
    }
    for finding in findings {
        let receipt = finding.receipt();
        output.push('\n');
        output.push_str(&format!(
            "Resource: {}\nSkill name: {}\nInstalled SHA: {}\n",
            receipt.catalog_resource_id(),
            receipt.skill_name(),
            receipt.source_commit(),
        ));
        match finding.catalog_commit() {
            Some(commit) => output.push_str(&format!("Catalog SHA: {commit}\n")),
            None => output
                .push_str("Catalog SHA: unavailable (resource absent from selected catalog)\n"),
        }
        output.push_str(&format!(
            "Status: {}\n  {}\n",
            finding.status().label(),
            finding.status().explanation(),
        ));
    }
    output
}

fn remove_skill(
    identifier: &str,
    agent: &str,
    scope: InstallScope,
    project: Option<&Path>,
    dry_run: bool,
) -> Result<()> {
    let resolved_scope = resolve_scope(scope, project)?;
    let store =
        ReceiptStore::for_system().map_err(|error| RemovalError::Store(error.to_string()))?;
    let output = execute_remove_in_scope_with(
        identifier,
        agent,
        &resolved_scope,
        &store,
        dry_run,
        &NoRemovalFaults,
    )?;
    write_stdout(&output)
}

fn update_skill(
    identifier: &str,
    agent: &str,
    scope: InstallScope,
    project: Option<&Path>,
    dry_run: bool,
) -> Result<()> {
    if agent != "codex" {
        return Err(UpdateError::UnsupportedAgent.into());
    }
    let resolved_scope = resolve_scope(scope, project)?;
    let selection = CatalogService::for_system().discover()?;
    let store = ReceiptStore::for_system()?;
    let transport = crate::sources::github::UreqGithubTransport::new();
    let output = execute_update_in_scope_with(
        &selection.catalog,
        identifier,
        agent,
        &resolved_scope,
        &store,
        &transport,
        &crate::sources::staging::SystemTempDirFactory,
        dry_run,
        &NoUpdateFaults,
    )?;
    report_cache_warning(selection.cache_warning.as_deref())?;
    write_stdout(&output)
}

#[allow(clippy::too_many_arguments)]
fn execute_update_in_scope_with<T: crate::sources::github::GithubTransport + ?Sized>(
    catalog: &crate::models::catalog::Catalog,
    identifier: &str,
    agent: &str,
    scope: &ResolvedScope,
    store: &ReceiptStore,
    transport: &T,
    tempdir_factory: &dyn crate::sources::staging::TempDirFactory,
    dry_run: bool,
    faults: &dyn UpdateFaultInjector,
) -> std::result::Result<String, UpdateError> {
    match prepare_update_in_scope(
        catalog,
        identifier,
        agent,
        scope,
        store,
        transport,
        tempdir_factory,
    )? {
        UpdatePreparation::AlreadyUpToDate(receipt) => {
            Ok(format_update_already_up_to_date(&receipt))
        }
        UpdatePreparation::Ready(plan) if dry_run => Ok(format_update_dry_run(&plan)),
        UpdatePreparation::Ready(plan) => {
            let summary = store.update(&plan, faults)?;
            Ok(format_update_success(&summary))
        }
    }
}

pub(crate) fn format_update_already_up_to_date(receipt: &InstallationReceipt) -> String {
    if receipt.scope() == "project" {
        return format!(
            "Already up to date\nResource: {}\nSkill name: {}\nDestination: {}\nPinned revision: {}/{}@{}\nNo project files or ownership receipt were modified.\n",
            receipt.catalog_resource_id(),
            receipt.skill_name(),
            safe_path_display(&receipt.destination_path()),
            receipt.source_owner(),
            receipt.source_repository(),
            receipt.source_commit(),
        );
    }
    format!(
        "Already up to date\nResource: {}\nSkill name: {}\nScope: {}\nDestination: {}\nPinned revision: {}/{}@{}\nNo scope files or ownership receipt were modified.\n",
        receipt.catalog_resource_id(),
        receipt.skill_name(),
        receipt.scope(),
        safe_path_display(&receipt.destination_path()),
        receipt.source_owner(),
        receipt.source_repository(),
        receipt.source_commit(),
    )
}

pub(crate) fn format_update_success(summary: &UpdateSummary) -> String {
    format!(
        "Updated THRN skill\nResource: {}\nSkill name: {}\nAgent: codex\nScope: {}\nDestination: {}\nPrevious pinned revision: {}\nNew pinned revision: {}\nFiles updated: {}\nBytes installed: {}\nOwnership receipt: updated and verified\n",
        summary.resource_id(),
        summary.skill_name(),
        summary.scope(),
        safe_path_display(summary.destination()),
        summary.previous_revision(),
        summary.new_revision(),
        summary.file_count(),
        summary.byte_count(),
    )
}

#[cfg(test)]
fn execute_remove_with(
    identifier: &str,
    agent: &str,
    project: Option<&Path>,
    current_directory: &Path,
    store: &ReceiptStore,
    dry_run: bool,
    faults: &dyn crate::install::removal::RemovalFaultInjector,
) -> std::result::Result<String, RemovalError> {
    if agent != "codex" {
        return Err(RemovalError::Conflict(
            "only explicit --agent codex is supported for removal".to_owned(),
        ));
    }
    if dry_run {
        let plan = store.plan_removal(identifier, project, current_directory)?;
        Ok(format_removal_dry_run(&plan))
    } else {
        let summary = store.remove_by_id(identifier, project, current_directory, faults)?;
        Ok(format_removal_success(&summary))
    }
}

fn execute_remove_in_scope_with(
    identifier: &str,
    agent: &str,
    scope: &ResolvedScope,
    store: &ReceiptStore,
    dry_run: bool,
    faults: &dyn crate::install::removal::RemovalFaultInjector,
) -> std::result::Result<String, RemovalError> {
    if agent != "codex" {
        return Err(RemovalError::Conflict(
            "only explicit --agent codex is supported for removal".to_owned(),
        ));
    }
    if dry_run {
        let plan = store.plan_removal_in_scope(identifier, scope)?;
        Ok(format_removal_dry_run(&plan))
    } else {
        let summary = store.remove_by_id_in_scope(identifier, scope, faults)?;
        Ok(format_removal_success(&summary))
    }
}

fn format_removal_success(summary: &RemovalSummary) -> String {
    let receipt = summary.receipt();
    format!(
        "Removed THRN skill\nResource: {}\nSkill name: {}\nAgent: codex\nScope: {}\nDestination: {}\nFiles removed: {}\nBytes removed: {}\nOwnership receipt: retired\n",
        receipt.catalog_resource_id(),
        receipt.skill_name(),
        receipt.scope(),
        safe_path_display(&receipt.destination_path()),
        summary.files_removed(),
        summary.bytes_removed(),
    )
}

fn format_removal_dry_run(plan: &RemovalPlan) -> String {
    let receipt = plan.receipt();
    let mut output = format!(
        "Verified THRN skill removal plan\nResource: {}\nSkill name: {}\nAgent: codex\nScope: {}\nDestination: {}\nFiles:\n",
        receipt.catalog_resource_id(),
        receipt.skill_name(),
        receipt.scope(),
        safe_path_display(&receipt.destination_path()),
    );
    for file in plan.files() {
        output.push_str(&format!("- {} ({})\n", file.path(), file.byte_count()));
    }
    output.push_str(&format!(
        "Files to remove: {}\nBytes to remove: {}\nVerification: complete receipt and current content match\n{} and receipt store unchanged.\n",
        plan.file_count(),
        plan.byte_count(),
        if receipt.scope() == "project" { "Project" } else { "User scope" },
    ));
    output
}

fn install_skill(
    identifier: &str,
    agent: &str,
    scope: InstallScope,
    project: Option<&Path>,
    dry_run: bool,
) -> Result<()> {
    if agent != "codex" {
        return Err(InstallCommandError::UnsupportedAgent.into());
    }
    let resolved_scope = resolve_scope(scope, project)?;
    let selection = CatalogService::for_system().discover()?;
    let transport = crate::sources::github::UreqGithubTransport::new();
    let output = if dry_run {
        let session = prepare_install_session_in_scope(
            &selection.catalog,
            identifier,
            agent,
            &resolved_scope,
            &transport,
            &crate::sources::staging::SystemTempDirFactory,
        )?;
        format_dry_run(session.plan())?
    } else {
        execute_install_in_scope_with(
            identifier,
            agent,
            &resolved_scope,
            InstallContext {
                catalog: &selection.catalog,
                _current_directory: resolved_scope.root(),
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
    _current_directory: &'a Path,
    transport: &'a T,
    tempdir_factory: &'a dyn crate::sources::staging::TempDirFactory,
    receipt_store: S,
    complete: C,
}

#[cfg(test)]
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

    let session = crate::install::command::prepare_install_session(
        context.catalog,
        identifier,
        agent,
        project,
        context._current_directory,
        context.transport,
        context.tempdir_factory,
    )?;
    finish_install_session(session, context.receipt_store, context.complete)
}

fn execute_install_in_scope_with<T, S, C>(
    identifier: &str,
    agent: &str,
    scope: &ResolvedScope,
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
    let session = prepare_install_session_in_scope(
        context.catalog,
        identifier,
        agent,
        scope,
        context.transport,
        context.tempdir_factory,
    )?;
    finish_install_session(session, context.receipt_store, context.complete)
}

fn finish_install_session<S, C>(
    session: InstallationSession,
    receipt_store: S,
    complete: C,
) -> std::result::Result<String, InstallCommandError>
where
    S: FnOnce() -> std::result::Result<ReceiptStore, ReceiptStoreError>,
    C: FnOnce(
        &InstallationSession,
        &ReceiptStore,
    ) -> std::result::Result<InstallationReceipt, InstallationCompletionError>,
{
    if !session.plan().conflicts().is_empty() {
        // A read-only receipt lookup lets the CLI distinguish an existing THRN-owned install
        // from a manually installed directory. Reconcile a valid interrupted operation first;
        // the unchanged destination will then be recognized as a managed duplicate.
        let store = receipt_store().map_err(InstallCommandError::ReceiptStore)?;
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

    let store = receipt_store().map_err(InstallCommandError::ReceiptStore)?;
    let receipt = complete(&session, &store)?;
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
        let destination_label = if receipt.scope() == "project" {
            "Project destination"
        } else {
            "User destination"
        };
        output.push_str(&format!(
            "  {destination_label}: {}\n",
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
        process::{Child, Command, ExitStatus},
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
        execute_install_in_scope_with, execute_install_with, execute_remove_in_scope_with,
        execute_remove_with, installed_listing_with,
    };
    use crate::{
        install::{
            commit::InstallFaultInjector,
            journal::NoJournalFaults,
            plan::InstallScope,
            receipt_store::{NoReceiptFaults, ReceiptFaultInjector, ReceiptOperation},
            removal::{NoRemovalFaults, RemovalFaultInjector, RemovalOperation},
            scope::ResolvedScope,
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, TransportError},
            staging::TempDirFactory,
        },
    };

    #[cfg(windows)]
    use crate::install::{
        destination::{ConflictKind, DestinationError, inspect_destination},
        windows_test_support::create_junction,
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
                    _current_directory: &self.current,
                    transport,
                    tempdir_factory: &factory,
                    receipt_store: || Ok(self.store()),
                    complete: InstallationSession::commit_and_record,
                },
            )
        }

        fn execute_in_scope(
            &self,
            identifier: &str,
            scope: &ResolvedScope,
            transport: &FixtureTransport,
        ) -> Result<String, InstallCommandError> {
            let factory = TestFactory(self.staging.clone());
            execute_install_in_scope_with(
                identifier,
                "codex",
                scope,
                InstallContext {
                    catalog: &self.catalog,
                    _current_directory: &self.current,
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
                _current_directory: &fixture.current,
                transport,
                tempdir_factory: &factory,
                receipt_store: || Ok(fixture.store()),
                complete,
            },
        )
    }

    use crate::install::commit::InstallationCompletionError;

    struct FailRemovalBefore(RemovalOperation);

    impl RemovalFaultInjector for FailRemovalBefore {
        fn before(&self, operation: RemovalOperation) -> io::Result<()> {
            if operation == self.0 {
                Err(io::Error::other("injected removal failure"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FaultTiming {
        Before,
        After,
    }

    struct FailRemovalBoundary {
        operation: RemovalOperation,
        timing: FaultTiming,
        fired: std::sync::atomic::AtomicBool,
    }

    impl FailRemovalBoundary {
        fn check(&self, operation: RemovalOperation, timing: FaultTiming) -> io::Result<()> {
            if operation == self.operation
                && timing == self.timing
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                Err(io::Error::other("injected removal boundary failure"))
            } else {
                Ok(())
            }
        }
    }

    impl RemovalFaultInjector for FailRemovalBoundary {
        fn before(&self, operation: RemovalOperation) -> io::Result<()> {
            self.check(operation, FaultTiming::Before)
        }

        fn after(&self, operation: RemovalOperation) -> io::Result<()> {
            self.check(operation, FaultTiming::After)
        }
    }

    struct ExitAfterRemoval(RemovalOperation);

    impl RemovalFaultInjector for ExitAfterRemoval {
        fn after(&self, operation: RemovalOperation) -> io::Result<()> {
            if operation == self.0 {
                std::process::exit(89);
            }
            Ok(())
        }
    }

    struct PauseBeforeRemovalRename {
        entered: PathBuf,
        release: PathBuf,
    }

    impl RemovalFaultInjector for PauseBeforeRemovalRename {
        fn before(&self, operation: RemovalOperation) -> io::Result<()> {
            if operation == RemovalOperation::RenameToQuarantine {
                fs::write(&self.entered, b"entered rename boundary")?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while !self.release.exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting for the removal test release signal",
                        ));
                    }
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            Ok(())
        }
    }

    fn wait_for_child(child: &mut Child, timeout: std::time::Duration) -> io::Result<ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for a concurrency-test subprocess",
                ));
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn release_removal(path: &Path) {
        let _ = fs::write(path, b"release removal test subprocess");
    }

    struct ReleaseRemovalOnDrop(PathBuf);

    impl Drop for ReleaseRemovalOnDrop {
        fn drop(&mut self) {
            release_removal(&self.0);
        }
    }

    struct SubstituteAfterRemovalPlanning {
        destination: PathBuf,
        displaced: PathBuf,
        substituted: std::sync::Mutex<bool>,
    }

    impl RemovalFaultInjector for SubstituteAfterRemovalPlanning {
        fn before(&self, operation: RemovalOperation) -> io::Result<()> {
            if operation != RemovalOperation::Revalidate {
                return Ok(());
            }
            let mut substituted = self
                .substituted
                .lock()
                .map_err(|_| io::Error::other("substitution test lock"))?;
            if *substituted {
                return Ok(());
            }
            fs::rename(&self.destination, &self.displaced)?;
            copy_fixture_tree(&self.displaced, &self.destination)?;
            *substituted = true;
            Ok(())
        }
    }

    fn copy_fixture_tree(source: &Path, destination: &Path) -> io::Result<()> {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                copy_fixture_tree(&entry.path(), &target)?;
            } else if metadata.is_file() {
                fs::copy(entry.path(), target)?;
            } else {
                return Err(io::Error::other("unexpected fixture object"));
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn assert_junction_rejected_by_inspection_and_orchestration(
        fixture: &InstallFixture,
        junction: &Path,
        expected_conflict: ConflictKind,
    ) {
        let snapshot = inspect_destination(None, &fixture.current, "uncharted-ideation")
            .expect("destination inspection must return the junction conflict");
        assert!(
            snapshot
                .conflicts()
                .iter()
                .any(|conflict| conflict.path() == junction && conflict.kind() == expected_conflict),
            "inspection did not reject junction {junction:?}: {:?}",
            snapshot.conflicts()
        );

        let transport = FixtureTransport::archive();
        let error = fixture
            .execute("uncharted", None, &transport)
            .expect_err("junction destination must not produce installation success");
        assert!(
            matches!(&error, InstallCommandError::InstallConflicts { .. }),
            "unexpected orchestration result: {error:?}"
        );
        assert!(
            !error.to_string().contains("Installed THRN skill"),
            "failure must not report installation success"
        );
        assert_eq!(transport.call_count(), 1, "fake GitHub source is exercised");
        assert!(
            fixture.store().list().expect("receipt listing").is_empty(),
            "failed junction install must not create an ownership receipt"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_agents_junction_is_rejected_by_inspection_and_install_orchestration() {
        let fixture = InstallFixture::new();
        let external = fixture._root.path().join("external-agents");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("keep.txt"), b"external sentinel").expect("external sentinel");
        let junction = fixture.project.join(".agents");
        create_junction(&junction, &external);

        assert_junction_rejected_by_inspection_and_orchestration(
            &fixture,
            &junction,
            ConflictKind::UnsafeDestinationComponent,
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("sentinel remains"),
            b"external sentinel"
        );
        assert_eq!(
            fs::read_dir(&external).expect("external entries").count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_skills_junction_is_rejected_by_inspection_and_install_orchestration() {
        let fixture = InstallFixture::new();
        let external = fixture._root.path().join("external-skills");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("keep.txt"), b"external sentinel").expect("external sentinel");
        fs::create_dir(fixture.project.join(".agents")).expect("real agents directory");
        let junction = fixture.project.join(".agents/skills");
        create_junction(&junction, &external);

        assert_junction_rejected_by_inspection_and_orchestration(
            &fixture,
            &junction,
            ConflictKind::UnsafeDestinationComponent,
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("sentinel remains"),
            b"external sentinel"
        );
        assert_eq!(
            fs::read_dir(&external).expect("external entries").count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_skill_destination_junction_is_rejected_without_adopting_external_content() {
        let fixture = InstallFixture::new();
        let external = fixture._root.path().join("external-skill");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("SKILL.md"), b"external skill metadata")
            .expect("external metadata");
        fs::write(external.join("keep.txt"), b"external sentinel").expect("external sentinel");
        fs::create_dir_all(fixture.project.join(".agents/skills")).expect("real skills directory");
        let junction = fixture.project.join(".agents/skills/uncharted-ideation");
        create_junction(&junction, &external);

        assert_junction_rejected_by_inspection_and_orchestration(
            &fixture,
            &junction,
            ConflictKind::ExistingSymlink,
        );
        assert_eq!(
            fs::read(external.join("SKILL.md")).expect("external metadata remains"),
            b"external skill metadata"
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("sentinel remains"),
            b"external sentinel"
        );
        assert_eq!(
            fs::read_dir(&external).expect("external entries").count(),
            2
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_project_junction_is_rejected_without_writing_into_its_target() {
        let fixture = InstallFixture::new();
        let external = fixture._root.path().join("external-project");
        fs::create_dir(&external).expect("external project");
        fs::write(external.join("keep.txt"), b"external project sentinel")
            .expect("external sentinel");
        let junction = fixture._root.path().join("project-junction");
        create_junction(&junction, &external);

        assert!(matches!(
            inspect_destination(Some(&junction), &fixture.current, "uncharted-ideation"),
            Err(DestinationError::ProjectRootSymlink { .. })
        ));
        let transport = FixtureTransport::archive();
        let error = fixture
            .execute("uncharted", Some(&junction), &transport)
            .expect_err("junction project root must fail installation");
        assert!(
            matches!(
                &error,
                InstallCommandError::Destination(DestinationError::ProjectRootSymlink { .. })
            ),
            "unexpected project-root result: {error:?}"
        );
        assert_eq!(transport.call_count(), 1);
        assert_eq!(
            fs::read(external.join("keep.txt")).expect("external sentinel remains"),
            b"external project sentinel"
        );
        assert_eq!(
            fs::read_dir(&external).expect("external entries").count(),
            1
        );
        assert!(fixture.store().list().expect("receipt listing").is_empty());
    }

    #[test]
    fn removal_dry_run_verifies_nested_binary_and_inert_script_files_read_only() {
        let fixture = InstallFixture::new();
        let transport = FixtureTransport::archive();
        fixture
            .execute("uncharted", None, &transport)
            .expect("install fixture");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        fs::write(fixture.project.join("keep.txt"), b"unrelated project data")
            .expect("unrelated file");

        let plan = fixture
            .store()
            .plan_removal("uncharted", Some(&fixture.project), &fixture.current)
            .expect("complete receipt and current inventory verify");
        let output = super::format_removal_dry_run(&plan);
        assert!(output.contains("Resource: uncharted\n"));
        assert!(output.contains("Skill name: uncharted-ideation\n"));
        assert!(output.contains("- references/guide.md ("));
        assert!(output.contains("- assets/blob.bin (6)"));
        assert!(output.contains("- scripts/not-run.sh ("));
        assert!(output.contains("Verification: complete receipt and current content match"));
        assert!(output.contains("Project and receipt store unchanged."));
        assert_eq!(transport.call_count(), 1, "removal makes no source request");
        assert_eq!(
            fs::read(destination.join("assets/blob.bin")).unwrap(),
            BINARY
        );
        assert_eq!(fixture.store().list().expect("receipt remains").len(), 1);
        assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        assert!(!fixture.data.join("removals").exists());
    }

    #[test]
    fn removal_success_uses_canonical_id_and_skill_name_then_retires_one_receipt() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let root = fs::canonicalize(&fixture.project).expect("canonical project root");
        let destination = root.join(".agents/skills/uncharted-ideation");
        fs::write(root.join("keep.txt"), b"unrelated project data").expect("keep file");
        let expected = format!(
            "Removed THRN skill\nResource: uncharted\nSkill name: uncharted-ideation\nAgent: codex\nScope: project\nDestination: {}\nFiles removed: 4\nBytes removed: {}\nOwnership receipt: retired\n",
            destination.display(),
            SKILL_MD.len() + GUIDE.len() + SCRIPT.len() + BINARY.len(),
        );

        let output = execute_remove_with(
            "uncharted",
            "codex",
            Some(&fixture.project),
            &fixture.current,
            &fixture.store(),
            false,
            &NoRemovalFaults,
        )
        .expect("removal completes durably");
        assert_eq!(output, expected);
        assert!(!destination.exists());
        assert!(root.join(".agents/skills").is_dir(), "parents are retained");
        assert_eq!(
            fs::read_dir(root.join(".agents/skills")).unwrap().count(),
            0
        );
        assert_eq!(
            fs::read(root.join("keep.txt")).unwrap(),
            b"unrelated project data"
        );
        assert!(fixture.store().list().expect("receipt retired").is_empty());
        assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        assert!(fixture.data.join("removals").is_dir());
        assert_eq!(
            fs::read_dir(fixture.data.join("removals")).unwrap().count(),
            0
        );
    }

    #[test]
    fn removal_of_one_project_keeps_same_skill_in_another_project() {
        let fixture = InstallFixture::new();
        let second = fixture.current.join("second-project");
        fs::create_dir(&second).expect("second project");
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install in first project");
        fixture
            .execute(
                "uncharted",
                Some(Path::new("second-project")),
                &FixtureTransport::archive(),
            )
            .expect("install in second project");

        fixture
            .store()
            .remove_by_id(
                "uncharted",
                Some(&fixture.project),
                &fixture.current,
                &NoRemovalFaults,
            )
            .expect("remove selected project only");
        assert!(
            !fixture
                .project
                .join(".agents/skills/uncharted-ideation")
                .exists()
        );
        assert!(
            second
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
        let remaining = fixture
            .store()
            .list()
            .expect("other project receipt remains");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].root_path(), fs::canonicalize(&second).unwrap());
    }

    #[test]
    fn modified_missing_extra_and_unmanaged_installations_are_conflicts() {
        for mutation in [
            "modified",
            "missing",
            "extra",
            "extra-directory",
            "unmanaged",
        ] {
            let fixture = InstallFixture::new();
            let destination = fixture.project.join(".agents/skills/uncharted-ideation");
            if mutation == "unmanaged" {
                fs::create_dir_all(&destination).expect("manual skill directory");
                fs::write(destination.join("SKILL.md"), SKILL_MD).expect("manual metadata");
            } else {
                fixture
                    .execute("uncharted", None, &FixtureTransport::archive())
                    .expect("install fixture");
                match mutation {
                    "modified" => {
                        let mut changed = SCRIPT.to_vec();
                        changed[0] ^= 1;
                        fs::write(destination.join("scripts/not-run.sh"), changed)
                            .expect("same-length mutation");
                    }
                    "missing" => fs::remove_file(destination.join("assets/blob.bin"))
                        .expect("remove expected file"),
                    "extra" => {
                        fs::write(destination.join("user.txt"), b"user data").expect("extra file")
                    }
                    "extra-directory" => {
                        fs::create_dir(destination.join("user-dir")).expect("extra directory")
                    }
                    _ => unreachable!(),
                }
            }
            let before = fs::read_dir(&destination)
                .expect("destination remains")
                .count();
            let result =
                fixture
                    .store()
                    .plan_removal("uncharted", Some(&fixture.project), &fixture.current);
            assert!(result.is_err(), "mutation={mutation}");
            assert!(destination.is_dir(), "mutation={mutation}");
            assert_eq!(
                fs::read_dir(&destination)
                    .expect("destination preserved")
                    .count(),
                before,
                "mutation={mutation}"
            );
            if mutation != "unmanaged" {
                assert_eq!(fixture.store().list().expect("receipt preserved").len(), 1);
            } else {
                assert!(fixture.store().list().expect("no receipt").is_empty());
            }
            assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        }
    }

    #[test]
    fn missing_corrupt_and_conflicting_receipts_never_authorize_removal() {
        for scenario in ["missing", "corrupt", "conflicting"] {
            let fixture = InstallFixture::new();
            fixture
                .execute("uncharted", None, &FixtureTransport::archive())
                .expect("install fixture");
            let receipt_dir = fixture.data.join("receipts");
            let receipt_path = fs::read_dir(&receipt_dir)
                .expect("receipt directory")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
                .expect("receipt file");
            match scenario {
                "missing" => fs::remove_file(&receipt_path).expect("remove receipt fixture"),
                "corrupt" => fs::write(&receipt_path, b"{broken receipt").expect("corrupt receipt"),
                "conflicting" => {
                    let mut json = fs::read_to_string(&receipt_path).expect("receipt JSON");
                    let modified = json.replace(
                        "\"catalogResourceId\":\"uncharted\"",
                        "\"catalogResourceId\":\"creative-lab\"",
                    );
                    assert_ne!(modified, json, "fixture resource id field is present");
                    json = modified;
                    let root = fs::canonicalize(&fixture.project).expect("canonical project root");
                    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
                    digest.update(b"THRN-RECEIPT-IDENTITY-V1\0");
                    for field in [root.to_str().unwrap().as_bytes(), b"codex", b"creative-lab"] {
                        digest.update(&(field.len() as u64).to_be_bytes());
                        digest.update(field);
                    }
                    let hash = digest.finish();
                    let mut hex = String::new();
                    for byte in hash.as_ref() {
                        use std::fmt::Write as _;
                        write!(hex, "{byte:02x}").unwrap();
                    }
                    fs::write(receipt_dir.join(format!("v1-{hex}.json")), json)
                        .expect("publish conflicting valid claim");
                }
                _ => unreachable!(),
            }
            let destination = fixture.project.join(".agents/skills/uncharted-ideation");
            let result = fixture.store().remove_by_id(
                "uncharted",
                Some(&fixture.project),
                &fixture.current,
                &NoRemovalFaults,
            );
            assert!(result.is_err(), "scenario={scenario}");
            assert!(
                destination.join("SKILL.md").is_file(),
                "scenario={scenario}"
            );
            assert_eq!(
                fs::read(destination.join("assets/blob.bin")).unwrap(),
                BINARY,
                "scenario={scenario}"
            );
            assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        }
    }

    #[test]
    fn conflicting_installation_identity_in_removal_journal_fails_closed() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let unrelated = fixture.project.join("unrelated.txt");
        fs::write(&unrelated, b"preserve user data").expect("unrelated sentinel");

        let result = fixture.store().remove_by_id(
            "uncharted",
            Some(&fixture.project),
            &fixture.current,
            &FailRemovalBefore(RemovalOperation::RenameToQuarantine),
        );
        assert!(
            result.is_err(),
            "injected pre-rename failure leaves a journal"
        );
        assert!(destination.join("SKILL.md").is_file());

        let journal_directory = fixture.data.join("removals");
        let mut snapshots = fs::read_dir(&journal_directory)
            .expect("removal journal directory")
            .map(|entry| entry.expect("journal entry").path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        snapshots.sort();
        let latest = snapshots.last().expect("published removal snapshot");
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(latest).expect("journal snapshot"))
                .expect("valid journal JSON");
        let object = record["destinationIdentity"]["object"]
            .as_u64()
            .expect("destination object identity");
        record["destinationIdentity"]["object"] = serde_json::json!(object.wrapping_add(1));
        fs::write(
            latest,
            serde_json::to_vec(&record).expect("serialize conflicting journal"),
        )
        .expect("write conflicting evidence fixture");

        let recovery = fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults);
        assert!(
            recovery.is_err(),
            "mismatched identity evidence fails closed"
        );
        assert!(destination.join("SKILL.md").is_file());
        assert_eq!(
            fs::read(&unrelated).expect("user data preserved"),
            b"preserve user data"
        );
        let receipt_entries = fs::read_dir(fixture.data.join("receipts"))
            .expect("receipt store remains")
            .map(|entry| entry.expect("receipt entry").path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .count();
        assert_eq!(receipt_entries, 1, "ownership receipt is preserved");
        assert!(
            journal_directory.is_dir(),
            "conflicting journal is preserved"
        );
    }

    #[test]
    fn removal_failures_at_each_journal_and_filesystem_boundary_recover_safely() {
        let operations = [
            RemovalOperation::CreateJournalStore,
            RemovalOperation::CreateJournalTemporary,
            RemovalOperation::WriteJournal,
            RemovalOperation::SyncJournalRecord,
            RemovalOperation::VerifyJournalRecord,
            RemovalOperation::PublishJournalRecord,
            RemovalOperation::SyncJournalStore,
            RemovalOperation::Revalidate,
            RemovalOperation::RenameToQuarantine,
            RemovalOperation::RemoveFile,
            RemovalOperation::RemoveDirectory,
            RemovalOperation::SyncContentDirectory,
            RemovalOperation::RetireReceipt,
            RemovalOperation::SyncReceiptDirectory,
            RemovalOperation::CleanupJournalRecord,
        ];
        for operation in operations {
            for timing in [FaultTiming::Before, FaultTiming::After] {
                let fixture = InstallFixture::new();
                fixture
                    .execute("uncharted", None, &FixtureTransport::archive())
                    .expect("install fixture");
                let sentinel = fixture.project.join("unrelated.txt");
                fs::write(&sentinel, b"preserve user data").expect("unrelated sentinel");
                let fault = FailRemovalBoundary {
                    operation,
                    timing,
                    fired: std::sync::atomic::AtomicBool::new(false),
                };
                let result = fixture.store().remove_by_id(
                    "uncharted",
                    Some(&fixture.project),
                    &fixture.current,
                    &fault,
                );
                assert!(
                    result.is_err(),
                    "injected operation={operation:?}, timing={timing:?} must not report success"
                );
                fixture
                    .store()
                    .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                    .unwrap_or_else(|error| {
                        panic!(
                            "recovery failed for operation={operation:?}, timing={timing:?}: {error}"
                        )
                    });
                let receipts = fixture
                    .store()
                    .list()
                    .expect("recovery leaves readable store");
                let destination = fixture.project.join(".agents/skills/uncharted-ideation");
                if receipts.is_empty() {
                    assert!(
                        !destination.exists(),
                        "operation={operation:?}, timing={timing:?}"
                    );
                } else {
                    assert_eq!(
                        receipts.len(),
                        1,
                        "operation={operation:?}, timing={timing:?}"
                    );
                    assert!(
                        destination.join("SKILL.md").is_file(),
                        "operation={operation:?}, timing={timing:?}"
                    );
                    assert_eq!(
                        fs::read(destination.join("assets/blob.bin")).unwrap(),
                        BINARY,
                        "operation={operation:?}, timing={timing:?}"
                    );
                }
                assert_eq!(fs::read(&sentinel).unwrap(), b"preserve user data");
                assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn removal_rejects_hardlinked_files_and_symlink_substitution() {
        use std::os::unix::fs::symlink;

        for replace_with_symlink in [false, true] {
            let fixture = InstallFixture::new();
            fixture
                .execute("uncharted", None, &FixtureTransport::archive())
                .expect("install fixture");
            let file = fixture
                .project
                .join(".agents/skills/uncharted-ideation/assets/blob.bin");
            let external = fixture._root.path().join("outside.bin");
            fs::write(&external, b"outside content").expect("outside sentinel");
            if replace_with_symlink {
                fs::remove_file(&file).expect("remove original path");
                symlink(&external, &file).expect("substitute symlink");
            } else {
                fs::remove_file(&external).expect("prepare hardlink destination");
                fs::hard_link(&file, &external).expect("create external hardlink");
            }
            assert!(
                fixture
                    .store()
                    .plan_removal("uncharted", Some(&fixture.project), &fixture.current)
                    .is_err()
            );
            let expected_external = if replace_with_symlink {
                b"outside content".as_slice()
            } else {
                BINARY
            };
            assert_eq!(
                fs::read(&external).expect("external content remains"),
                expected_external
            );
            assert_eq!(fixture.store().list().expect("receipt remains").len(), 1);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removal_rejects_case_insensitive_destination_collisions() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let collision = fixture.project.join(".agents/skills/Uncharted-Ideation");
        fs::create_dir(&collision).expect("case collision");
        fs::write(collision.join("keep.txt"), b"unrelated").expect("collision sentinel");
        assert!(
            fixture
                .store()
                .plan_removal("uncharted", Some(&fixture.project), &fixture.current)
                .is_err()
        );
        assert!(collision.join("keep.txt").is_file());
        assert!(
            fixture
                .project
                .join(".agents/skills/uncharted-ideation/SKILL.md")
                .is_file()
        );
    }

    #[cfg(windows)]
    #[test]
    fn removal_rejects_real_windows_destination_junction() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let displaced = fixture._root.path().join("displaced-skill");
        let external = fixture._root.path().join("external-skill");
        fs::create_dir(&external).expect("external directory");
        fs::write(external.join("keep.txt"), b"external sentinel").expect("sentinel");
        fs::rename(&destination, &displaced).expect("displace managed content");
        create_junction(&destination, &external);
        assert!(
            fixture
                .store()
                .plan_removal("uncharted", Some(&fixture.project), &fixture.current)
                .is_err()
        );
        assert_eq!(
            fs::read(external.join("keep.txt")).unwrap(),
            b"external sentinel"
        );
        assert!(displaced.join("SKILL.md").is_file());
        assert!(fixture.store().list().expect("receipt remains").len() == 1);
    }

    #[test]
    fn replacement_with_identical_inventory_before_removal_planning_is_rejected() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let displaced = fixture._root.path().join("original-installed-skill");
        fs::rename(&destination, &displaced).expect("preserve original installation");
        copy_fixture_tree(&displaced, &destination).expect("create byte-identical directory copy");
        assert_eq!(
            fs::read(displaced.join("SKILL.md")).unwrap(),
            fs::read(destination.join("SKILL.md")).unwrap()
        );
        assert_ne!(
            crate::install::destination::DestinationFileSystem::object_identity(
                &crate::install::destination::RealDestinationFileSystem,
                &displaced,
            )
            .unwrap(),
            crate::install::destination::DestinationFileSystem::object_identity(
                &crate::install::destination::RealDestinationFileSystem,
                &destination,
            )
            .unwrap(),
            "the replacement must have a distinct filesystem identity"
        );

        let error = fixture
            .store()
            .plan_removal("uncharted", Some(&fixture.project), &fixture.current)
            .expect_err("same files under a new directory identity are not the installation");
        assert!(
            error
                .to_string()
                .contains("installation-time receipt evidence")
        );
        assert!(displaced.join("SKILL.md").is_file());
        assert!(destination.join("SKILL.md").is_file());
        assert_eq!(fixture.store().list().expect("receipt remains").len(), 1);
    }

    #[test]
    fn substitution_after_removal_planning_is_rejected_before_thrn_mutation() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let displaced = fixture._root.path().join("displaced-installed-skill");
        let substitution = SubstituteAfterRemovalPlanning {
            destination: destination.clone(),
            displaced: displaced.clone(),
            substituted: std::sync::Mutex::new(false),
        };
        let error = fixture
            .store()
            .remove_by_id(
                "uncharted",
                Some(&fixture.project),
                &fixture.current,
                &substitution,
            )
            .expect_err("identity substitution after planning must fail closed");
        assert!(error.to_string().contains("removal may be incomplete"));
        assert!(displaced.join("SKILL.md").is_file());
        assert!(destination.join("SKILL.md").is_file());
        assert!(destination.join("scripts/not-run.sh").is_file());
        assert!(!fixture.project.join("SKILL-WAS-EXECUTED").exists());
        assert!(fixture.data.join("removals").is_dir());
    }

    #[test]
    fn incomplete_file_cleanup_recovers_idempotently_without_false_success() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let error = fixture
            .store()
            .remove_by_id(
                "uncharted",
                Some(&fixture.project),
                &fixture.current,
                &FailRemovalBefore(RemovalOperation::RemoveFile),
            )
            .expect_err("injected cleanup failure cannot report success");
        assert!(error.to_string().contains("removal may be incomplete"));
        let skills = fixture.project.join(".agents/skills");
        let quarantine = fs::read_dir(&skills)
            .expect("skill parent remains")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".thrn-removal-"))
            })
            .expect("verified installation remains quarantined");
        assert!(quarantine.join("SKILL.md").is_file());
        assert!(!skills.join("uncharted-ideation").exists());
        assert!(
            fixture
                .store()
                .list()
                .unwrap_err()
                .to_string()
                .contains("recovery is required")
        );
        fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("recovery resumes exact verified deletion");
        assert!(fixture.store().list().expect("receipt retired").is_empty());
        assert!(
            !fixture
                .project
                .join(".agents/skills/uncharted-ideation")
                .exists()
        );
        fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("second recovery is idempotent");
    }

    #[test]
    fn installation_and_removal_cannot_race_under_the_receipt_store_lock() {
        if std::env::var_os("THRN_M3_PAUSED_REMOVE_DATA").is_some() {
            let data = PathBuf::from(std::env::var_os("THRN_M3_PAUSED_REMOVE_DATA").unwrap());
            let project = PathBuf::from(std::env::var_os("THRN_M3_PAUSED_REMOVE_PROJECT").unwrap());
            let current = PathBuf::from(std::env::var_os("THRN_M3_PAUSED_REMOVE_CURRENT").unwrap());
            let entered = PathBuf::from(std::env::var_os("THRN_M3_PAUSED_REMOVE_ENTERED").unwrap());
            let release = PathBuf::from(std::env::var_os("THRN_M3_PAUSED_REMOVE_RELEASE").unwrap());
            ReceiptStore::new(data.join("receipts"))
                .remove_by_id(
                    "uncharted",
                    Some(&project),
                    &current,
                    &PauseBeforeRemovalRename { entered, release },
                )
                .expect("removal completes after the parent releases its test gate");
            return;
        }

        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("install fixture");
        let entered = fixture._root.path().join("removal-entered");
        let release = fixture._root.path().join("removal-release");
        let test_executable = std::env::current_exe().expect("test executable");
        let mut removal = Command::new(&test_executable)
            .arg("--exact")
            .arg("cli::install_tests::installation_and_removal_cannot_race_under_the_receipt_store_lock")
            .arg("--nocapture")
            .env("THRN_M3_PAUSED_REMOVE_DATA", &fixture.data)
            .env("THRN_M3_PAUSED_REMOVE_PROJECT", &fixture.project)
            .env("THRN_M3_PAUSED_REMOVE_CURRENT", &fixture.current)
            .env("THRN_M3_PAUSED_REMOVE_ENTERED", &entered)
            .env("THRN_M3_PAUSED_REMOVE_RELEASE", &release)
            .spawn()
            .expect("launch removal-lock subprocess");
        let _release_on_drop = ReleaseRemovalOnDrop(release.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !entered.exists()
            && removal
                .try_wait()
                .expect("inspect removal subprocess")
                .is_none()
            && std::time::Instant::now() < deadline
        {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        if !entered.exists() {
            release_removal(&release);
            let _ = wait_for_child(&mut removal, std::time::Duration::from_secs(5));
            panic!("removal subprocess did not reach the locked rename boundary");
        }

        let mut installation = match Command::new(&test_executable)
            .arg("--exact")
            .arg("cli::install_tests::subprocess_install_is_rejected_while_removal_holds_the_writer_lock")
            .arg("--nocapture")
            .env("THRN_M3_INSTALL_LOCK_DATA", &fixture.data)
            .env("THRN_M3_INSTALL_LOCK_PROJECT", &fixture.project)
            .env("THRN_M3_INSTALL_LOCK_CURRENT", &fixture.current)
            .env("THRN_M3_INSTALL_LOCK_STAGING", &fixture.staging)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                release_removal(&release);
                let _ = wait_for_child(&mut removal, std::time::Duration::from_secs(10));
                panic!("could not launch install-lock subprocess: {error}");
            }
        };
        let install_result = wait_for_child(&mut installation, std::time::Duration::from_secs(15));
        release_removal(&release);
        let removal_result = wait_for_child(&mut removal, std::time::Duration::from_secs(15));

        assert!(
            install_result.as_ref().is_ok_and(|status| status.success()),
            "install subprocess must report the active OS writer lock; result={install_result:?}"
        );
        assert!(
            removal_result.as_ref().is_ok_and(|status| status.success()),
            "removal subprocess must complete after release; result={removal_result:?}"
        );
        assert!(fixture.store().list().expect("receipt retired").is_empty());
        assert!(
            !fixture
                .project
                .join(".agents/skills/uncharted-ideation")
                .exists(),
            "competing install must not recreate content removed by the winner"
        );
    }

    #[test]
    fn subprocess_install_is_rejected_while_removal_holds_the_writer_lock() {
        let Some(data) = std::env::var_os("THRN_M3_INSTALL_LOCK_DATA") else {
            return;
        };
        let project = PathBuf::from(std::env::var_os("THRN_M3_INSTALL_LOCK_PROJECT").unwrap());
        let current = PathBuf::from(std::env::var_os("THRN_M3_INSTALL_LOCK_CURRENT").unwrap());
        let staging = PathBuf::from(std::env::var_os("THRN_M3_INSTALL_LOCK_STAGING").unwrap());
        let catalog = parse_catalog_json(include_bytes!(
            "../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("offline catalog fixture");
        let factory = TestFactory(staging);
        let result = execute_install_with(
            "uncharted",
            "codex",
            Some(&project),
            InstallContext {
                catalog: &catalog,
                _current_directory: &current,
                transport: &FixtureTransport::archive(),
                tempdir_factory: &factory,
                receipt_store: || Ok(ReceiptStore::new(PathBuf::from(&data).join("receipts"))),
                complete: InstallationSession::commit_and_record,
            },
        );
        let error = result.expect_err("active removal must exclude installation");
        assert!(
            error
                .to_string()
                .contains("busy or left by an interrupted write"),
            "{error}"
        );
    }

    #[test]
    fn subprocess_crash_after_cleanup_intent_recovers_removed_skill() {
        if std::env::var_os("THRN_M3_CRASH_DURING_REMOVAL").is_some() {
            let project = PathBuf::from(std::env::var_os("THRN_M3_PROJECT").expect("project"));
            let data = PathBuf::from(std::env::var_os("THRN_M3_DATA").expect("data"));
            let store = ReceiptStore::new(data.join("receipts"));
            let operation = match std::env::var("THRN_M3_CRASH_OPERATION").as_deref() {
                Ok("rename") => RemovalOperation::RenameToQuarantine,
                Ok("file") => RemovalOperation::RemoveFile,
                Ok("receipt") => RemovalOperation::RetireReceipt,
                _ => panic!("unknown crash boundary"),
            };
            if std::env::var_os("THRN_M3_SCOPE").is_some() {
                let home = PathBuf::from(std::env::var_os("THRN_M3_HOME").expect("home"));
                let scope = ResolvedScope::for_test(InstallScope::User, home);
                let _ =
                    store.remove_by_id_in_scope("uncharted", &scope, &ExitAfterRemoval(operation));
            } else {
                let _ =
                    store.remove_by_id("uncharted", None, &project, &ExitAfterRemoval(operation));
            }
            panic!("child process should have terminated at the crash boundary");
        }

        for boundary in ["rename", "file", "receipt"] {
            let fixture = InstallFixture::new();
            fixture
                .execute("uncharted", None, &FixtureTransport::archive())
                .expect("install fixture");
            let status = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("cli::install_tests::subprocess_crash_after_cleanup_intent_recovers_removed_skill")
                .arg("--nocapture")
                .env("THRN_M3_CRASH_DURING_REMOVAL", "1")
                .env("THRN_M3_CRASH_OPERATION", boundary)
                .env("THRN_M3_PROJECT", &fixture.project)
                .env("THRN_M3_DATA", &fixture.data)
                .status()
                .expect("launch removal crash simulation");
            assert_eq!(status.code(), Some(89), "boundary={boundary}");
            fixture
                .store()
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recover process interruption");
            assert!(fixture.store().list().expect("receipt retired").is_empty());
            assert!(
                !fixture
                    .project
                    .join(".agents/skills/uncharted-ideation")
                    .exists()
            );
            fixture
                .store()
                .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
                .expect("recovery repeats safely");
        }
    }

    #[test]
    fn subprocess_user_removal_crash_recovers_without_touching_project_installation() {
        let fixture = InstallFixture::new();
        fixture
            .execute("uncharted", None, &FixtureTransport::archive())
            .expect("project installation");
        let home = fixture
            ._root
            .path()
            .canonicalize()
            .expect("fixture root")
            .join("home");
        fs::create_dir(&home).expect("user home");
        let user_scope = ResolvedScope::for_test(InstallScope::User, home.clone());
        fixture
            .execute_in_scope("uncharted", &user_scope, &FixtureTransport::archive())
            .expect("user installation");
        let project_skill = fixture
            .project
            .join(".agents/skills/uncharted-ideation/SKILL.md");
        let project_bytes = fs::read(&project_skill).expect("project skill bytes");
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("cli::install_tests::subprocess_crash_after_cleanup_intent_recovers_removed_skill")
            .arg("--nocapture")
            .env("THRN_M3_CRASH_DURING_REMOVAL", "1")
            .env("THRN_M3_CRASH_OPERATION", "file")
            .env("THRN_M3_SCOPE", "user")
            .env("THRN_M3_PROJECT", &fixture.project)
            .env("THRN_M3_HOME", &home)
            .env("THRN_M3_DATA", &fixture.data)
            .status()
            .expect("launch user-removal crash simulation");
        assert_eq!(status.code(), Some(89));
        assert!(fixture.store().list().is_err());
        fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("recover user removal interruption");
        let receipts = fixture.store().list().expect("project receipt remains");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].scope(), "project");
        assert!(project_skill.is_file());
        assert_eq!(
            fs::read(&project_skill).expect("project content preserved"),
            project_bytes
        );
        assert!(!home.join(".agents/skills/uncharted-ideation").exists());
    }

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
    fn project_and_user_scope_install_independently_and_user_remove_preserves_unmanaged_skill() {
        let fixture = InstallFixture::new();
        let project_transport = FixtureTransport::archive();
        fixture
            .execute("uncharted", None, &project_transport)
            .expect("project install");

        let root = fixture.project.parent().expect("fixture root");
        let home = root.join("home");
        fs::create_dir(&home).expect("isolated user home");
        let unmanaged = home.join(".agents/skills/manual-skill");
        fs::create_dir_all(&unmanaged).expect("unmanaged user skill");
        fs::write(unmanaged.join("SKILL.md"), b"keep this unmanaged skill\n")
            .expect("unmanaged content");

        let user_scope = ResolvedScope::for_test(InstallScope::User, home.clone());
        let user_transport = FixtureTransport::archive();
        let factory = TestFactory(fixture.staging.clone());
        let output = execute_install_in_scope_with(
            "uncharted",
            "codex",
            &user_scope,
            InstallContext {
                catalog: &fixture.catalog,
                _current_directory: &fixture.current,
                transport: &user_transport,
                tempdir_factory: &factory,
                receipt_store: || Ok(fixture.store()),
                complete: InstallationSession::commit_and_record,
            },
        )
        .expect("independent user install");
        let user_destination = home.join(".agents/skills/uncharted-ideation");
        let project_destination = fixture.project.join(".agents/skills/uncharted-ideation");
        assert!(output.contains("Scope: user"));
        assert!(output.contains(&user_destination.display().to_string()));
        assert!(user_destination.join("SKILL.md").is_file());
        assert!(project_destination.join("SKILL.md").is_file());
        assert_eq!(
            fs::read(unmanaged.join("SKILL.md")).expect("unmanaged file preserved"),
            b"keep this unmanaged skill\n"
        );

        let receipts = fixture.store().list().expect("both receipts list offline");
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].catalog_resource_id(), "uncharted");
        assert_ne!(
            receipts[0].receipt_filename().expect("receipt key"),
            receipts[1].receipt_filename().expect("receipt key")
        );
        assert!(receipts.iter().any(|receipt| receipt.scope() == "project"));
        assert!(receipts.iter().any(|receipt| receipt.scope() == "user"));
        let listing = installed_listing_with(&fixture.store()).expect("installed listing");
        assert!(listing.contains("Scope: project"));
        assert!(listing.contains("Scope: user"));
        assert!(listing.contains("User destination:"));

        let user_inspection = fixture
            .store()
            .inspect_scope(&user_scope)
            .expect("read-only user inspection");
        assert_eq!(user_inspection.installations.len(), 1);
        assert_eq!(user_inspection.installations[0].receipt.scope(), "user");
        let project_scope = ResolvedScope::for_test(InstallScope::Project, fixture.project.clone());
        let project_inspection = fixture
            .store()
            .inspect_scope(&project_scope)
            .expect("read-only project inspection");
        assert_eq!(project_inspection.installations.len(), 1);
        assert_eq!(
            project_inspection.installations[0].receipt.scope(),
            "project"
        );

        let removal = execute_remove_in_scope_with(
            "uncharted",
            "codex",
            &user_scope,
            &fixture.store(),
            false,
            &NoRemovalFaults,
        )
        .expect("remove only the selected user installation");
        assert!(removal.contains("Scope: user"));
        assert!(!user_destination.exists());
        assert!(project_destination.join("SKILL.md").is_file());
        assert!(unmanaged.join("SKILL.md").is_file());
        let remaining = fixture.store().list().expect("project receipt remains");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope(), "project");

        fixture
            .execute_in_scope("uncharted", &user_scope, &FixtureTransport::archive())
            .expect("reinstall user skill for reverse isolation check");
        let project_removal = execute_remove_with(
            "uncharted",
            "codex",
            None,
            &fixture.current,
            &fixture.store(),
            false,
            &NoRemovalFaults,
        )
        .expect("default project removal");
        assert!(project_removal.contains("Scope: project"));
        assert!(!project_destination.exists());
        assert!(user_destination.join("SKILL.md").is_file());
        assert!(unmanaged.join("SKILL.md").is_file());
        let remaining = fixture.store().list().expect("user receipt remains");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope(), "user");
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
                    _current_directory: &fixture.current,
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
    fn user_install_receipt_publication_recovery_keeps_scope_and_root_identity() {
        let fixture = InstallFixture::new();
        let home = fixture
            ._root
            .path()
            .canonicalize()
            .expect("fixture root")
            .join("home");
        fs::create_dir(&home).expect("isolated home");
        let scope = ResolvedScope::for_test(InstallScope::User, home.clone());
        let factory = TestFactory(fixture.staging.clone());
        let failed = execute_install_in_scope_with(
            "uncharted",
            "codex",
            &scope,
            InstallContext {
                catalog: &fixture.catalog,
                _current_directory: &fixture.current,
                transport: &FixtureTransport::archive(),
                tempdir_factory: &factory,
                receipt_store: || Ok(fixture.store()),
                complete: |session: &InstallationSession, store: &ReceiptStore| {
                    session.commit_and_record_at_with_journal(
                        store,
                        1_900_000_000,
                        &NoInstallFaults,
                        &FailReceiptAt(ReceiptOperation::PublishNoReplace),
                        &NoJournalFaults,
                    )
                },
            },
        );
        let error = failed.expect_err("publication fault must not report success");
        assert!(error.to_string().contains("installed files remain"));
        let destination = home.join(".agents/skills/uncharted-ideation");
        assert!(destination.join("SKILL.md").is_file());
        assert!(
            fixture
                .store()
                .list()
                .expect("unpublished receipt")
                .is_empty()
        );

        fixture
            .store()
            .recover_with_faults(&NoJournalFaults, &NoReceiptFaults)
            .expect("recover interrupted user install");
        let receipts = fixture.store().list().expect("recovered receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].scope(), "user");
        assert_eq!(receipts[0].schema_version(), 3);
        assert_eq!(receipts[0].root_path(), home);
        assert_eq!(receipts[0].destination_path(), destination);
        assert!(receipts[0].supports_lifecycle());
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
                        _current_directory: &current,
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

    #[test]
    fn concurrent_project_and_user_installs_share_the_writer_lock_without_scope_confusion() {
        let fixture = InstallFixture::new();
        let home = fixture
            ._root
            .path()
            .canonicalize()
            .expect("fixture root")
            .join("home");
        fs::create_dir(&home).expect("isolated user home");
        let scopes = [
            ResolvedScope::for_test(InstallScope::Project, fixture.project.clone()),
            ResolvedScope::for_test(InstallScope::User, home),
        ];
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for (index, scope) in scopes.iter().cloned().enumerate() {
            let catalog = fixture.catalog.clone();
            let current = fixture.current.clone();
            let staging = fixture
                .staging
                .parent()
                .expect("staging parent")
                .join(format!("scope-staging-{index}"));
            fs::create_dir(&staging).expect("worker staging directory");
            let data = fixture.data.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                let factory = TestFactory(staging);
                execute_install_in_scope_with(
                    "uncharted",
                    "codex",
                    &scope,
                    InstallContext {
                        catalog: &catalog,
                        _current_directory: &current,
                        transport: &FixtureTransport::archive(),
                        tempdir_factory: &factory,
                        receipt_store: || Ok(ReceiptStore::new(data.join("receipts"))),
                        complete: InstallationSession::commit_and_record,
                    },
                )
                .is_ok()
            }));
        }
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("scope worker finishes"))
            .collect::<Vec<_>>();
        let successes = outcomes.iter().filter(|success| **success).count();
        assert!(
            successes >= 1,
            "at least one concurrent scope install succeeds"
        );
        let mut receipts = fixture.store().list().expect("winner receipt");
        assert!((1..=2).contains(&receipts.len()));
        for scope in &scopes {
            if !receipts
                .iter()
                .any(|receipt| receipt.scope() == scope.scope().as_str())
            {
                fixture
                    .execute_in_scope("uncharted", scope, &FixtureTransport::archive())
                    .expect("a contended scope succeeds after the shared lock is released");
                receipts = fixture.store().list().expect("scope receipts after retry");
            }
        }
        receipts = fixture.store().list().expect("both scopes after retry");
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().any(|receipt| receipt.scope() == "project"));
        assert!(receipts.iter().any(|receipt| receipt.scope() == "user"));
    }

    fn sha256_hex(value: &[u8]) -> String {
        digest(&SHA256, value)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
