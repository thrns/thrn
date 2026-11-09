//! Explicit, offline agent adapters and destination policy.

use std::path::PathBuf;

use clap::ValueEnum;

use crate::install::plan::InstallScope;

const MAX_AGENT_PATH_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AgentId {
    Codex,
    #[value(name = "claude-code")]
    ClaudeCode,
    Cursor,
    Gemini,
}

impl AgentId {
    pub fn from_identifier(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude-code" => Some(Self::ClaudeCode),
            "cursor" => Some(Self::Cursor),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
        }
    }

    /// Command name that can identify an installed agent CLI on `PATH`.
    /// This is a host capability hint only; it does not prove that a target
    /// directory is discoverable or that an agent will load its contents.
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
        }
    }

    pub const fn adapter(self) -> AgentAdapter {
        match self {
            Self::Codex => AgentAdapter {
                id: self,
                directory: ".agents",
                layout_id: "codex-agents-v1",
            },
            Self::ClaudeCode => AgentAdapter {
                id: self,
                directory: ".claude",
                layout_id: "claude-code-default-v1",
            },
            Self::Cursor => AgentAdapter {
                id: self,
                directory: ".cursor",
                layout_id: "cursor-default-v1",
            },
            Self::Gemini => AgentAdapter {
                id: self,
                directory: ".gemini",
                layout_id: "gemini-cli-default-v1",
            },
        }
    }
}

/// Read-only PATH inspection result for an agent's command-line executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentBinaryCapability {
    pub agent: AgentId,
    pub executable: Option<PathBuf>,
}

/// Inspect PATH without invoking binaries or creating agent configuration paths.
/// The result is informational and must not be used to select installation targets.
pub fn inspect_host_agent_binaries() -> Vec<AgentBinaryCapability> {
    let search_paths = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    inspect_agent_binaries_in(&search_paths)
}

/// Inspect at most 256 entries from an explicit PATH-like list.
pub fn inspect_agent_binaries_in(search_paths: &[PathBuf]) -> Vec<AgentBinaryCapability> {
    [
        AgentId::Codex,
        AgentId::ClaudeCode,
        AgentId::Cursor,
        AgentId::Gemini,
    ]
    .into_iter()
    .map(|agent| AgentBinaryCapability {
        agent,
        executable: find_agent_binary(
            agent,
            &search_paths[..search_paths.len().min(MAX_AGENT_PATH_ENTRIES)],
        ),
    })
    .collect()
}

fn find_agent_binary(agent: AgentId, search_paths: &[PathBuf]) -> Option<PathBuf> {
    let names = executable_names(agent.binary_name());
    for directory in search_paths {
        for name in &names {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn executable_names(name: &str) -> Vec<String> {
    [".exe", ".cmd", ".bat"]
        .into_iter()
        .map(|extension| format!("{name}{extension}"))
        .collect()
}

#[cfg(not(windows))]
fn executable_names(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(not(any(unix, windows)))]
fn is_executable_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentAdapter {
    id: AgentId,
    directory: &'static str,
    layout_id: &'static str,
}

impl AgentAdapter {
    pub const fn id(self) -> AgentId {
        self.id
    }

    /// First path component below the selected project or user root.
    pub const fn skill_directory(self) -> &'static str {
        self.directory
    }

    /// Versioned destination layout recorded in new receipts.
    pub const fn layout_id(self) -> &'static str {
        self.layout_id
    }

    pub const fn supports_scope(self, _scope: InstallScope) -> bool {
        true
    }

    /// Cursor and Gemini also discover the interoperable `.agents/skills` location.
    pub const fn discovers_shared_agents_skills(self) -> bool {
        matches!(self.id, AgentId::Cursor | AgentId::Gemini)
    }

    pub fn skills_root(self, root: std::path::PathBuf) -> PathBuf {
        root.join(self.directory).join("skills")
    }

    pub fn project_skills_root(self, project: impl Into<PathBuf>) -> PathBuf {
        self.skills_root(project.into())
    }

    pub fn user_skills_root(self, home: impl Into<PathBuf>) -> PathBuf {
        self.skills_root(home.into())
    }

    pub fn destination_path(
        self,
        scope: InstallScope,
        root: impl Into<PathBuf>,
        skill_name: &str,
    ) -> Option<PathBuf> {
        (self.supports_scope(scope) && !self.rejects_skill_name(skill_name))
            .then(|| self.skills_root(root.into()).join(skill_name))
    }

    pub fn validates_destination(
        self,
        scope: InstallScope,
        root: impl Into<PathBuf>,
        skill_name: &str,
        destination: &std::path::Path,
    ) -> bool {
        self.destination_path(scope, root, skill_name)
            .is_some_and(|expected| expected == destination)
    }

    pub fn rejects_skill_name(self, name: &str) -> bool {
        match self.id {
            AgentId::Codex => false,
            AgentId::ClaudeCode => {
                name.eq_ignore_ascii_case("synced")
                    || name.eq_ignore_ascii_case("anthropic-skills")
                    || name
                        .get(.."anthropic-skills:".len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("anthropic-skills:"))
            }
            AgentId::Cursor | AgentId::Gemini => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, inspect_agent_binaries_in};
    use crate::install::plan::InstallScope;

    #[test]
    fn adapters_define_distinct_project_and_user_layouts() {
        assert_eq!(
            AgentId::Codex.adapter().skills_root("/tmp/project".into()),
            std::path::PathBuf::from("/tmp/project/.agents/skills")
        );
        assert_eq!(
            AgentId::ClaudeCode
                .adapter()
                .skills_root("/tmp/home".into()),
            std::path::PathBuf::from("/tmp/home/.claude/skills")
        );
        assert_eq!(
            AgentId::Cursor.adapter().skills_root("/tmp/project".into()),
            std::path::PathBuf::from("/tmp/project/.cursor/skills")
        );
        assert_eq!(
            AgentId::Gemini.adapter().skills_root("/tmp/home".into()),
            std::path::PathBuf::from("/tmp/home/.gemini/skills")
        );
        for agent in [
            AgentId::Codex,
            AgentId::ClaudeCode,
            AgentId::Cursor,
            AgentId::Gemini,
        ] {
            assert!(agent.adapter().supports_scope(InstallScope::Project));
            assert!(agent.adapter().supports_scope(InstallScope::User));
        }
        assert_eq!(
            AgentId::ClaudeCode.adapter().destination_path(
                InstallScope::Project,
                "/tmp/project",
                "document-skills",
            ),
            Some(std::path::PathBuf::from(
                "/tmp/project/.claude/skills/document-skills"
            ))
        );
        assert!(AgentId::ClaudeCode.adapter().validates_destination(
            InstallScope::User,
            "/tmp/home",
            "document-skills",
            std::path::Path::new("/tmp/home/.claude/skills/document-skills")
        ));
    }

    #[test]
    fn claude_reserved_names_are_case_insensitive() {
        let claude = AgentId::ClaudeCode.adapter();
        assert!(claude.rejects_skill_name("synced"));
        assert!(claude.rejects_skill_name("Synced"));
        assert!(claude.rejects_skill_name("anthropic-skills"));
        assert!(claude.rejects_skill_name("ANTHROPIC-SKILLS:pdf"));
        assert!(!claude.rejects_skill_name("document-skills"));
        assert!(!AgentId::Codex.adapter().rejects_skill_name("synced"));
    }

    #[test]
    fn host_capability_inspection_reads_path_without_creating_agent_roots() {
        let root = tempfile::tempdir().expect("temporary capability root");
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).expect("bin directory");
        #[cfg(windows)]
        let executable = bin.join("gemini.exe");
        #[cfg(not(windows))]
        let executable = bin.join("gemini");
        std::fs::write(&executable, b"fixture executable").expect("fake executable file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("mark fixture executable");
        }

        let capabilities = inspect_agent_binaries_in(std::slice::from_ref(&bin));
        let gemini = capabilities
            .iter()
            .find(|capability| capability.agent == AgentId::Gemini)
            .expect("Gemini capability");
        assert_eq!(gemini.executable.as_deref(), Some(executable.as_path()));
        assert!(
            capabilities
                .iter()
                .filter(|capability| capability.agent != AgentId::Gemini)
                .all(|capability| capability.executable.is_none())
        );
        assert!(!root.path().join(".gemini/skills").exists());
        assert!(!root.path().join(".cursor/skills").exists());
    }
}
