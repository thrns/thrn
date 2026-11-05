use std::{collections::HashMap, str::FromStr};

use serde::Deserialize;
use serde::de::{self, SeqAccess, Visitor};
use thiserror::Error;

use crate::models::catalog::{
    CATALOG_SCHEMA_VERSION, Catalog, CommitSha, GithubOwner, GithubRepository, Resource,
    ResourceKind, ResourceVersion, SkillAlias, SkillId, SkillName, SourceIdentity, SourcePath,
    SourceProvider,
};

/// Maximum encoded catalog size accepted by [`parse_catalog_json`].
pub const MAX_CATALOG_BYTES: usize = 1024 * 1024;
/// Maximum resources accepted in one catalog.
pub const MAX_CATALOG_RESOURCES: usize = 256;
/// Maximum explicit aliases accepted for one resource.
pub const MAX_ALIASES_PER_RESOURCE: usize = 32;

const MAX_DISPLAY_NAME_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 2048;

/// Parse and validate an untrusted version-1 catalog document.
///
/// The size and collection limits are checked while decoding. The returned model contains only
/// validated identity and source-location types. This checks source paths lexically; it does not
/// fetch files, inspect archives, or guarantee safe filesystem extraction.
pub fn parse_catalog_json(input: &[u8]) -> Result<Catalog, CatalogError> {
    if input.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError::InputTooLarge {
            actual_bytes: input.len(),
            maximum_bytes: MAX_CATALOG_BYTES,
        });
    }

    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let raw: RawCatalog =
        serde_path_to_error::deserialize(&mut deserializer).map_err(CatalogJsonError::from)?;
    deserializer.end().map_err(CatalogJsonError::from)?;
    if raw.schema_version != u64::from(CATALOG_SCHEMA_VERSION) {
        return Err(CatalogError::UnsupportedSchemaVersion {
            found: raw.schema_version,
            supported: CATALOG_SCHEMA_VERSION,
        });
    }

    let resources = raw
        .resources
        .0
        .into_iter()
        .enumerate()
        .map(|(index, resource)| validate_resource(index, resource))
        .collect::<Result<Vec<_>, _>>()?;
    validate_unique_identifiers(&resources)?;

    Ok(Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        resources,
    })
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("catalog input is {actual_bytes} bytes; maximum allowed is {maximum_bytes} bytes")]
    InputTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },
    #[error("{0}")]
    Json(#[from] CatalogJsonError),
    #[error("unsupported catalog schemaVersion {found}; supported version is {supported}")]
    UnsupportedSchemaVersion { found: u64, supported: u8 },
    #[error("invalid catalog field `{field}`: {reason}")]
    InvalidField {
        field: String,
        reason: ValidationReason,
    },
    #[error("{duplicate} collides case-insensitively with {first}")]
    DuplicateIdentifier { first: String, duplicate: String },
}

#[derive(Debug)]
pub struct CatalogJsonError {
    line: usize,
    column: usize,
    field_path: Option<String>,
    problem: JsonProblem,
}

impl CatalogJsonError {
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn column(&self) -> usize {
        self.column
    }

    pub fn problem(&self) -> JsonProblem {
        self.problem
    }

    pub fn field_path(&self) -> Option<&str> {
        self.field_path.as_deref()
    }
}

impl std::fmt::Display for CatalogJsonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "catalog JSON {}", self.problem)?;
        if let Some(field_path) = self.field_path.as_deref() {
            write!(formatter, " at field `{field_path}`")?;
        }
        write!(formatter, " at line {}, column {}", self.line, self.column)
    }
}

impl std::error::Error for CatalogJsonError {}

impl From<serde_path_to_error::Error<serde_json::Error>> for CatalogJsonError {
    fn from(error: serde_path_to_error::Error<serde_json::Error>) -> Self {
        let field_path = sanitized_field_path(error.path());
        Self::with_inner(error.into_inner(), field_path)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JsonProblem {
    #[error("is malformed")]
    Malformed,
    #[error("ended unexpectedly")]
    UnexpectedEnd,
    #[error("has the wrong shape or JSON type")]
    InvalidShape,
    #[error("is missing required field `{0}`")]
    MissingRequiredField(&'static str),
    #[error("contains an unknown object field")]
    UnknownField,
    #[error("contains a duplicate object field")]
    DuplicateField,
    #[error("array exceeds the maximum of {0} entries")]
    TooManyEntries(usize),
}

impl From<serde_json::Error> for CatalogJsonError {
    fn from(error: serde_json::Error) -> Self {
        Self::with_inner(error, None)
    }
}

impl CatalogJsonError {
    fn with_inner(error: serde_json::Error, field_path: Option<String>) -> Self {
        let message = error.to_string();
        let problem = match error.classify() {
            serde_json::error::Category::Syntax => JsonProblem::Malformed,
            serde_json::error::Category::Eof => JsonProblem::UnexpectedEnd,
            serde_json::error::Category::Io => JsonProblem::Malformed,
            serde_json::error::Category::Data => classify_schema_error(&message),
        };

        Self {
            line: error.line(),
            column: error.column(),
            field_path,
            problem,
        }
    }
}

fn sanitized_field_path(path: &serde_path_to_error::Path) -> Option<String> {
    let mut output = String::new();
    for segment in path.iter() {
        match segment {
            serde_path_to_error::Segment::Seq { index } => {
                use std::fmt::Write as _;
                write!(&mut output, "[{index}]").ok()?;
            }
            serde_path_to_error::Segment::Map { key }
                if matches!(
                    key.as_str(),
                    "schemaVersion"
                        | "resources"
                        | "id"
                        | "kind"
                        | "displayName"
                        | "skillName"
                        | "description"
                        | "version"
                        | "aliases"
                        | "source"
                        | "provider"
                        | "owner"
                        | "repository"
                        | "path"
                        | "commit"
                ) =>
            {
                if !output.is_empty() {
                    output.push('.');
                }
                output.push_str(key);
            }
            serde_path_to_error::Segment::Map { .. }
            | serde_path_to_error::Segment::Enum { .. }
            | serde_path_to_error::Segment::Unknown => return None,
        }
    }
    (!output.is_empty()).then_some(output)
}

fn classify_schema_error(message: &str) -> JsonProblem {
    const FIELDS: &[&str] = &[
        "schemaVersion",
        "resources",
        "id",
        "kind",
        "displayName",
        "skillName",
        "description",
        "version",
        "source",
        "provider",
        "owner",
        "repository",
        "path",
        "commit",
    ];

    for field in FIELDS {
        if message.contains(&format!("missing field `{field}`")) {
            return JsonProblem::MissingRequiredField(field);
        }
    }
    if message.contains("unknown field") {
        return JsonProblem::UnknownField;
    }
    if message.contains("duplicate field") {
        return JsonProblem::DuplicateField;
    }
    for maximum in [MAX_CATALOG_RESOURCES, MAX_ALIASES_PER_RESOURCE] {
        if message.contains(&format!("array exceeds the maximum of {maximum} entries")) {
            return JsonProblem::TooManyEntries(maximum);
        }
    }
    JsonProblem::InvalidShape
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ValidationReason {
    #[error("must not be empty or whitespace-only")]
    Empty,
    #[error("exceeds the maximum of {maximum_bytes} UTF-8 bytes")]
    TooLong { maximum_bytes: usize },
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    InvalidIdentifier(crate::models::catalog::IdentifierError),
    #[error("unsupported value")]
    UnsupportedValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    resources: BoundedVec<RawResource, MAX_CATALOG_RESOURCES>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResource {
    id: String,
    kind: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "skillName")]
    skill_name: String,
    description: String,
    version: String,
    #[serde(default)]
    aliases: BoundedVec<String, MAX_ALIASES_PER_RESOURCE>,
    source: RawSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    provider: String,
    owner: String,
    repository: String,
    path: String,
    commit: String,
}

struct BoundedVec<T, const MAXIMUM: usize>(Vec<T>);

impl<T, const MAXIMUM: usize> Default for BoundedVec<T, MAXIMUM> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T, const MAXIMUM: usize> Deserialize<'de> for BoundedVec<T, MAXIMUM>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BoundedVecVisitor<T, const MAXIMUM: usize>(std::marker::PhantomData<T>);

        impl<'de, T, const MAXIMUM: usize> Visitor<'de> for BoundedVecVisitor<T, MAXIMUM>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedVec<T, MAXIMUM>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "an array with at most {MAXIMUM} entries")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                if sequence.size_hint().is_some_and(|length| length > MAXIMUM) {
                    return Err(de::Error::custom(format!(
                        "array exceeds the maximum of {MAXIMUM} entries"
                    )));
                }

                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    if values.len() == MAXIMUM {
                        return Err(de::Error::custom(format!(
                            "array exceeds the maximum of {MAXIMUM} entries"
                        )));
                    }
                    values.push(value);
                }
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor::<T, MAXIMUM>(std::marker::PhantomData))
    }
}

fn validate_resource(index: usize, raw: RawResource) -> Result<Resource, CatalogError> {
    let field = |name: &str| format!("resources[{index}].{name}");

    let id = parse_identifier::<SkillId>(&field("id"), &raw.id)?;
    if raw.kind != "skill" {
        return Err(invalid(field("kind"), ValidationReason::UnsupportedValue));
    }
    let display_name = validate_text(
        &field("displayName"),
        raw.display_name,
        MAX_DISPLAY_NAME_BYTES,
    )?;
    let skill_name = parse_identifier::<SkillName>(&field("skillName"), &raw.skill_name)?;
    let description = validate_text(
        &field("description"),
        raw.description,
        MAX_DESCRIPTION_BYTES,
    )?;
    let version = parse_identifier::<ResourceVersion>(&field("version"), &raw.version)?;
    let aliases = raw
        .aliases
        .0
        .iter()
        .enumerate()
        .map(|(alias_index, value)| {
            parse_identifier::<SkillAlias>(
                &format!("resources[{index}].aliases[{alias_index}]"),
                value,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let source = SourceIdentity {
        provider: match raw.source.provider.as_str() {
            "github" => SourceProvider::Github,
            _ => {
                return Err(invalid(
                    field("source.provider"),
                    ValidationReason::UnsupportedValue,
                ));
            }
        },
        owner: parse_identifier::<GithubOwner>(&field("source.owner"), &raw.source.owner)?,
        repository: parse_identifier::<GithubRepository>(
            &field("source.repository"),
            &raw.source.repository,
        )?,
        path: parse_identifier::<SourcePath>(&field("source.path"), &raw.source.path)?,
        commit: parse_identifier::<CommitSha>(&field("source.commit"), &raw.source.commit)?,
    };

    Ok(Resource {
        id,
        kind: ResourceKind::Skill,
        display_name,
        skill_name,
        description,
        version,
        aliases,
        source,
    })
}

fn validate_text(field: &str, value: String, maximum_bytes: usize) -> Result<String, CatalogError> {
    if value.trim().is_empty() {
        return Err(invalid(field, ValidationReason::Empty));
    }
    if value.len() > maximum_bytes {
        return Err(invalid(field, ValidationReason::TooLong { maximum_bytes }));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(
            field,
            ValidationReason::Invalid("must not contain control characters"),
        ));
    }
    Ok(value)
}

trait ParseValidated: Sized + FromStr<Err = crate::models::catalog::IdentifierError> {}

impl<T> ParseValidated for T where T: Sized + FromStr<Err = crate::models::catalog::IdentifierError> {}

fn parse_identifier<T: ParseValidated>(field: &str, value: &str) -> Result<T, CatalogError> {
    value
        .parse()
        .map_err(|error: crate::models::catalog::IdentifierError| {
            invalid(field, ValidationReason::InvalidIdentifier(error))
        })
}

fn validate_unique_identifiers(resources: &[Resource]) -> Result<(), CatalogError> {
    let mut identifiers = HashMap::<String, String>::new();

    for (index, resource) in resources.iter().enumerate() {
        insert_identifier(
            &mut identifiers,
            resource.id.lookup_key(),
            format!("resources[{index}].id"),
        )?;
        for (alias_index, alias) in resource.aliases.iter().enumerate() {
            insert_identifier(
                &mut identifiers,
                alias.lookup_key(),
                format!("resources[{index}].aliases[{alias_index}]"),
            )?;
        }
    }
    Ok(())
}

fn insert_identifier(
    identifiers: &mut HashMap<String, String>,
    lookup_key: String,
    path: String,
) -> Result<(), CatalogError> {
    if let Some(first) = identifiers.get(&lookup_key) {
        return Err(CatalogError::DuplicateIdentifier {
            first: first.clone(),
            duplicate: path,
        });
    }
    identifiers.insert(lookup_key, path);
    Ok(())
}

fn invalid(field: impl Into<String>, reason: ValidationReason) -> CatalogError {
    CatalogError::InvalidField {
        field: field.into(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::{CatalogError, MAX_CATALOG_BYTES, MAX_CATALOG_RESOURCES, parse_catalog_json};

    #[test]
    fn rejects_input_size_before_json_decoding() {
        let input = vec![b' '; MAX_CATALOG_BYTES + 1];
        assert!(matches!(
            parse_catalog_json(&input),
            Err(CatalogError::InputTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_excessive_resource_count_during_decoding() {
        let resources = std::iter::repeat_n(
            r#"{"id":"item","kind":"skill","displayName":"Item","skillName":"item","description":"Test item","version":"1.0.0","source":{"provider":"github","owner":"owner","repository":"repo","path":"skills/item","commit":"0123456789abcdef0123456789abcdef01234567"}}"#,
            MAX_CATALOG_RESOURCES + 1,
        )
        .collect::<Vec<_>>()
        .join(",");
        let input = format!(r#"{{"schemaVersion":1,"resources":[{resources}]}}"#);

        let error = parse_catalog_json(input.as_bytes()).expect_err("resource limit enforced");
        assert!(error.to_string().contains("maximum of 256 entries"));
    }
}
