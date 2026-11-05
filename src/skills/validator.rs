use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde_json::Value;
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy, from_str_with_options};
use thiserror::Error;

/// Maximum size of the complete `SKILL.md`, including its Markdown body.
pub const MAX_SKILL_MD_BYTES: usize = 256 * 1024;
/// Maximum number of entries in the optional `metadata` mapping.
pub const MAX_METADATA_ENTRIES: usize = 64;
/// Maximum YAML collection nesting depth in the frontmatter.
pub const MAX_FRONTMATTER_DEPTH: usize = 16;
/// Maximum YAML nodes parsed from frontmatter.
pub const MAX_FRONTMATTER_NODES: usize = 512;
/// Maximum YAML parser events accepted for frontmatter.
pub const MAX_FRONTMATTER_EVENTS: usize = 2_048;

const MAX_SKILL_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1_024;
const MAX_COMPATIBILITY_LENGTH: usize = 500;

const ALLOWED_FIELDS: [&str; 6] = [
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];

/// Validated Agent Skills metadata read from a local `SKILL.md`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
}

/// One actionable validation error. Messages avoid printing the input path or skill body.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ValidationError {
    #[error("skill directory does not exist")]
    DirectoryNotFound,
    #[error("cannot inspect the skill directory ({kind})")]
    InspectDirectory { kind: io::ErrorKind },
    #[error("path is not a directory")]
    NotDirectory,
    #[error("the skill directory name is not valid UTF-8")]
    DirectoryNameNotUtf8,
    #[error("the skill directory needs a final path component; pass the skill directory itself")]
    DirectoryNameMissing,
    #[error("SKILL.md is missing; add a regular SKILL.md file to the skill directory")]
    MissingSkillFile,
    #[error("SKILL.md must be a regular file; symlinks and special files are not accepted")]
    SkillFileNotRegular,
    #[error("SKILL.md exceeds the {maximum_bytes} byte limit")]
    SkillFileTooLarge { maximum_bytes: usize },
    #[error("cannot read SKILL.md ({kind})")]
    ReadSkillFile { kind: io::ErrorKind },
    #[error("SKILL.md is not valid UTF-8")]
    SkillFileNotUtf8,
    #[error("SKILL.md must start with an opening --- frontmatter delimiter on its own line")]
    MissingOpeningDelimiter,
    #[error("SKILL.md frontmatter needs a closing --- delimiter on its own line")]
    MissingClosingDelimiter,
    #[error(
        "invalid YAML in SKILL.md frontmatter at line {line}, column {column}; check YAML syntax"
    )]
    InvalidYamlAt { line: u64, column: u64 },
    #[error("invalid YAML in SKILL.md frontmatter; check YAML syntax")]
    InvalidYaml,
    #[error("SKILL.md frontmatter must be a YAML mapping")]
    FrontmatterNotMapping,
    #[error(
        "unsupported frontmatter field {field:?}; remove it or use a field defined by the Agent Skills specification"
    )]
    UnknownField { field: String },
    #[error("frontmatter field `{field}` is required; add it as a string")]
    MissingField { field: &'static str },
    #[error("frontmatter field {field:?} must be a string")]
    WrongFieldType { field: String },
    #[error("frontmatter field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error(
        "frontmatter field `{field}` must be no more than {maximum} characters (found {actual})"
    )]
    FieldTooLong {
        field: &'static str,
        maximum: usize,
        actual: usize,
    },
    #[error(
        "frontmatter field `name` must use lowercase Unicode letters or digits separated by single hyphens"
    )]
    InvalidNameGrammar,
    #[error("frontmatter field `name` must exactly match the skill directory's final component")]
    NameDirectoryMismatch,
    #[error("frontmatter field `metadata` must be a mapping of string keys to string values")]
    MetadataMustBeStringMap,
    #[error("frontmatter metadata entry {key:?} must have a string value")]
    MetadataValueMustBeString { key: String },
    #[error("frontmatter metadata may contain at most {maximum} entries (found {actual})")]
    TooManyMetadataEntries { maximum: usize, actual: usize },
}

/// Read and validate the required `SKILL.md` in `skill_dir` without writing or executing files.
/// Supporting files are intentionally not opened or traversed.
pub fn validate_skill_directory(skill_dir: &Path) -> Result<SkillMetadata, Vec<ValidationError>> {
    let directory_metadata = match fs::metadata(skill_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(vec![ValidationError::DirectoryNotFound]);
        }
        Err(error) => {
            return Err(vec![ValidationError::InspectDirectory {
                kind: error.kind(),
            }]);
        }
    };
    if !directory_metadata.is_dir() {
        return Err(vec![ValidationError::NotDirectory]);
    }

    let canonical_directory = fs::canonicalize(skill_dir)
        .map_err(|error| vec![ValidationError::InspectDirectory { kind: error.kind() }])?;
    let directory_name = canonical_directory
        .file_name()
        .ok_or_else(|| vec![ValidationError::DirectoryNameMissing])?;
    let directory_name = directory_name
        .to_str()
        .ok_or_else(|| vec![ValidationError::DirectoryNameNotUtf8])?;

    let skill_file: PathBuf = canonical_directory.join("SKILL.md");
    let file_metadata = match fs::symlink_metadata(&skill_file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(vec![ValidationError::MissingSkillFile]);
        }
        Err(error) => {
            return Err(vec![ValidationError::ReadSkillFile { kind: error.kind() }]);
        }
    };
    if !file_metadata.file_type().is_file() {
        return Err(vec![ValidationError::SkillFileNotRegular]);
    }
    if file_metadata.len() > MAX_SKILL_MD_BYTES as u64 {
        return Err(vec![ValidationError::SkillFileTooLarge {
            maximum_bytes: MAX_SKILL_MD_BYTES,
        }]);
    }

    let file = open_without_following_symlinks(&skill_file).map_err(|error| {
        let is_symlink = fs::symlink_metadata(&skill_file)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            vec![ValidationError::SkillFileNotRegular]
        } else {
            vec![ValidationError::ReadSkillFile { kind: error.kind() }]
        }
    })?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| vec![ValidationError::ReadSkillFile { kind: error.kind() }])?;
    if !opened_metadata.is_file() || opened_metadata.file_type().is_symlink() {
        return Err(vec![ValidationError::SkillFileNotRegular]);
    }

    let mut bytes = Vec::with_capacity(file_metadata.len() as usize);
    file.take((MAX_SKILL_MD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| vec![ValidationError::ReadSkillFile { kind: error.kind() }])?;
    if bytes.len() > MAX_SKILL_MD_BYTES {
        return Err(vec![ValidationError::SkillFileTooLarge {
            maximum_bytes: MAX_SKILL_MD_BYTES,
        }]);
    }
    let content =
        std::str::from_utf8(&bytes).map_err(|_| vec![ValidationError::SkillFileNotUtf8])?;
    let frontmatter = extract_frontmatter(content)?;
    let value = parse_frontmatter(frontmatter)?;
    validate_metadata(value, directory_name)
}

fn open_without_following_symlinks(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn extract_frontmatter(content: &str) -> Result<&str, Vec<ValidationError>> {
    let Some(first_newline) = content.find('\n') else {
        let first_line = content.strip_suffix('\r').unwrap_or(content);
        return if first_line == "---" {
            Err(vec![ValidationError::MissingClosingDelimiter])
        } else {
            Err(vec![ValidationError::MissingOpeningDelimiter])
        };
    };
    let first_line = content[..first_newline]
        .strip_suffix('\r')
        .unwrap_or(&content[..first_newline]);
    if first_line != "---" {
        return Err(vec![ValidationError::MissingOpeningDelimiter]);
    }

    let yaml_start = first_newline + 1;
    let mut line_start = yaml_start;
    while line_start <= content.len() {
        let line_end = content[line_start..]
            .find('\n')
            .map(|offset| line_start + offset)
            .unwrap_or(content.len());
        let line = content[line_start..line_end]
            .strip_suffix('\r')
            .unwrap_or(&content[line_start..line_end]);
        if line == "---" {
            return Ok(&content[yaml_start..line_start]);
        }
        if line_end == content.len() {
            break;
        }
        line_start = line_end + 1;
    }
    Err(vec![ValidationError::MissingClosingDelimiter])
}

fn parse_frontmatter(frontmatter: &str) -> Result<Value, Vec<ValidationError>> {
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(frontmatter.len()),
            max_events: MAX_FRONTMATTER_EVENTS,
            max_aliases: 0,
            max_anchors: 0,
            max_recorded_anchor_events: 0,
            max_recorded_anchor_bytes: 0,
            max_depth: MAX_FRONTMATTER_DEPTH,
            flow_nesting_limit: MAX_FRONTMATTER_DEPTH,
            max_documents: 1,
            max_nodes: MAX_FRONTMATTER_NODES,
            max_total_scalar_bytes: MAX_SKILL_MD_BYTES,
            max_merge_keys: 0,
            max_inclusion_depth: 0,
        },
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        with_snippet: false,
    };

    from_str_with_options::<Value>(frontmatter, options).map_err(|error| {
        error
            .location()
            .map(|location| {
                vec![ValidationError::InvalidYamlAt {
                    line: location.line(),
                    column: location.column(),
                }]
            })
            .unwrap_or_else(|| vec![ValidationError::InvalidYaml])
    })
}

fn validate_metadata(
    value: Value,
    directory_name: &str,
) -> Result<SkillMetadata, Vec<ValidationError>> {
    let Value::Object(mut fields) = value else {
        return Err(vec![ValidationError::FrontmatterNotMapping]);
    };
    let mut errors = Vec::new();

    for field in fields.keys() {
        if !ALLOWED_FIELDS.contains(&field.as_str()) {
            errors.push(ValidationError::UnknownField {
                field: field.clone(),
            });
        }
    }

    let name = required_string(&mut fields, "name", &mut errors);
    let description = required_string(&mut fields, "description", &mut errors);
    let license = optional_string(&mut fields, "license", &mut errors);
    let compatibility = optional_string(&mut fields, "compatibility", &mut errors);
    let allowed_tools = optional_string(&mut fields, "allowed-tools", &mut errors);
    let metadata = parse_metadata_map(fields.remove("metadata"), &mut errors);

    if let Some(name) = name.as_deref() {
        validate_name(name, directory_name, &mut errors);
    }
    if let Some(description) = description.as_deref() {
        validate_description(description, &mut errors);
    }
    if let Some(compatibility) = compatibility.as_deref() {
        validate_compatibility(compatibility, &mut errors);
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    let Some(name) = name else {
        return Err(vec![ValidationError::MissingField { field: "name" }]);
    };
    let Some(description) = description else {
        return Err(vec![ValidationError::MissingField {
            field: "description",
        }]);
    };

    Ok(SkillMetadata {
        name,
        description,
        license,
        compatibility,
        metadata,
        allowed_tools,
    })
}

fn required_string(
    fields: &mut serde_json::Map<String, Value>,
    field: &'static str,
    errors: &mut Vec<ValidationError>,
) -> Option<String> {
    match fields.remove(field) {
        None => {
            errors.push(ValidationError::MissingField { field });
            None
        }
        Some(Value::String(value)) => Some(value),
        Some(_) => {
            errors.push(ValidationError::WrongFieldType {
                field: field.to_owned(),
            });
            None
        }
    }
}

fn optional_string(
    fields: &mut serde_json::Map<String, Value>,
    field: &'static str,
    errors: &mut Vec<ValidationError>,
) -> Option<String> {
    match fields.remove(field) {
        None => None,
        Some(Value::String(value)) => Some(value),
        Some(_) => {
            errors.push(ValidationError::WrongFieldType {
                field: field.to_owned(),
            });
            None
        }
    }
}

fn parse_metadata_map(
    value: Option<Value>,
    errors: &mut Vec<ValidationError>,
) -> BTreeMap<String, String> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Value::Object(entries) = value else {
        errors.push(ValidationError::MetadataMustBeStringMap);
        return BTreeMap::new();
    };
    if entries.len() > MAX_METADATA_ENTRIES {
        errors.push(ValidationError::TooManyMetadataEntries {
            maximum: MAX_METADATA_ENTRIES,
            actual: entries.len(),
        });
    }

    let mut metadata = BTreeMap::new();
    for (key, value) in entries {
        match value {
            Value::String(value) => {
                metadata.insert(key, value);
            }
            _ => errors.push(ValidationError::MetadataValueMustBeString { key }),
        }
    }
    metadata
}

fn validate_name(name: &str, directory_name: &str, errors: &mut Vec<ValidationError>) {
    let length = name.chars().count();
    if length == 0 {
        errors.push(ValidationError::EmptyField { field: "name" });
    }
    if length > MAX_SKILL_NAME_LENGTH {
        errors.push(ValidationError::FieldTooLong {
            field: "name",
            maximum: MAX_SKILL_NAME_LENGTH,
            actual: length,
        });
    }

    let mut previous_was_hyphen = true;
    let mut valid = true;
    for character in name.chars() {
        if character == '-' {
            if previous_was_hyphen {
                valid = false;
            }
            previous_was_hyphen = true;
        } else if character.is_alphanumeric() {
            previous_was_hyphen = false;
        } else {
            valid = false;
            previous_was_hyphen = false;
        }
    }
    if previous_was_hyphen || !name.chars().flat_map(char::to_lowercase).eq(name.chars()) {
        valid = false;
    }
    if !valid {
        errors.push(ValidationError::InvalidNameGrammar);
    }
    if name != directory_name {
        errors.push(ValidationError::NameDirectoryMismatch);
    }
}

fn validate_description(description: &str, errors: &mut Vec<ValidationError>) {
    let length = description.chars().count();
    if description.trim().is_empty() {
        errors.push(ValidationError::EmptyField {
            field: "description",
        });
    }
    if length > MAX_DESCRIPTION_LENGTH {
        errors.push(ValidationError::FieldTooLong {
            field: "description",
            maximum: MAX_DESCRIPTION_LENGTH,
            actual: length,
        });
    }
}

fn validate_compatibility(compatibility: &str, errors: &mut Vec<ValidationError>) {
    let length = compatibility.chars().count();
    if length == 0 {
        errors.push(ValidationError::EmptyField {
            field: "compatibility",
        });
    }
    if length > MAX_COMPATIBILITY_LENGTH {
        errors.push(ValidationError::FieldTooLong {
            field: "compatibility",
            maximum: MAX_COMPATIBILITY_LENGTH,
            actual: length,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::{
        MAX_METADATA_ENTRIES, MAX_SKILL_MD_BYTES, SkillMetadata, ValidationError,
        validate_skill_directory,
    };

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir {
        root: PathBuf,
        skill: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "thrn-skill-validator-{}-{sequence}-{name}",
                std::process::id()
            ));
            fs::create_dir(&root).expect("isolated fixture directory should be created");
            let skill = root.join(name);
            fs::create_dir(&skill).expect("skill directory should be created");
            Self { root, skill }
        }

        fn write(&self, content: &str) {
            fs::write(self.skill.join("SKILL.md"), content)
                .expect("SKILL.md fixture should be written");
        }

        fn path(&self) -> &std::path::Path {
            &self.skill
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn valid_content(name: &str) -> String {
        format!("---\nname: {name}\ndescription: A valid skill.\n---\n\n# Body\n")
    }

    fn validate_content(name: &str, content: &str) -> Result<SkillMetadata, Vec<ValidationError>> {
        let directory = TestDir::new(name);
        directory.write(content);
        validate_skill_directory(directory.path())
    }

    #[test]
    fn accepts_minimal_yaml_and_preserves_arbitrary_markdown_body() {
        let metadata = validate_content(
            "valid-skill",
            "---\nname: valid-skill\ndescription: A useful skill.\n---\n\n# Body\n\n```yaml\nnot: frontmatter\n```\n",
        )
        .expect("valid metadata should pass");
        assert_eq!(metadata.name, "valid-skill");
        assert_eq!(metadata.description, "A useful skill.");
        assert!(metadata.metadata.is_empty());
    }

    #[test]
    fn accepts_standard_optional_fields_and_unicode_lowercase_names() {
        let metadata = validate_content(
            "résumé-工具",
            "---\nname: résumé-工具\ndescription: A skill with non-ASCII letters.\nlicense: Apache-2.0\ncompatibility: Requires a recent editor.\nmetadata:\n  author: example-org\n  version: \"1.2\"\nallowed-tools: Read Bash(git:*)\n---\n",
        )
        .expect("spec-compatible optional fields should pass");
        assert_eq!(metadata.name, "résumé-工具");
        assert_eq!(metadata.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(
            metadata.compatibility.as_deref(),
            Some("Requires a recent editor.")
        );
        assert_eq!(metadata.allowed_tools.as_deref(), Some("Read Bash(git:*)"));
        assert_eq!(
            metadata.metadata.get("author").map(String::as_str),
            Some("example-org")
        );
    }

    #[test]
    fn reports_missing_fields_types_unknowns_and_invalid_names() {
        let errors = validate_content(
            "valid-skill",
            "---\nname: [wrong]\ndescription: 42\nfuture-field: true\n---\n",
        )
        .expect_err("invalid metadata should fail");
        assert!(errors.contains(&ValidationError::WrongFieldType {
            field: "name".to_owned()
        }));
        assert!(errors.contains(&ValidationError::WrongFieldType {
            field: "description".to_owned()
        }));
        assert!(errors.contains(&ValidationError::UnknownField {
            field: "future-field".to_owned()
        }));

        let errors = validate_content("Bad--Name", &valid_content("Bad--Name"))
            .expect_err("bad skill name should fail");
        assert!(errors.contains(&ValidationError::InvalidNameGrammar));
    }

    #[test]
    fn rejects_yaml_aliases_tags_duplicate_keys_and_complex_metadata_values() {
        for frontmatter in [
            "name: valid-skill\ndescription: &desc useful\ncompatibility: *desc\n",
            "name: !custom value\ndescription: useful\n",
            "name: valid-skill\nname: duplicate\ndescription: useful\n",
            "name: valid-skill\ndescription: useful\nmetadata:\n  version: 1\n",
            "name: valid-skill\ndescription: useful\n<<: {extra: value}\n",
        ] {
            let content = format!("---\n{frontmatter}---\n");
            assert!(
                validate_content("valid-skill", &content).is_err(),
                "frontmatter should be rejected: {frontmatter}"
            );
        }

        let excessive_nesting = format!(
            "name: valid-skill\ndescription: useful\nmetadata: {}0{}\n",
            "[".repeat(super::MAX_FRONTMATTER_DEPTH + 1),
            "]".repeat(super::MAX_FRONTMATTER_DEPTH + 1),
        );
        assert!(
            validate_content("valid-skill", &format!("---\n{excessive_nesting}---\n")).is_err()
        );
    }

    #[test]
    fn enforces_specification_lengths_and_metadata_collection_budget() {
        let long_name = "a".repeat(65);
        let errors = validate_content(&long_name, &valid_content(&long_name))
            .expect_err("long name should fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::FieldTooLong {
                field: "name",
                maximum: 64,
                ..
            }
        )));

        let description = "d".repeat(1_025);
        let content = format!("---\nname: valid-skill\ndescription: {description}\n---\n");
        let errors =
            validate_content("valid-skill", &content).expect_err("long description should fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::FieldTooLong {
                field: "description",
                maximum: 1_024,
                ..
            }
        )));

        let compatibility = "c".repeat(501);
        let content = format!(
            "---\nname: valid-skill\ndescription: valid\ncompatibility: {compatibility}\n---\n"
        );
        let errors =
            validate_content("valid-skill", &content).expect_err("long compatibility should fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::FieldTooLong {
                field: "compatibility",
                maximum: 500,
                ..
            }
        )));

        let entries = (0..=MAX_METADATA_ENTRIES)
            .map(|index| format!("  key-{index}: value\n"))
            .collect::<String>();
        let content =
            format!("---\nname: valid-skill\ndescription: valid\nmetadata:\n{entries}---\n");
        let errors = validate_content("valid-skill", &content)
            .expect_err("too many metadata entries should fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::TooManyMetadataEntries {
                maximum: MAX_METADATA_ENTRIES,
                ..
            }
        )));
    }

    #[test]
    fn rejects_missing_or_malformed_frontmatter_and_oversized_files() {
        assert!(matches!(
            validate_content("valid-skill", ""),
            Err(errors) if errors.contains(&ValidationError::MissingOpeningDelimiter)
        ));
        assert!(matches!(
            validate_content("valid-skill", "---\nname: valid-skill\n"),
            Err(errors) if errors.contains(&ValidationError::MissingClosingDelimiter)
        ));
        assert!(matches!(
            validate_content("valid-skill", "---\nname: [\ndescription: x\n---\n"),
            Err(errors) if errors.iter().any(|error| matches!(error, ValidationError::InvalidYamlAt { .. } | ValidationError::InvalidYaml))
        ));

        let directory = TestDir::new("valid-skill");
        fs::write(
            directory.path().join("SKILL.md"),
            vec![b'x'; MAX_SKILL_MD_BYTES + 1],
        )
        .expect("oversized fixture should be written");
        assert!(matches!(
            validate_skill_directory(directory.path()),
            Err(errors) if errors.contains(&ValidationError::SkillFileTooLarge { maximum_bytes: MAX_SKILL_MD_BYTES })
        ));
    }

    #[test]
    fn rejects_nonregular_skill_files_and_leaves_all_files_unchanged() {
        let directory = TestDir::new("valid-skill");
        fs::create_dir(directory.path().join("SKILL.md"))
            .expect("directory fixture should be created");
        assert!(matches!(
            validate_skill_directory(directory.path()),
            Err(errors) if errors.contains(&ValidationError::SkillFileNotRegular)
        ));

        let directory = TestDir::new("valid-skill");
        directory.write(&valid_content("valid-skill"));
        fs::create_dir(directory.path().join("scripts")).expect("supporting directory");
        fs::write(directory.path().join("scripts/run.sh"), "echo untouched\n")
            .expect("supporting file");
        let before = fs::read(directory.path().join("SKILL.md")).expect("read initial skill");
        let script_before =
            fs::read(directory.path().join("scripts/run.sh")).expect("read supporting file");
        validate_skill_directory(directory.path()).expect("valid skill should pass");
        assert_eq!(fs::read(directory.path().join("SKILL.md")).unwrap(), before);
        assert_eq!(
            fs::read(directory.path().join("scripts/run.sh")).unwrap(),
            script_before
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_skill_file_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let parent = TestDir::new("symlink-parent");
        let directory = parent.path().join("valid-skill");
        fs::create_dir(&directory).expect("skill directory");
        let external = parent.path().join("outside.md");
        fs::write(&external, valid_content("valid-skill")).expect("external file");
        symlink(&external, directory.join("SKILL.md")).expect("symlink fixture");

        assert!(matches!(
            validate_skill_directory(&directory),
            Err(errors) if errors.contains(&ValidationError::SkillFileNotRegular)
        ));
    }
}
