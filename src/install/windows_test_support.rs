use std::{fs, os::windows::fs::MetadataExt, path::Path, process::Command};

pub(crate) fn create_junction(junction: &Path, target: &Path) {
    assert!(target.is_dir(), "junction target must be a real directory");
    assert!(
        fs::symlink_metadata(junction).is_err(),
        "junction path must not already exist: {junction:?}"
    );

    let command = "$ErrorActionPreference = 'Stop'; try { New-Item -ItemType Junction -Path $env:THRN_TEST_JUNCTION_PATH -Target $env:THRN_TEST_JUNCTION_TARGET -ErrorAction Stop | Out-Null; exit 0 } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }";
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ])
        .env("THRN_TEST_JUNCTION_PATH", junction.as_os_str())
        .env("THRN_TEST_JUNCTION_TARGET", target.as_os_str())
        .output()
        .unwrap_or_else(|error| {
            panic!("could not start PowerShell to create test junction: {error}")
        });
    assert!(
        output.status.success(),
        "Windows junction fixture creation failed (status: {:?}): stdout={:?}, stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let metadata = fs::symlink_metadata(junction)
        .unwrap_or_else(|error| panic!("created junction cannot be inspected: {error}"));
    assert_ne!(
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        0,
        "fixture must be an actual filesystem reparse point"
    );
}
