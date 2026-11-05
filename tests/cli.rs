use std::process::{Command, Output};

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_thrn"))
        .args(args)
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
    assert!(stdout.contains("Build target: "));
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
