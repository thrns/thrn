use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let parent = std::env::temp_dir();
        loop {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("thrn-cli-test-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create isolated test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command_in(directory: &TestDir, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_thrn"));
    command.args(args).current_dir(directory.path()).env_clear();

    // Windows may need SystemRoot to launch a child process. This is an OS
    // setting, not a credential or user-specific application configuration.
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }

    command
}

fn run_cli(args: &[&str]) -> Output {
    let directory = TestDir::new();
    command_in(&directory, args)
        .output()
        .expect("the compiled thrn binary should start")
}

#[test]
fn help_shows_the_implemented_command_surface() {
    let output = run_cli(&["--help"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("local-first developer CLI"));
    assert!(stdout.contains("doctor"));
    assert!(stdout.contains("skills"));
    assert!(output.stderr.is_empty());
}

#[test]
fn empty_skill_search_lists_the_bundled_catalog_offline() {
    let output = run_cli(&["skills", "search"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Curated skill catalog (schema v1; bundled static metadata"));
    assert!(stdout.contains("frontend-design  Frontend Design"));
    assert!(stdout.contains("typography"));
    assert!(output.stderr.is_empty());
}

#[test]
fn skill_search_matches_aliases_and_descriptions_case_insensitively() {
    for query in ["FRONTEND", "TYPOGRAPHY"] {
        let output = run_cli(&["skills", "search", query]);

        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("frontend-design  Frontend Design"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn skill_info_resolves_alias_and_canonical_id_to_the_same_curated_record() {
    let alias = run_cli(&["skills", "info", "frontend"]);
    let canonical = run_cli(&["skills", "info", "frontend-design"]);

    assert_eq!(alias.status.code(), Some(0));
    assert_eq!(canonical.status.code(), Some(0));
    assert_eq!(alias.stdout, canonical.stdout);
    let stdout = String::from_utf8_lossy(&alias.stdout);
    for expected in [
        "Curated catalog information (schema v1)",
        "Canonical catalog ID: frontend-design",
        "Display name: Frontend Design",
        "Skill directory name: frontend-design",
        "Catalog version: unversioned",
        "Explicit aliases: frontend",
        "GitHub owner: anthropics",
        "GitHub repository: skills",
        "Source directory: skills/frontend-design",
        "Pinned commit: ca45fc496a3436251f191354091d557da31c15b5",
        "Publisher authenticity: not guaranteed by this commit pin.",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}: {stdout}");
    }
    assert!(alias.stderr.is_empty());
    assert!(canonical.stderr.is_empty());
}

#[test]
fn unknown_and_malformed_skill_ids_fail_with_actionable_diagnostics() {
    let unknown = run_cli(&["skills", "info", "no-such-skill"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(unknown.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(stderr.contains("no catalog entry matches"));
    assert!(stderr.contains("thrn skills search"));
    assert!(!stderr.contains("panicked"));

    let malformed = run_cli(&["skills", "info", "bad--identifier"]);
    assert_eq!(malformed.status.code(), Some(1));
    assert!(malformed.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(stderr.contains("invalid skill identifier"));
    assert!(stderr.contains("single hyphens"));
    assert!(!stderr.contains("panicked"));
}

#[test]
fn zero_search_matches_are_informative_and_successful() {
    let output = run_cli(&["skills", "search", "no-such-skill"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No skills matched query \"no-such-skill\"."));
    assert!(stdout.contains("thrn skills search"));
    assert!(output.stderr.is_empty());
}

#[test]
fn skill_search_results_have_stable_order_and_missing_info_id_is_usage_error() {
    let first = run_cli(&["skills", "search"]);
    let second = run_cli(&["skills", "search"]);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, second.stdout);

    let missing = run_cli(&["skills", "info"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("Usage:"));
    assert!(stderr.contains("<ID>"));
}

#[test]
fn version_comes_from_the_crate_metadata() {
    let output = run_cli(&["--version"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("thrn {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn doctor_reports_only_read_only_build_diagnostics() {
    let output = run_cli(&["doctor"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("THRN doctor"));
    assert!(stdout.contains(&format!("Version: {}", env!("CARGO_PKG_VERSION"))));
    assert!(stdout.contains(&format!(
        "Platform: OS={}, architecture={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )));
    assert!(stdout.contains("not a full target triple"));
    assert!(stdout.contains("build metadata only"));
    assert!(stdout.contains("no filesystem or network checks"));
    assert!(output.stderr.is_empty());
}

#[test]
fn unknown_command_has_actionable_usage_error() {
    let output = run_cli(&["instal"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized subcommand"));
    assert!(stderr.contains("Usage:"));
    assert!(!stderr.contains("panicked"));
}

#[test]
fn invalid_option_has_actionable_usage_error() {
    let output = run_cli(&["doctor", "--not-a-real-option"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument"));
    assert!(stderr.contains("Usage:"));
    assert!(!stderr.contains("panicked"));
}

#[test]
fn missing_command_is_a_usage_error() {
    let output = run_cli(&[]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "unexpected exit status; stderr:\n{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "stderr should contain an actionable usage message; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("<COMMAND>"),
        "stderr should identify the required command argument; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "CLI should not panic; stderr:\n{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn stdout_io_failure_is_reported_on_stderr_with_nonzero_status() {
    use std::{os::fd::OwnedFd, os::unix::net::UnixStream, process::Stdio};

    let directory = TestDir::new();
    let (reader, writer) = UnixStream::pair().expect("socket pair should be created");
    drop(reader);

    let child = command_in(&directory, &["doctor"])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::piped())
        .spawn()
        .expect("the compiled thrn binary should start");
    let output = child
        .wait_with_output()
        .expect("the compiled thrn process should finish");

    assert!(
        !output.status.success(),
        "stdout write unexpectedly succeeded: {output:?}"
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not write command output"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"));
}
