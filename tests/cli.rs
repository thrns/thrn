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
    assert!(!stdout.contains("skills"));
    assert!(output.stderr.is_empty());
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

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage: thrn <COMMAND>"));
    assert!(!stderr.contains("panicked"));
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
