//! Explicit, offline agent adapters and destination policy.

use std::path::PathBuf;

use clap::ValueEnum;

use crate::install::plan::InstallScope;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AgentId {
    Codex,
    #[value(name = "claude-code")]
    ClaudeCode,
}

impl AgentId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
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
        }
    }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentId;
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
        for agent in [AgentId::Codex, AgentId::ClaudeCode] {
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
}
