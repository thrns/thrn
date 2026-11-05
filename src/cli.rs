use std::io::{self, Write};

use clap::{Parser, Subcommand};

use crate::{
    doctor,
    error::Result,
    registry::discovery::{CatalogResolver, builtin_catalog},
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
    /// Search and inspect curated Agent Skills catalog metadata offline.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    /// Search the locally bundled catalog.
    Search { query: Option<String> },
    /// Show curated catalog metadata for an exact ID or explicit alias.
    Info { id: String },
}

pub fn run() -> Result<()> {
    match Cli::try_parse()
        .map_err(crate::error::ThrnError::from)?
        .command
    {
        Command::Doctor => doctor::run(),
        Command::Skills { command } => match command {
            SkillsCommand::Search { query } => search_skills(query.as_deref()),
            SkillsCommand::Info { id } => show_skill_info(&id),
        },
    }
}

fn search_skills(query: Option<&str>) -> Result<()> {
    let catalog = builtin_catalog()?;
    let resolver = CatalogResolver::new(&catalog);
    let resources = resolver.search(query);

    let output = if resources.is_empty() {
        if catalog.resources().is_empty() {
            "No skills are available in the bundled catalog.\n".to_owned()
        } else if let Some(query) = query {
            format!(
                "No skills matched query {query:?}.\nTry `thrn skills search` to list available skills.\n"
            )
        } else {
            "No skills are available in the bundled catalog.\n".to_owned()
        }
    } else {
        let mut output = String::from(
            "Curated skill catalog (schema v1; bundled static metadata, no network access)\n",
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
    let catalog = builtin_catalog()?;
    let resolver = CatalogResolver::new(&catalog);
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
        "Curated catalog information (schema v{})\nCanonical catalog ID: {}\nDisplay name: {}\nDescription: {}\nSkill directory name: {}\nCatalog version: {}\nExplicit aliases: {}\nGitHub owner: {}\nGitHub repository: {}\nSource directory: {}\nPinned commit: {}\nPublisher authenticity: not guaranteed by this commit pin.\n",
        catalog.schema_version(),
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

fn write_stdout(output: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
