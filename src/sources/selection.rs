use std::collections::BTreeSet;

use thiserror::Error;

use crate::models::catalog::{Resource, SourceProvider};

use super::archive::{ArchiveEntryKind, ValidatedArchive};

/// The exact paths inside a validated GitHub archive selected for one catalog resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedSkill {
    archive_root: String,
    source_directory: String,
    skill_file: String,
}

impl SelectedSkill {
    pub fn archive_root(&self) -> &str {
        &self.archive_root
    }

    pub fn source_directory(&self) -> &str {
        &self.source_directory
    }

    pub fn skill_file(&self) -> &str {
        &self.skill_file
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SelectionError {
    #[error("the validated archive is empty")]
    EmptyArchive,
    #[error("the archive does not have exactly one top-level repository root")]
    AmbiguousRepositoryRoots,
    #[error("the top-level archive entry is not a recognized GitHub repository root")]
    InvalidRepositoryRoot,
    #[error("the requested source directory is missing from the pinned archive")]
    MissingSourceDirectory,
    #[error("the requested source path differs in capitalization from the archive")]
    SourcePathCaseMismatch,
    #[error("the requested source path identifies a regular file, not a directory")]
    SourcePathIsFile,
    #[error("the selected source directory does not contain a regular SKILL.md")]
    MissingSkillFile,
    #[error("the selected directory contains SKILL.md with incorrect capitalization")]
    SkillFileCaseMismatch,
    #[error("the source provider is not GitHub")]
    UnsupportedProvider,
}

/// Select exactly `resource.source.path()` beneath the generated GitHub archive root.
///
/// The M2-01 manifest is the authority for all names and entry kinds. This function does not
/// inspect or search other directories for a skill.
pub fn select_skill_directory(
    resource: &Resource,
    archive: &ValidatedArchive,
) -> Result<SelectedSkill, SelectionError> {
    let source = resource.source();
    if source.provider() != SourceProvider::Github {
        return Err(SelectionError::UnsupportedProvider);
    }

    let entries = archive.entries();
    if entries.is_empty() {
        return Err(SelectionError::EmptyArchive);
    }

    let roots = entries
        .iter()
        .filter_map(|entry| entry.path().components().next())
        .collect::<BTreeSet<_>>();
    if roots.len() != 1 {
        return Err(SelectionError::AmbiguousRepositoryRoots);
    }
    let Some(root) = roots.first().copied() else {
        return Err(SelectionError::EmptyArchive);
    };

    let expected_root = format!(
        "{}-{}",
        source.repository().as_str(),
        source.commit().as_str()
    );
    let root_entry = entries.iter().find(|entry| entry.path().as_str() == root);
    if root != expected_root
        || !root_entry
            .is_some_and(|entry| entry.kind() == ArchiveEntryKind::Directory && entry.is_explicit())
    {
        return Err(SelectionError::InvalidRepositoryRoot);
    }

    let source_directory = format!("{root}/{}", source.path().as_str());
    let source_entry = entries
        .iter()
        .find(|entry| entry.path().as_str() == source_directory);
    let Some(source_entry) = source_entry else {
        if entries.iter().any(|entry| {
            entry
                .path()
                .as_str()
                .eq_ignore_ascii_case(&source_directory)
        }) {
            return Err(SelectionError::SourcePathCaseMismatch);
        }
        return Err(SelectionError::MissingSourceDirectory);
    };
    if source_entry.kind() == ArchiveEntryKind::File {
        return Err(SelectionError::SourcePathIsFile);
    }

    let skill_file = format!("{source_directory}/SKILL.md");
    let skill_entry = entries
        .iter()
        .find(|entry| entry.path().as_str() == skill_file);
    match skill_entry {
        Some(entry) if entry.kind() == ArchiveEntryKind::File => {}
        Some(_) => return Err(SelectionError::MissingSkillFile),
        None if entries
            .iter()
            .any(|entry| entry.path().as_str().eq_ignore_ascii_case(&skill_file)) =>
        {
            return Err(SelectionError::SkillFileCaseMismatch);
        }
        _ => return Err(SelectionError::MissingSkillFile),
    }

    Ok(SelectedSkill {
        archive_root: root.to_owned(),
        source_directory,
        skill_file,
    })
}
