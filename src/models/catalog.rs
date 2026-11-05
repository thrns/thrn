use std::str::FromStr;

use thiserror::Error;

/// The catalog's schema version. Version 1 is the only supported version.
pub const CATALOG_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Catalog {
    pub(crate) schema_version: u8,
    pub(crate) resources: Vec<Resource>,
}

impl Catalog {
    pub fn schema_version(&self) -> u8 {
        self.schema_version
    }

    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resource {
    pub(crate) id: SkillId,
    pub(crate) kind: ResourceKind,
    pub(crate) display_name: String,
    pub(crate) skill_name: SkillName,
    pub(crate) description: String,
    pub(crate) version: ResourceVersion,
    pub(crate) aliases: Vec<SkillAlias>,
    pub(crate) source: SourceIdentity,
}

impl Resource {
    pub fn id(&self) -> &SkillId {
        &self.id
    }

    pub fn kind(&self) -> ResourceKind {
        self.kind
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn skill_name(&self) -> &SkillName {
        &self.skill_name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn version(&self) -> &ResourceVersion {
        &self.version
    }

    pub fn aliases(&self) -> &[SkillAlias] {
        &self.aliases
    }

    pub fn source(&self) -> &SourceIdentity {
        &self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Skill,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceProvider {
    Github,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceIdentity {
    pub(crate) provider: SourceProvider,
    pub(crate) owner: GithubOwner,
    pub(crate) repository: GithubRepository,
    pub(crate) path: SourcePath,
    pub(crate) commit: CommitSha,
}

impl SourceIdentity {
    pub fn provider(&self) -> SourceProvider {
        self.provider
    }

    pub fn owner(&self) -> &GithubOwner {
        &self.owner
    }

    pub fn repository(&self) -> &GithubRepository {
        &self.repository
    }

    pub fn path(&self) -> &SourcePath {
        &self.path
    }

    pub fn commit(&self) -> &CommitSha {
        &self.commit
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillId(String);

impl SkillId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the ASCII case-insensitive lookup key without changing the stored ID.
    pub fn lookup_key(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl FromStr for SkillId {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_lookup_identifier(value)?;
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillAlias(String);

impl SkillAlias {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the ASCII case-insensitive lookup key without changing the stored alias.
    pub fn lookup_key(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl FromStr for SkillAlias {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_lookup_identifier(value)?;
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillName(String);

impl SkillName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillName {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !valid_identifier_shape(value, 64, true) {
            return Err(IdentifierError::SkillName);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubOwner(String);

impl GithubOwner {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for GithubOwner {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid_chars = value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let valid_edges = value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric);
        let no_repeated_hyphens = !value.contains("--");
        if value.len() > 39 || !valid_chars || !valid_edges || !no_repeated_hyphens {
            return Err(IdentifierError::GithubOwner);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubRepository(String);

impl GithubRepository {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for GithubRepository {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid_chars = value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        let valid_edges = value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric);
        if value.len() > 100 || !valid_chars || !valid_edges || value.contains("..") {
            return Err(IdentifierError::GithubRepository);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePath(String);

impl SourcePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Number of slash-separated components in this lexical repository path.
    pub fn depth(&self) -> usize {
        self.0.split('/').count()
    }
}

impl FromStr for SourcePath {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 1024
            || value.starts_with('/')
            || value.contains('\\')
            || value.chars().any(char::is_control)
            || value
                .as_bytes()
                .get(1)
                .is_some_and(|colon| *colon == b':' && value.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err(IdentifierError::SourcePath);
        }

        let mut depth = 0;
        for segment in value.split('/') {
            depth += 1;
            if depth > 32
                || segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment.len() > 255
            {
                return Err(IdentifierError::SourcePath);
            }
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CommitSha {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        let known_placeholder = matches!(
            normalized.as_str(),
            "0123456789abcdef0123456789abcdef01234567" | "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        );
        if value.len() != 40
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            || value.bytes().all(|byte| byte == value.as_bytes()[0])
            || known_placeholder
        {
            return Err(IdentifierError::CommitSha);
        }
        Ok(Self(normalized))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceVersion(String);

impl ResourceVersion {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ResourceVersion {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid_chars = value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-' | b'_'));
        if value.is_empty()
            || value.len() > 64
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !valid_chars
        {
            return Err(IdentifierError::Version);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IdentifierError {
    #[error("must contain 1–64 ASCII letters, digits, or single hyphens")]
    LookupIdentifier,
    #[error("must contain 1–64 lowercase ASCII letters, digits, or single hyphens")]
    SkillName,
    #[error("must be a GitHub owner using up to 39 ASCII letters, digits, or single hyphens")]
    GithubOwner,
    #[error(
        "must be a GitHub repository name using up to 100 ASCII letters, digits, dots, hyphens, or underscores"
    )]
    GithubRepository,
    #[error("must be a relative slash-separated path with safe lexical components")]
    SourcePath,
    #[error("must contain exactly 40 hexadecimal characters")]
    CommitSha,
    #[error("must be a 1–64 character ASCII version token")]
    Version,
}

fn validate_lookup_identifier(value: &str) -> Result<(), IdentifierError> {
    if valid_identifier_shape(value, 64, false) {
        Ok(())
    } else {
        Err(IdentifierError::LookupIdentifier)
    }
}

fn valid_identifier_shape(value: &str, maximum: usize, lowercase_only: bool) -> bool {
    if value.is_empty() || value.len() > maximum {
        return false;
    }

    let mut previous_was_hyphen = true;
    for byte in value.bytes() {
        if byte == b'-' {
            if previous_was_hyphen {
                return false;
            }
            previous_was_hyphen = true;
        } else if byte.is_ascii_alphanumeric() && (!lowercase_only || !byte.is_ascii_uppercase()) {
            previous_was_hyphen = false;
        } else {
            return false;
        }
    }
    !previous_was_hyphen
}

#[cfg(test)]
mod tests {
    use super::{CommitSha, GithubOwner, GithubRepository, SkillId, SkillName, SourcePath};
    use std::str::FromStr;

    #[test]
    fn lookup_identifiers_preserve_spelling_and_normalize_only_the_lookup_key() {
        let identifier = SkillId::from_str("Uncharted").expect("valid identifier");

        assert_eq!(identifier.as_str(), "Uncharted");
        assert_eq!(identifier.lookup_key(), "uncharted");
    }

    #[test]
    fn skill_names_remain_lowercase_and_follow_agent_skills_shape() {
        assert!(SkillName::from_str("uncharted-ideation").is_ok());
        assert!(SkillName::from_str("Uncharted").is_err());
        assert!(SkillName::from_str("double--hyphen").is_err());
    }

    #[test]
    fn github_components_and_paths_reject_unsafe_shapes() {
        assert!(GithubOwner::from_str("good-owner").is_ok());
        assert!(GithubOwner::from_str("bad/owner").is_err());
        assert!(GithubRepository::from_str("skills_repo.v1").is_ok());
        assert!(GithubRepository::from_str("repository..name").is_err());
        assert!(SourcePath::from_str("skills/uncharted").is_ok());
        assert!(SourcePath::from_str("skills/../outside").is_err());
        assert!(SourcePath::from_str("C:/skills/item").is_err());
    }

    #[test]
    fn commit_hash_accepts_full_hex_and_normalizes_case() {
        let commit = CommitSha::from_str("ABCDEF0123456789ABCDEF0123456789ABCDEF01")
            .expect("valid full commit hash");

        assert_eq!(commit.as_str(), "abcdef0123456789abcdef0123456789abcdef01");
        assert!(CommitSha::from_str("deadbeef").is_err());
    }
}
