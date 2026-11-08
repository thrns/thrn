use std::{fs, os::windows::fs::MetadataExt, path::Path, process::Command};

pub(crate) fn create_junction(junction: &Path, target: &Path) {
    assert!(target.is_dir(), "junction target must be a real directory");
    assert!(
        fs::symlink_metadata(junction).is_err(),
        "junction path must not already exist: {junction:?}"
    );

    let output = Command::new("cmd.exe")
        .arg("/D")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(junction)
        .arg(target)
        .output()
        .unwrap_or_else(|error| panic!("could not start cmd.exe to create test junction: {error}"));
    assert!(
        output.status.success(),
        "Windows junction fixture creation failed (status: {}): stdout={:?}, stderr={:?}",
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
