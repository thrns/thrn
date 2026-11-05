use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir {
    root: PathBuf,
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let parent = std::env::temp_dir();
        loop {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let root = parent.join(format!(
                "thrn-skill-cli-test-{}-{sequence}-{name}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let path = root.join(name);
                    fs::create_dir(&path).expect("skill directory should be created");
                    return Self { root, path };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create isolated test directory: {error}"),
            }
        }
    }

    fn write_skill(&self, contents: &str) -> PathBuf {
        fs::write(self.path.join("SKILL.md"), contents)
            .expect("SKILL.md fixture should be written");
        self.path.clone()
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn command_base(directory: &TestDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_thrn"));
    command.current_dir(directory.path()).env_clear();

    // Windows may need SystemRoot to launch a child process.
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn command_in(directory: &TestDir, skill_path: &Path) -> Command {
    let mut command = command_base(directory);
    command.args(["skills", "validate"]).arg(skill_path);
    command
}

fn run_fixture(path: &Path) -> Output {
    let working_directory = TestDir::new("cwd");
    command_in(&working_directory, path)
        .output()
        .expect("the compiled thrn binary should start")
}

fn assert_validation_error(path: &Path, message: &str) -> Output {
    let output = run_fixture(path);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "failure must not report success");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(message),
        "expected {message:?} in {stderr:?}"
    );
    assert!(!stderr.contains("panicked"), "validator panicked: {stderr}");
    output
}

#[test]
fn validates_minimal_optional_and_pinned_frontend_metadata() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills");
    let valid = run_fixture(&fixtures.join("valid-skill"));
    assert_eq!(valid.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&valid.stdout);
    assert!(stdout.contains("Skill validation passed"));
    assert!(stdout.contains("Name: valid-skill"));
    assert!(stdout.contains("A minimal synthetic skill"));
    assert!(stdout.contains("License: (not specified)"));
    assert!(valid.stderr.is_empty());

    let optional = run_fixture(&fixtures.join("optional-skill"));
    assert_eq!(optional.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&optional.stdout);
    assert!(stdout.contains("License: Apache-2.0"));
    assert!(stdout.contains("Compatibility: Requires a recent editor."));
    assert!(stdout.contains("Allowed tools: Read Bash(git:*)"));
    assert!(stdout.contains("Additional metadata entries: 2"));
    assert!(optional.stderr.is_empty());

    let frontend = run_fixture(&fixtures.join("frontend-design"));
    assert_eq!(frontend.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&frontend.stdout).contains("Name: frontend-design"));
    assert!(frontend.stderr.is_empty());
}

#[test]
fn missing_path_argument_retains_clap_usage_status() {
    let directory = TestDir::new("cwd");
    let output = command_base(&directory)
        .args(["skills", "validate"])
        .output()
        .expect("the compiled thrn binary should start");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("<PATH>"));
}

#[test]
fn reports_missing_files_empty_files_and_bad_frontmatter_on_stderr() {
    let missing = TestDir::new("missing-skill-file");
    assert_validation_error(missing.path(), "SKILL.md is missing");

    let empty = TestDir::new("empty-skill");
    empty.write_skill("");
    assert_validation_error(empty.path(), "opening --- frontmatter delimiter");

    let malformed = TestDir::new("malformed-yaml");
    malformed.write_skill("---\nname: [PRIVATE_BODY_MUST_NOT_LEAK\ndescription: useful\n---\n");
    let output = assert_validation_error(malformed.path(), "invalid YAML in SKILL.md frontmatter");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("PRIVATE_BODY_MUST_NOT_LEAK"));

    let unterminated = TestDir::new("unterminated-frontmatter");
    unterminated.write_skill("---\nname: valid-skill\ndescription: useful\n");
    assert_validation_error(unterminated.path(), "closing --- delimiter");
}

#[test]
fn reports_required_fields_types_unknown_fields_and_name_errors() {
    let missing_fields = TestDir::new("missing-fields");
    missing_fields.write_skill("---\nlicense: Apache-2.0\n---\n");
    let output = run_fixture(missing_fields.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("field `name` is required"));
    assert!(stderr.contains("field `description` is required"));

    let wrong_types = TestDir::new("wrong-types");
    wrong_types.write_skill("---\nname: [nope]\ndescription: 17\nlicense: false\ncompatibility: []\nmetadata: [invalid]\nallowed-tools: 7\n---\n");
    let output = run_fixture(wrong_types.path());
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for field in [
        "name",
        "description",
        "license",
        "compatibility",
        "allowed-tools",
    ] {
        assert!(
            stderr.contains(&format!("{field:?}")),
            "missing {field}: {stderr}"
        );
    }
    assert!(stderr.contains("metadata` must be a mapping"));

    let unknown = TestDir::new("unknown-field");
    unknown
        .write_skill("---\nname: unknown-field\ndescription: useful\nfuture-field: present\n---\n");
    assert_validation_error(
        unknown.path(),
        "unsupported frontmatter field \"future-field\"",
    );

    let invalid_grammar = TestDir::new("Bad--Name");
    invalid_grammar.write_skill("---\nname: Bad--Name\ndescription: useful\n---\n");
    assert_validation_error(invalid_grammar.path(), "lowercase Unicode letters");

    let mismatch = TestDir::new("directory-name");
    mismatch.write_skill("---\nname: another-name\ndescription: useful\n---\n");
    assert_validation_error(mismatch.path(), "exactly match the skill directory");
}

#[test]
fn enforces_name_description_compatibility_and_metadata_limits() {
    let long_name = "n".repeat(65);
    let long_name_dir = TestDir::new(&long_name);
    long_name_dir.write_skill(&format!(
        "---\nname: {long_name}\ndescription: useful\n---\n"
    ));
    assert_validation_error(
        long_name_dir.path(),
        "`name` must be no more than 64 characters",
    );

    let long_description = TestDir::new("long-description");
    let description = "d".repeat(1_025);
    long_description.write_skill(&format!(
        "---\nname: long-description\ndescription: {description}\n---\n"
    ));
    assert_validation_error(
        long_description.path(),
        "`description` must be no more than 1024 characters",
    );

    let long_compatibility = TestDir::new("long-compatibility");
    let compatibility = "c".repeat(501);
    long_compatibility.write_skill(&format!(
        "---\nname: long-compatibility\ndescription: useful\ncompatibility: {compatibility}\n---\n"
    ));
    assert_validation_error(
        long_compatibility.path(),
        "`compatibility` must be no more than 500 characters",
    );

    let too_many_metadata = TestDir::new("many-metadata");
    let entries = (0..65)
        .map(|index| format!("  key-{index}: value\n"))
        .collect::<String>();
    too_many_metadata.write_skill(&format!(
        "---\nname: many-metadata\ndescription: useful\nmetadata:\n{entries}---\n"
    ));
    assert_validation_error(
        too_many_metadata.path(),
        "metadata may contain at most 64 entries",
    );
}

#[test]
fn rejects_duplicate_keys_aliases_tags_and_oversized_files() {
    for (name, frontmatter, expected) in [
        (
            "duplicate-key",
            "name: duplicate-key\nname: duplicate-key\ndescription: useful\n",
            "invalid YAML in SKILL.md frontmatter",
        ),
        (
            "alias-skill",
            "name: alias-skill\ndescription: &desc useful\ncompatibility: *desc\n",
            "invalid YAML in SKILL.md frontmatter",
        ),
        (
            "tagged-skill",
            "name: tagged-skill\ndescription: !custom unsafe\n",
            "invalid YAML in SKILL.md frontmatter",
        ),
    ] {
        let directory = TestDir::new(name);
        directory.write_skill(&format!("---\n{frontmatter}---\n"));
        assert_validation_error(directory.path(), expected);
    }

    let oversized = TestDir::new("oversized");
    fs::write(
        oversized.path().join("SKILL.md"),
        vec![b'x'; 256 * 1024 + 1],
    )
    .expect("oversized fixture should be written");
    assert_validation_error(oversized.path(), "SKILL.md exceeds the 262144 byte limit");
}

#[test]
fn rejects_nonregular_and_unreadable_files_where_the_os_allows_reproduction() {
    let nonregular = TestDir::new("nonregular");
    fs::create_dir(nonregular.path().join("SKILL.md")).expect("directory fixture");
    assert_validation_error(nonregular.path(), "SKILL.md must be a regular file");

    #[cfg(unix)]
    {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};

        let unreadable = TestDir::new("unreadable");
        unreadable.write_skill("---\nname: unreadable\ndescription: useful\n---\n");
        fs::set_permissions(
            unreadable.path().join("SKILL.md"),
            Permissions::from_mode(0o000),
        )
        .expect("permissions should be changed for test");
        if fs::File::open(unreadable.path().join("SKILL.md")).is_err() {
            assert_validation_error(unreadable.path(), "cannot read SKILL.md");
        }
        fs::set_permissions(
            unreadable.path().join("SKILL.md"),
            Permissions::from_mode(0o600),
        )
        .expect("fixture permissions should be restored");
    }
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape_and_does_not_read_the_external_target() {
    use std::os::unix::fs::symlink;

    let parent = TestDir::new("symlink-root");
    let directory = parent.path().join("symlink-skill");
    fs::create_dir(&directory).expect("skill directory");
    let outside = parent.path().join("outside.md");
    fs::write(&outside, "PRIVATE_EXTERNAL_CONTENT\n").expect("outside fixture");
    symlink(&outside, directory.join("SKILL.md")).expect("symlink fixture");
    let output = assert_validation_error(&directory, "SKILL.md must be a regular file");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_EXTERNAL_CONTENT"));
}

#[test]
fn validation_is_read_only_and_does_not_traverse_or_execute_supporting_files() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills/valid-skill");
    let skill_before = fs::read(fixture.join("SKILL.md")).expect("read fixture");
    let script_path = fixture.join("scripts/not-run.sh");
    let script_before = fs::read(&script_path).expect("read supporting script");
    let marker = fixture.join("scripts/VALIDATOR_EXECUTED_THIS_SCRIPT");
    let _ = fs::remove_file(&marker);

    let output = run_fixture(&fixture);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert!(
        !marker.exists(),
        "validator must never execute supporting scripts"
    );
    assert_eq!(
        fs::read(fixture.join("SKILL.md")).expect("read fixture"),
        skill_before
    );
    assert_eq!(fs::read(&script_path).expect("read script"), script_before);
}
