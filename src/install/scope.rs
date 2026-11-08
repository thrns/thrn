//! Typed, validated scope resolution shared by install and lifecycle commands.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::os_paths::{OsPathError, OsPaths};

use super::{
    commit::SecureDir,
    plan::{InstallAgent, InstallScope},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedScope {
    agent: InstallAgent,
    scope: InstallScope,
    root: PathBuf,
}

impl ResolvedScope {
    #[cfg(test)]
    pub(crate) fn from_validated(scope: InstallScope, root: PathBuf) -> Self {
        Self {
            agent: InstallAgent::Codex,
            scope,
            root,
        }
    }

    pub(crate) fn from_agent_validated(
        agent: InstallAgent,
        scope: InstallScope,
        root: PathBuf,
    ) -> Self {
        Self { agent, scope, root }
    }

    pub(crate) fn agent(&self) -> InstallAgent {
        self.agent
    }

    pub(crate) fn scope(&self) -> InstallScope {
        self.scope
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    pub(crate) fn skills_root(&self) -> PathBuf {
        self.agent.adapter().skills_root(self.root.clone())
    }

    #[cfg(test)]
    pub(crate) fn for_test(scope: InstallScope, root: PathBuf) -> Self {
        Self::from_validated(scope, root)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ScopeResolver {
    paths: OsPaths,
}

impl ScopeResolver {
    pub(crate) fn for_system() -> Result<Self, ScopeResolutionError> {
        Ok(Self {
            paths: OsPaths::for_system()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_paths(paths: OsPaths) -> Self {
        Self { paths }
    }

    pub(crate) fn resolve(
        &self,
        agent: InstallAgent,
        scope: InstallScope,
        project: Option<&Path>,
        current_directory: Option<&Path>,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        if !agent.adapter().supports_scope(scope) {
            return Err(ScopeResolutionError::UnsupportedScope {
                agent: agent.as_str(),
                scope: scope.as_str(),
            });
        }
        if scope == InstallScope::User && project.is_some() {
            return Err(ScopeResolutionError::ProjectPathWithUserScope);
        }
        match scope {
            InstallScope::Project => {
                let current_directory =
                    current_directory.ok_or(ScopeResolutionError::InvalidProjectRoot)?;
                let root = super::removal::resolve_project_root(project, current_directory)
                    .map_err(|_| ScopeResolutionError::InvalidProjectRoot)?;
                Ok(ResolvedScope { agent, scope, root })
            }
            InstallScope::User => {
                let root = resolve_user_home(self.paths.user_home())?;
                Ok(ResolvedScope { agent, scope, root })
            }
        }
    }
}

fn resolve_user_home(home: &Path) -> Result<PathBuf, ScopeResolutionError> {
    if !home.is_absolute()
        || home.as_os_str().is_empty()
        || home
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ScopeResolutionError::UnsafeHomeDirectory);
    }

    let opened =
        SecureDir::open_absolute(home).map_err(|_| ScopeResolutionError::UnsafeHomeDirectory)?;
    let canonical = home
        .canonicalize()
        .map_err(|_| ScopeResolutionError::UnsafeHomeDirectory)?;
    let canonical_opened = SecureDir::open_absolute(&canonical)
        .map_err(|_| ScopeResolutionError::UnsafeHomeDirectory)?;
    let opened_identity = opened
        .identity()
        .map_err(|_| ScopeResolutionError::UnsafeHomeDirectory)?;
    let canonical_identity = canonical_opened
        .identity()
        .map_err(|_| ScopeResolutionError::UnsafeHomeDirectory)?;
    if opened_identity != canonical_identity {
        return Err(ScopeResolutionError::UnsafeHomeDirectory);
    }
    Ok(canonical)
}

#[derive(Debug, Error)]
pub(crate) enum ScopeResolutionError {
    #[error(transparent)]
    OsPaths(#[from] OsPathError),
    #[error("--project can only be used with --scope project")]
    ProjectPathWithUserScope,
    #[error("unsupported agent {0:?}")]
    UnsupportedAgent(String),
    #[error("agent {agent} does not support {scope} scope")]
    UnsupportedScope {
        agent: &'static str,
        scope: &'static str,
    },
    #[error("the selected project directory is unavailable or unsafe")]
    InvalidProjectRoot,
    #[error("the operating-system home directory is missing, ambiguous, or unsafe")]
    UnsafeHomeDirectory,
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    #[cfg(windows)]
    use crate::install::windows_test_support::create_junction;
    use crate::os_paths::OsPaths;

    use super::{InstallAgent, ScopeResolutionError, ScopeResolver};
    use crate::install::plan::InstallScope;

    fn resolver(home: PathBuf) -> ScopeResolver {
        ScopeResolver::with_paths(OsPaths::for_test(home))
    }

    #[test]
    fn user_scope_resolves_only_to_the_existing_real_home() {
        let root = tempdir().expect("temporary root");
        let root_path = root.path().canonicalize().expect("canonical temp root");
        let home = root_path.join("home");
        fs::create_dir(&home).expect("home directory");
        let resolved = resolver(home.clone())
            .resolve(
                InstallAgent::Codex,
                InstallScope::User,
                None,
                Some(root.path()),
            )
            .expect("user scope resolves");
        assert_eq!(resolved.scope(), InstallScope::User);
        assert_eq!(resolved.root(), home);
        assert_eq!(resolved.skills_root(), home.join(".agents").join("skills"));
    }

    #[test]
    fn user_scope_rejects_missing_relative_and_symlink_homes() {
        let root = tempdir().expect("temporary root");
        let root_path = root.path().canonicalize().expect("canonical temp root");
        let missing = root_path.join("missing");
        assert!(matches!(
            resolver(missing).resolve(
                InstallAgent::Codex,
                InstallScope::User,
                None,
                Some(&root_path)
            ),
            Err(ScopeResolutionError::UnsafeHomeDirectory)
        ));
        assert!(matches!(
            resolver(PathBuf::from("relative-home")).resolve(
                InstallAgent::Codex,
                InstallScope::User,
                None,
                Some(&root_path)
            ),
            Err(ScopeResolutionError::UnsafeHomeDirectory)
        ));

        let real_home = root_path.join("real-home");
        let link_home = root_path.join("linked-home");
        fs::create_dir(&real_home).expect("real home");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_home, &link_home).expect("symlink home");
        #[cfg(windows)]
        create_junction(&link_home, &real_home);
        assert!(matches!(
            resolver(link_home).resolve(
                InstallAgent::Codex,
                InstallScope::User,
                None,
                Some(&root_path)
            ),
            Err(ScopeResolutionError::UnsafeHomeDirectory)
        ));
    }

    #[test]
    fn project_path_is_rejected_for_user_scope_before_resolution() {
        let root = tempdir().expect("temporary root");
        let root_path = root.path().canonicalize().expect("canonical temp root");
        let home = root_path.join("home");
        fs::create_dir(&home).expect("home directory");
        let project = root_path.join("project");
        fs::create_dir(&project).expect("project directory");
        assert!(matches!(
            resolver(home).resolve(
                InstallAgent::Codex,
                InstallScope::User,
                Some(&project),
                Some(&project)
            ),
            Err(ScopeResolutionError::ProjectPathWithUserScope)
        ));
    }
}
