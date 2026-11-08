use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use ring::digest::{Context, SHA256, digest};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir {
    root: PathBuf,
    project: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let parent = std::env::temp_dir();
        loop {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("thrn-cli-test-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let project = path.join("project");
                    fs::create_dir(&project).expect("test project directory should be created");
                    return Self {
                        root: path,
                        project,
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create isolated test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.project
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn command_in(directory: &TestDir, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_thrn"));
    command.args(args).current_dir(directory.path()).env_clear();
    command.env("HOME", directory.root());
    command.env("XDG_CACHE_HOME", directory.root().join("xdg-cache"));
    command.env("LOCALAPPDATA", directory.root().join("local-app-data"));
    command.env("THRN_CACHE_DIR", directory.root().join("thrn-cache"));
    command.env("THRN_DATA_DIR", directory.root().join("thrn-data"));
    command.env("ALL_PROXY", "http://127.0.0.1:0");
    command.env("HTTPS_PROXY", "http://127.0.0.1:0");
    command.env("NO_PROXY", "");

    // Windows may need SystemRoot to launch a child process. This is an OS
    // setting, not a credential or user-specific application configuration.
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }

    command
}

fn cache_file(directory: &TestDir) -> PathBuf {
    directory.root().join("thrn-cache/catalog-v1.cache")
}

fn data_directory(directory: &TestDir) -> PathBuf {
    directory.root().join("thrn-data")
}

fn receipt_directory(directory: &TestDir) -> PathBuf {
    data_directory(directory).join("receipts")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = digest(&SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn receipt_filename(project_root: &str, resource_id: &str) -> String {
    let mut context = Context::new(&SHA256);
    context.update(b"THRN-RECEIPT-IDENTITY-V1\0");
    for field in [project_root.as_bytes(), b"codex", resource_id.as_bytes()] {
        context.update(&(field.len() as u64).to_be_bytes());
        context.update(field);
    }
    let value = context.finish();
    let hex = value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("v1-{hex}.json")
}

fn receipt_json(project_root: &str, resource_id: &str, skill_name: &str, identity: u64) -> Vec<u8> {
    let content = b"abc";
    let file_digest = digest(&SHA256, content);
    let mut content_context = Context::new(&SHA256);
    content_context.update(b"THRN-CONTENT-INVENTORY-V1\0");
    content_context.update(&0_u64.to_be_bytes());
    content_context.update(&1_u64.to_be_bytes());
    content_context.update(b"F");
    content_context.update(&("SKILL.md".len() as u64).to_be_bytes());
    content_context.update(b"SKILL.md");
    content_context.update(&(content.len() as u64).to_be_bytes());
    content_context.update(file_digest.as_ref());
    let content_digest = format!("sha256:{}", {
        let value = content_context.finish();
        value
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });

    let destination = std::path::Path::new(project_root)
        .join(".agents/skills")
        .join(skill_name)
        .to_string_lossy()
        .into_owned();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "catalogResourceId": resource_id,
        "skillName": skill_name,
        "agent": "codex",
        "scope": "project",
        "canonicalProjectRoot": project_root,
        "projectRootIdentity": { "volume": 1, "object": identity },
        "destination": destination,
        "installedAtUnixSeconds": 1,
        "sourceOwner": "example-org",
        "sourceRepository": "catalog-test-fixtures",
        "sourceCommit": "2f68bee292a8c68346b4294fb31d9563eefe1ade",
        "sourceDirectory": "skills/uncharted-ideation",
        "resourceVersion": "1.0.0",
        "directories": [],
        "files": [{
            "path": "SKILL.md",
            "byteCount": content.len(),
            "sha256": format!("sha256:{}", sha256_hex(content))
        }],
        "fileCount": 1,
        "byteCount": content.len(),
        "contentDigest": content_digest
    }))
    .expect("valid receipt JSON")
}

fn receipt_json_with_install_identity(
    project_root: &str,
    resource_id: &str,
    skill_name: &str,
    project_identity: (u64, u64),
    destination_identity: (u64, u64),
) -> Vec<u8> {
    let content = b"abc";
    let file_digest = digest(&SHA256, content);
    let mut content_context = Context::new(&SHA256);
    content_context.update(b"THRN-CONTENT-INVENTORY-V2\0");
    content_context.update(&destination_identity.0.to_be_bytes());
    content_context.update(&destination_identity.1.to_be_bytes());
    content_context.update(&0_u64.to_be_bytes());
    content_context.update(&1_u64.to_be_bytes());
    content_context.update(b"F");
    content_context.update(&("SKILL.md".len() as u64).to_be_bytes());
    content_context.update(b"SKILL.md");
    content_context.update(&(content.len() as u64).to_be_bytes());
    content_context.update(file_digest.as_ref());
    let content_digest = format!("sha256:{}", {
        let value = content_context.finish();
        value
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });
    let destination = std::path::Path::new(project_root)
        .join(".agents/skills")
        .join(skill_name)
        .to_string_lossy()
        .into_owned();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "catalogResourceId": resource_id,
        "skillName": skill_name,
        "agent": "codex",
        "scope": "project",
        "canonicalProjectRoot": project_root,
        "projectRootIdentity": { "volume": project_identity.0, "object": project_identity.1 },
        "destination": destination,
        "installedDirectoryIdentity": {
            "volume": destination_identity.0,
            "object": destination_identity.1
        },
        "installedAtUnixSeconds": 1,
        "sourceOwner": "example-org",
        "sourceRepository": "catalog-test-fixtures",
        "sourceCommit": "2f68bee292a8c68346b4294fb31d9563eefe1ade",
        "sourceDirectory": "skills/uncharted-ideation",
        "resourceVersion": "1.0.0",
        "directories": [],
        "files": [{
            "path": "SKILL.md",
            "byteCount": content.len(),
            "sha256": format!("sha256:{}", sha256_hex(content))
        }],
        "fileCount": 1,
        "byteCount": content.len(),
        "contentDigest": content_digest
    }))
    .expect("valid identity-bound receipt JSON")
}

fn write_receipt(
    directory: &TestDir,
    project_root: &str,
    resource_id: &str,
    skill_name: &str,
    identity: u64,
) {
    let receipts = receipt_directory(directory);
    fs::create_dir_all(&receipts).expect("receipt storage should be created");
    fs::write(
        receipts.join(receipt_filename(project_root, resource_id)),
        receipt_json(project_root, resource_id, skill_name, identity),
    )
    .expect("receipt should be written");
}

fn write_receipt_with_install_identity(
    directory: &TestDir,
    project_root: &str,
    resource_id: &str,
    skill_name: &str,
    project_identity: (u64, u64),
    destination_identity: (u64, u64),
) {
    let receipts = receipt_directory(directory);
    fs::create_dir_all(&receipts).expect("receipt storage should be created");
    fs::write(
        receipts.join(receipt_filename(project_root, resource_id)),
        receipt_json_with_install_identity(
            project_root,
            resource_id,
            skill_name,
            project_identity,
            destination_identity,
        ),
    )
    .expect("identity-bound receipt should be written");
}

fn project_filesystem_identity(path: &std::path::Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).expect("project metadata is available");
        (metadata.dev(), metadata.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path).expect("project directory opens safely");
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` is a live directory handle and `information` is writable storage for
        // the documented output structure.
        let result =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
        assert_ne!(result, 0, "project directory identity is available");
        let object =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        (u64::from(information.dwVolumeSerialNumber), object)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        (0, 0)
    }
}

fn prepare_receipted_removal_fixture(directory: &TestDir, resource_id: &str) -> PathBuf {
    let root = fs::canonicalize(directory.path()).expect("canonical project path");
    let destination = root.join(".agents/skills/uncharted-ideation");
    fs::create_dir_all(&destination).expect("skill destination");
    fs::write(destination.join("SKILL.md"), b"abc").expect("skill file");
    let project_identity = project_filesystem_identity(&root);
    let destination_identity = project_filesystem_identity(&destination);
    write_receipt_with_install_identity(
        directory,
        root.to_str().expect("canonical path is UTF-8"),
        resource_id,
        "uncharted-ideation",
        project_identity,
        destination_identity,
    );
    destination
}

fn write_cache(directory: &TestDir, catalog: &[u8]) {
    let refreshed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock is after UNIX epoch")
        .as_secs();
    let cache_file = cache_file(directory);
    fs::create_dir_all(cache_file.parent().expect("cache file has a parent"))
        .expect("test cache directory should be created");
    let envelope = format!(
        "THRN-CATALOG-CACHE 1\nsource=https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json\nrefreshed_at_unix_seconds={refreshed_at}\n\n"
    );
    let mut contents = envelope.into_bytes();
    contents.extend_from_slice(catalog);
    fs::write(cache_file, contents).expect("test cache should be written");
}

fn write_check_receipt(
    directory: &TestDir,
    project: &std::path::Path,
    resource_id: &str,
    source_commit: &str,
) {
    let root = fs::canonicalize(project).expect("canonical project root");
    let destination = root.join(".agents/skills/uncharted-ideation");
    fs::create_dir_all(&destination).expect("skill destination");
    fs::write(destination.join("SKILL.md"), b"abc").expect("skill file");
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&receipt_json_with_install_identity(
            root.to_str().expect("UTF-8 project root"),
            resource_id,
            "uncharted-ideation",
            project_filesystem_identity(&root),
            project_filesystem_identity(&destination),
        ))
        .expect("valid receipt fixture");
    receipt["sourceOwner"] = serde_json::json!("Example-Org");
    receipt["sourceCommit"] = serde_json::json!(source_commit);
    let receipts = receipt_directory(directory);
    fs::create_dir_all(&receipts).expect("receipt storage");
    let writer_lock = receipts.join(".writer.lock");
    if !writer_lock.exists() {
        fs::write(writer_lock, []).expect("persistent writer lock fixture");
    }
    fs::write(
        receipts.join(receipt_filename(
            root.to_str().expect("UTF-8 project root"),
            resource_id,
        )),
        serde_json::to_vec(&receipt).expect("receipt JSON"),
    )
    .expect("write check receipt");
}

fn recursive_snapshot(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut result = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read snapshot directory") {
            let entry = entry.expect("snapshot entry");
            let path = entry.path();
            let kind = entry.file_type().expect("snapshot type");
            if kind.is_dir() {
                result.push((path.clone(), Vec::new()));
                pending.push(path);
            } else if kind.is_file() {
                result.push((path.clone(), fs::read(path).expect("snapshot file")));
            }
        }
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    result
}

fn run_cli(args: &[&str]) -> Output {
    let directory = TestDir::new();
    command_in(&directory, args)
        .output()
        .expect("the compiled thrn binary should start")
}

#[test]
fn skills_check_empty_project_uses_bundled_metadata_offline_without_creating_storage() {
    let directory = TestDir::new();
    let before = recursive_snapshot(directory.root());
    let output = command_in(&directory, &["skills", "check"])
        .output()
        .expect("compiled CLI starts with network disabled");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Offline skill update check"));
    assert!(stdout.contains("Catalog source: bundled metadata; freshness: unknown"));
    assert!(stdout.contains("No THRN-managed Codex project skills are recorded"));
    assert!(stdout.contains("no network request was made"));
    assert!(output.stderr.is_empty());
    assert_eq!(recursive_snapshot(directory.root()), before);
    assert!(!directory.root().join("thrn-cache").exists());
    assert!(!data_directory(&directory).exists());
}

#[test]
fn skills_check_compiled_cli_is_deterministic_and_limits_results_to_selected_project() {
    let directory = TestDir::new();
    let other_project = directory.path().join("other-project");
    fs::create_dir(&other_project).expect("second project");
    write_cache(
        &directory,
        include_bytes!("fixtures/catalog/test-only-valid-v1.json"),
    );
    write_check_receipt(
        &directory,
        directory.path(),
        "uncharted",
        "2f68bee292a8c68346b4294fb31d9563eefe1ade",
    );
    write_check_receipt(
        &directory,
        &other_project,
        "other-project-skill",
        "2f68bee292a8c68346b4294fb31d9563eefe1ade",
    );
    let before = recursive_snapshot(directory.root());

    let first = command_in(&directory, &["skills", "check", "--agent", "codex"])
        .output()
        .expect("compiled CLI starts");
    let second = command_in(&directory, &["skills", "check", "--agent", "codex"])
        .output()
        .expect("compiled CLI starts again");
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, second.stdout);
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: fresh"));
    assert!(stdout.contains("Resource: uncharted"));
    assert!(stdout.contains("Skill name: uncharted-ideation"));
    assert!(stdout.contains("Installed SHA: 2f68bee292a8c68346b4294fb31d9563eefe1ade"));
    assert!(stdout.contains("Catalog SHA: 2f68bee292a8c68346b4294fb31d9563eefe1ade"));
    assert!(stdout.contains("Status: Up to date"));
    assert!(!stdout.contains("other-project-skill"));
    assert!(first.stderr.is_empty());

    let project_arg = "./other-project";
    let selected = command_in(&directory, &["skills", "check", "--project", project_arg])
        .output()
        .expect("explicit project check starts");
    assert_eq!(selected.status.code(), Some(0));
    let selected_stdout = String::from_utf8_lossy(&selected.stdout);
    let canonical_other_project = fs::canonicalize(&other_project).expect("canonical project");
    assert!(selected_stdout.contains(&format!("Project: {}", canonical_other_project.display())));
    assert!(selected_stdout.contains("Resource: other-project-skill"));
    assert!(selected_stdout.contains("Status: Not in catalog"));
    assert!(!selected_stdout.contains("Resource: uncharted"));
    assert!(selected.stderr.is_empty());
    assert_eq!(recursive_snapshot(directory.root()), before);
}

#[test]
fn skills_check_reports_different_catalog_pin_and_stale_local_catalog() {
    let directory = TestDir::new();
    let mut catalog: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/catalog/test-only-valid-v1.json"))
            .expect("catalog fixture JSON");
    catalog["resources"][0]["source"]["commit"] =
        serde_json::json!("3f68bee292a8c68346b4294fb31d9563eefe1ade");
    write_cache(
        &directory,
        &serde_json::to_vec(&catalog).expect("edited catalog JSON"),
    );
    write_check_receipt(
        &directory,
        directory.path(),
        "uncharted",
        "2f68bee292a8c68346b4294fb31d9563eefe1ade",
    );
    let before = recursive_snapshot(directory.root());

    let output = command_in(&directory, &["skills", "check"])
        .output()
        .expect("compiled CLI starts");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: fresh"));
    assert!(stdout.contains("Status: Update available"));
    assert!(stdout.contains("catalog pins a different revision; Git ancestry is not inferred"));
    assert!(stdout.contains("Installed SHA: 2f68bee292a8c68346b4294fb31d9563eefe1ade"));
    assert!(stdout.contains("Catalog SHA: 3f68bee292a8c68346b4294fb31d9563eefe1ade"));
    assert!(output.stderr.is_empty());
    assert_eq!(recursive_snapshot(directory.root()), before);

    let stale = TestDir::new();
    let cache = cache_file(&stale);
    fs::create_dir_all(cache.parent().expect("cache parent")).unwrap();
    let mut bytes = b"THRN-CATALOG-CACHE 1\nsource=https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json\nrefreshed_at_unix_seconds=1\n\n".to_vec();
    bytes.extend_from_slice(include_bytes!("fixtures/catalog/test-only-valid-v1.json"));
    fs::write(cache, bytes).unwrap();
    let output = command_in(&stale, &["skills", "check"])
        .output()
        .expect("offline stale-catalog check starts");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Catalog source: local cache; freshness: stale")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn skills_check_labels_modified_installation_and_still_exits_successfully() {
    let directory = TestDir::new();
    write_cache(
        &directory,
        include_bytes!("fixtures/catalog/test-only-valid-v1.json"),
    );
    write_check_receipt(
        &directory,
        directory.path(),
        "uncharted",
        "2f68bee292a8c68346b4294fb31d9563eefe1ade",
    );
    fs::write(
        directory
            .path()
            .join(".agents/skills/uncharted-ideation/SKILL.md"),
        b"xyz",
    )
    .expect("same-length modification");
    let before = recursive_snapshot(directory.root());

    let output = command_in(&directory, &["skills", "check"])
        .output()
        .expect("compiled CLI starts");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status: Modified or unsafe installation"));
    assert!(stdout.contains("automatic update is not eligible"));
    assert!(output.stderr.is_empty());
    assert_eq!(recursive_snapshot(directory.root()), before);
}

#[test]
fn skills_check_usage_and_project_errors_have_expected_exit_codes() {
    let directory = TestDir::new();
    let invalid_agent = command_in(&directory, &["skills", "check", "--agent", "claude"])
        .output()
        .expect("compiled CLI starts");
    assert_eq!(invalid_agent.status.code(), Some(2));
    assert!(invalid_agent.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid_agent.stderr).contains("invalid value"));

    let missing_project = directory.root().join("missing-project");
    let project_arg = missing_project.to_string_lossy().into_owned();
    let invalid_project = command_in(&directory, &["skills", "check", "--project", &project_arg])
        .output()
        .expect("compiled CLI starts");
    assert_eq!(invalid_project.status.code(), Some(1));
    assert!(invalid_project.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid_project.stderr).contains("selected project"));
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
    assert!(stdout.contains("Catalog source: bundled metadata; freshness: unknown"));
    assert!(stdout.contains("Curated skill catalog (schema v1)"));
    assert!(stdout.contains("frontend-design  Frontend Design"));
    assert!(stdout.contains("typography"));
    assert!(output.stderr.is_empty());
}

#[test]
fn skills_list_shows_the_bundled_catalog_offline_and_in_canonical_order() {
    let directory = TestDir::new();
    let first = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");
    let second = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, second.stdout);
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("Catalog source: bundled metadata; freshness: unknown"));
    assert!(stdout.contains("Curated skill catalog (schema v1)"));
    assert!(stdout.contains("frontend-design  Frontend Design"));
    assert!(stdout.contains("Guidance for distinctive visual design"));
    assert!(first.stderr.is_empty());
    assert!(!directory.root().join("thrn-cache").exists());
    assert!(!data_directory(&directory).exists());
}

#[test]
fn skills_list_uses_valid_cached_catalog_without_writing_it() {
    let directory = TestDir::new();
    let fixture = include_bytes!("fixtures/catalog/test-only-valid-v1.json");
    write_cache(&directory, fixture);
    let cache_before = fs::read(cache_file(&directory)).expect("cache exists");

    let output = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: fresh"));
    assert!(stdout.contains("uncharted  Uncharted Ideation"));
    assert!(!stdout.contains("frontend-design"));
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(cache_file(&directory)).expect("cache remains present"),
        cache_before
    );
    assert!(!data_directory(&directory).exists());
}

#[test]
fn skills_list_uses_stale_cache_offline_and_reports_its_freshness() {
    let directory = TestDir::new();
    let fixture = include_bytes!("fixtures/catalog/test-only-valid-v1.json");
    let cache = cache_file(&directory);
    fs::create_dir_all(cache.parent().expect("cache has a parent"))
        .expect("cache directory should be created");
    let envelope = "THRN-CATALOG-CACHE 1\nsource=https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json\nrefreshed_at_unix_seconds=1\n\n";
    let mut contents = envelope.as_bytes().to_vec();
    contents.extend_from_slice(fixture);
    fs::write(cache, contents).expect("stale cache should be written");

    let output = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: stale"));
    assert!(stdout.contains("uncharted  Uncharted Ideation"));
    assert!(output.stderr.is_empty());
}

#[test]
fn skills_list_ignores_corrupt_cache_with_a_bundled_fallback_diagnostic() {
    let directory = TestDir::new();
    let cache = cache_file(&directory);
    fs::create_dir_all(cache.parent().expect("cache has a parent"))
        .expect("cache directory should be created");
    fs::write(cache, b"not a catalog cache").expect("corrupt cache should be written");

    let output = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Catalog source: bundled metadata; freshness: unknown"));
    assert!(stdout.contains("frontend-design  Frontend Design"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cached catalog could not be read and was ignored"));
    assert!(stderr.contains("using bundled metadata"));
}

#[test]
fn skills_list_sorts_cached_catalog_by_canonical_id_and_handles_empty_catalog() {
    let directory = TestDir::new();
    write_cache(
        &directory,
        include_bytes!("fixtures/catalog/test-only-multiple-resources-v1.json"),
    );

    let ordered = command_in(&directory, &["skills", "list"])
        .output()
        .expect("CLI process starts");
    assert_eq!(ordered.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&ordered.stdout);
    let refactor = stdout
        .find("refactor-helper")
        .expect("refactor entry is listed");
    let research = stdout
        .find("research-helper")
        .expect("research entry is listed");
    assert!(
        refactor < research,
        "catalog listing is not ID-sorted: {stdout}"
    );
    assert!(ordered.stderr.is_empty());

    let empty = TestDir::new();
    write_cache(
        &empty,
        include_bytes!("fixtures/catalog/test-only-empty-v1.json"),
    );
    let output = command_in(&empty, &["skills", "list"])
        .output()
        .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("No catalog skills are available in the selected catalog.")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn installed_listing_treats_missing_or_empty_receipt_storage_as_no_managed_installs() {
    let missing = TestDir::new();
    let catalog = command_in(&missing, &["skills", "list"])
        .output()
        .expect("catalog listing process starts");
    assert_eq!(catalog.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&catalog.stdout).contains("frontend-design"));

    let output = command_in(&missing, &["skills", "list", "--installed"])
        .output()
        .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No THRN-managed installations are recorded."));
    assert!(stdout.contains("Skills installed outside THRN are not included."));
    assert!(!stdout.contains("frontend-design"));
    assert!(output.stderr.is_empty());
    assert!(!data_directory(&missing).exists());

    let empty = TestDir::new();
    fs::create_dir_all(receipt_directory(&empty)).expect("empty receipt storage should be created");
    let output = command_in(&empty, &["skills", "list", "--installed"])
        .output()
        .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("No THRN-managed installations are recorded.")
    );
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_dir(receipt_directory(&empty)).unwrap().count(), 0);
}

#[test]
fn installed_listing_displays_valid_receipt_without_checking_project_content() {
    let directory = TestDir::new();
    let project_root = fs::canonicalize(directory.path())
        .expect("project path is canonical")
        .to_string_lossy()
        .into_owned();
    write_receipt(
        &directory,
        &project_root,
        "creative-lab",
        "uncharted-ideation",
        42,
    );

    let first = command_in(&directory, &["skills", "list", "--installed"])
        .output()
        .expect("CLI starts");
    let second = command_in(&directory, &["skills", "list", "--installed"])
        .output()
        .expect("CLI starts again");

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("creative-lab  uncharted-ideation"));
    assert!(stdout.contains("Agent: codex"));
    assert!(stdout.contains("Scope: project"));
    let expected_destination = PathBuf::from(&project_root)
        .join(".agents")
        .join("skills")
        .join("uncharted-ideation");
    assert!(stdout.contains(&format!(
        "Project destination: {}",
        expected_destination.display()
    )));
    assert!(stdout.contains("Pinned revision: example-org/catalog-test-fixtures@2f68bee292a8c68346b4294fb31d9563eefe1ade"));
    assert!(
        stdout.contains("Ownership: recorded by THRN; current content integrity: not checked.")
    );
    assert!(!stdout.contains("No THRN-managed installations are recorded."));
}

#[test]
fn installed_listing_orders_projects_deterministically_and_rejects_destination_claim_conflicts() {
    let directory = TestDir::new();
    let canonical_data_root = fs::canonicalize(directory.root()).expect("canonical data root");
    let first_root = canonical_data_root.join("z-project");
    let second_root = canonical_data_root.join("a-project");
    write_receipt(
        &directory,
        &first_root.to_string_lossy(),
        "z-resource",
        "skill-z",
        11,
    );
    write_receipt(
        &directory,
        &second_root.to_string_lossy(),
        "a-resource",
        "skill-a",
        12,
    );
    write_receipt(
        &directory,
        &second_root.to_string_lossy(),
        "b-resource",
        "skill-b",
        13,
    );
    let output = command_in(&directory, &["skills", "list", "--installed"])
        .output()
        .expect("CLI starts");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.find("a-resource").expect("first record")
            < stdout.find("b-resource").expect("second record")
    );
    assert!(
        stdout.find("b-resource").expect("second record")
            < stdout.find("z-resource").expect("third record")
    );
    assert!(output.stderr.is_empty());

    let conflict = TestDir::new();
    let same_root = fs::canonicalize(conflict.path())
        .expect("project path is canonical")
        .to_string_lossy()
        .into_owned();
    write_receipt(
        &conflict,
        &same_root,
        "creative-lab",
        "uncharted-ideation",
        13,
    );
    write_receipt(
        &conflict,
        &same_root,
        "other-resource",
        "uncharted-ideation",
        13,
    );
    let output = command_in(&conflict, &["skills", "list", "--installed"])
        .output()
        .expect("CLI starts");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("conflicting claims for the same Codex project destination")
    );
}

#[test]
fn installed_listing_fails_on_unrecognized_receipt_content_without_changing_it() {
    let directory = TestDir::new();
    let receipts = receipt_directory(&directory);
    fs::create_dir_all(&receipts).expect("receipt directory should be created");
    let file = receipts.join("unrecognized.json");
    fs::write(&file, b"not an M2 receipt").expect("unsupported content should be written");

    let output = command_in(&directory, &["skills", "list", "--installed"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("schema-v1 deterministic identity key"),
        "unexpected listing error: {stderr}"
    );
    assert_eq!(
        fs::read(file).expect("unrecognized content remains"),
        b"not an M2 receipt"
    );
}

#[test]
fn remove_dry_run_is_offline_read_only_and_prints_the_full_verified_plan() {
    let directory = TestDir::new();
    let destination = prepare_receipted_removal_fixture(&directory, "creative-lab");
    let root = fs::canonicalize(directory.path()).expect("canonical project root");
    let receipt_path = receipt_directory(&directory).join(receipt_filename(
        root.to_str().expect("canonical path is UTF-8"),
        "creative-lab",
    ));
    let receipt_before = fs::read(&receipt_path).expect("receipt before dry run");

    let output = command_in(
        &directory,
        &[
            "skills",
            "remove",
            "creative-lab",
            "--agent",
            "codex",
            "--dry-run",
        ],
    )
    .output()
    .expect("CLI starts");

    assert_eq!(
        output.status.code(),
        Some(0),
        "removal dry-run stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Verified THRN skill removal plan"));
    assert!(stdout.contains("Resource: creative-lab"));
    assert!(stdout.contains("Skill name: uncharted-ideation"));
    assert!(stdout.contains(&format!("Destination: {}", destination.display())));
    assert!(stdout.contains("- SKILL.md (3)"));
    assert!(stdout.contains("Files to remove: 1\nBytes to remove: 3"));
    assert!(stdout.contains("Verification: complete receipt and current content match"));
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(destination.join("SKILL.md")).unwrap(), b"abc");
    assert_eq!(fs::read(&receipt_path).unwrap(), receipt_before);
    assert!(!data_directory(&directory).join("removals").exists());
}

#[test]
fn remove_success_reports_only_after_receipt_retirement_and_installed_listing_is_empty() {
    let directory = TestDir::new();
    let destination = prepare_receipted_removal_fixture(&directory, "creative-lab");
    let expected = format!(
        "Removed THRN skill\nResource: creative-lab\nSkill name: uncharted-ideation\nAgent: codex\nScope: project\nDestination: {}\nFiles removed: 1\nBytes removed: 3\nOwnership receipt: retired\n",
        destination.display(),
    );

    let output = command_in(
        &directory,
        &["skills", "remove", "creative-lab", "--agent", "codex"],
    )
    .output()
    .expect("CLI starts");

    assert_eq!(
        output.status.code(),
        Some(0),
        "removal stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    assert!(output.stderr.is_empty());
    assert!(!destination.exists());
    assert!(directory.path().join(".agents/skills").is_dir());

    let listed = command_in(&directory, &["skills", "list", "--installed"])
        .output()
        .expect("installed listing starts");
    assert_eq!(listed.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&listed.stdout)
            .contains("No THRN-managed installations are recorded.")
    );
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("creative-lab"));
    assert!(listed.stderr.is_empty());
}

#[test]
fn remove_refuses_legacy_receipt_without_install_identity_and_preserves_content() {
    let directory = TestDir::new();
    let root = fs::canonicalize(directory.path()).expect("canonical project root");
    let destination = root.join(".agents/skills/uncharted-ideation");
    fs::create_dir_all(&destination).expect("skill destination");
    fs::write(destination.join("SKILL.md"), b"abc").expect("skill file");
    let (volume, object) = project_filesystem_identity(&root);
    write_receipt(
        &directory,
        root.to_str().expect("canonical project root is UTF-8"),
        "creative-lab",
        "uncharted-ideation",
        object,
    );
    let receipt_path = receipt_directory(&directory).join(receipt_filename(
        root.to_str().expect("canonical project root is UTF-8"),
        "creative-lab",
    ));
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt_path).expect("legacy receipt"))
            .expect("legacy JSON");
    legacy["projectRootIdentity"]["volume"] = serde_json::json!(volume);
    let legacy_bytes = serde_json::to_vec(&legacy).expect("serialize legacy receipt");
    fs::write(&receipt_path, &legacy_bytes).expect("write valid legacy receipt");

    let output = command_in(
        &directory,
        &["skills", "remove", "creative-lab", "--agent", "codex"],
    )
    .output()
    .expect("CLI starts");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no installation-time directory identity"),
        "legacy removal stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(destination.join("SKILL.md")).unwrap(), b"abc");
    assert_eq!(fs::read(receipt_path).unwrap(), legacy_bytes);
}

#[test]
fn remove_rejects_unmanaged_content_unsupported_agent_and_force_option() {
    let directory = TestDir::new();
    let unmanaged = directory.path().join(".agents/skills/uncharted-ideation");
    fs::create_dir_all(&unmanaged).expect("unmanaged skill directory");
    fs::write(unmanaged.join("SKILL.md"), b"user content").expect("unmanaged skill file");

    let missing_receipt = command_in(
        &directory,
        &["skills", "remove", "creative-lab", "--agent", "codex"],
    )
    .output()
    .expect("CLI starts");
    assert_eq!(missing_receipt.status.code(), Some(1));
    assert!(missing_receipt.stdout.is_empty());
    assert!(!missing_receipt.stderr.is_empty());
    assert_eq!(
        fs::read(unmanaged.join("SKILL.md")).unwrap(),
        b"user content"
    );

    let unsupported = command_in(
        &directory,
        &["skills", "remove", "creative-lab", "--agent", "claude"],
    )
    .output()
    .expect("CLI starts");
    assert_eq!(unsupported.status.code(), Some(1));
    assert!(unsupported.stdout.is_empty());
    assert!(!unsupported.stderr.is_empty());

    let force = command_in(
        &directory,
        &[
            "skills",
            "remove",
            "creative-lab",
            "--agent",
            "codex",
            "--force",
        ],
    )
    .output()
    .expect("CLI starts");
    assert_eq!(force.status.code(), Some(2));
    assert!(force.stdout.is_empty());
    assert!(!force.stderr.is_empty());
    assert_eq!(
        fs::read(unmanaged.join("SKILL.md")).unwrap(),
        b"user content"
    );
}

#[test]
fn skills_list_help_and_unknown_options_follow_clap_usage_semantics() {
    let help = run_cli(&["skills", "list", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("--installed"));
    assert!(help.stderr.is_empty());

    let invalid = run_cli(&["skills", "list", "--not-a-real-option"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Usage:"));
}

#[test]
fn install_requires_agent_and_identifier_arguments() {
    let missing_agent = run_cli(&["skills", "install", "frontend", "--dry-run"]);
    assert_eq!(missing_agent.status.code(), Some(2));
    assert!(missing_agent.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_agent.stderr).contains("--agent <AGENT>"));

    let missing_identifier = run_cli(&["skills", "install", "--agent", "codex", "--dry-run"]);
    assert_eq!(missing_identifier.status.code(), Some(2));
    assert!(missing_identifier.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_identifier.stderr).contains("<ID>"));
}

#[test]
fn install_without_dry_run_resolves_unknown_resource_without_writes() {
    let directory = TestDir::new();
    let output = command_in(
        &directory,
        &["skills", "install", "unknown-resource", "--agent", "codex"],
    )
    .output()
    .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no catalog entry matches"));
    assert!(!directory.path().join(".agents").exists());
    assert!(!receipt_directory(&directory).exists());
}

#[test]
fn unsupported_agent_and_invalid_id_fail_without_success_output_or_project_writes() {
    let unsupported = TestDir::new();
    let output = command_in(
        &unsupported,
        &[
            "skills",
            "install",
            "frontend",
            "--agent",
            "cursor",
            "--dry-run",
        ],
    )
    .output()
    .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only --agent codex"));
    assert!(!unsupported.path().join(".agents").exists());

    let invalid = TestDir::new();
    let output = command_in(
        &invalid,
        &[
            "skills",
            "install",
            "bad--id",
            "--agent",
            "codex",
            "--dry-run",
        ],
    )
    .output()
    .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid skill identifier"));
    assert!(!invalid.path().join(".agents").exists());
    assert!(!receipt_directory(&invalid).exists());
}

#[test]
fn install_help_distinguishes_planning_from_installation() {
    let output = run_cli(&["skills", "install", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--agent <AGENT>"));
    assert!(stdout.contains("--project <PATH>"));
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("without changing the project"));
    assert!(output.stderr.is_empty());
}

#[test]
fn update_help_exposes_only_explicit_codex_project_options() {
    let output = run_cli(&["skills", "update", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--agent <AGENT>"));
    assert!(stdout.contains("--project <PATH>"));
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("exactly verified"));
    assert!(output.stderr.is_empty());
}

#[test]
fn update_cli_reports_usage_and_runtime_failures_on_stderr_without_partial_stdout() {
    let missing_agent = run_cli(&["skills", "update", "uncharted"]);
    assert_eq!(missing_agent.status.code(), Some(2));
    assert!(missing_agent.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_agent.stderr).contains("--agent <AGENT>"));

    let directory = TestDir::new();
    let unsupported = command_in(
        &directory,
        &["skills", "update", "uncharted", "--agent", "cursor"],
    )
    .output()
    .expect("CLI process starts");
    assert_eq!(unsupported.status.code(), Some(1));
    assert!(unsupported.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("only explicit --agent codex"));
    assert!(!directory.root().join("thrn-data").exists());

    let missing_receipt = command_in(
        &directory,
        &[
            "skills",
            "update",
            "frontend-design",
            "--agent",
            "codex",
            "--dry-run",
        ],
    )
    .output()
    .expect("CLI process starts");
    assert_eq!(missing_receipt.status.code(), Some(1));
    assert!(missing_receipt.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_receipt.stderr).contains("ownership receipt"));
    assert!(!directory.path().join(".agents").exists());
}

#[test]
fn install_does_not_accept_overwrite_or_other_unlisted_options() {
    let directory = TestDir::new();
    let output = command_in(
        &directory,
        &[
            "skills",
            "install",
            "frontend-design",
            "--agent",
            "codex",
            "--force",
        ],
    )
    .output()
    .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
    assert!(!directory.path().join(".agents").exists());
    assert!(!receipt_directory(&directory).exists());
}

#[test]
fn process_uses_persisted_cache_for_search_and_info() {
    let directory = TestDir::new();
    let mut catalog: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/catalog/test-only-valid-v1.json"))
            .expect("test fixture is valid JSON");
    catalog["resources"][0]["id"] = serde_json::Value::String("remote-only".to_owned());
    let bytes = serde_json::to_vec(&catalog).expect("test catalog serializes");
    write_cache(&directory, &bytes);

    let search = command_in(&directory, &["skills", "search"])
        .output()
        .expect("CLI process starts");
    assert_eq!(search.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&search.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: fresh"));
    assert!(stdout.contains("remote-only"));
    assert!(!stdout.contains("frontend-design"));
    assert!(search.stderr.is_empty());

    let info = command_in(&directory, &["skills", "info", "remote-only"])
        .output()
        .expect("CLI process starts");
    assert_eq!(info.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Catalog source: local cache"));
    assert!(stdout.contains("Catalog freshness: fresh"));
    assert!(stdout.contains("Canonical catalog ID: remote-only"));
    assert!(info.stderr.is_empty());
}

#[test]
fn process_uses_stale_cache_offline_and_labels_it_stale() {
    let directory = TestDir::new();
    let mut catalog: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/catalog/test-only-valid-v1.json"))
            .expect("test fixture is valid JSON");
    catalog["resources"][0]["id"] = serde_json::Value::String("stale-entry".to_owned());
    let bytes = serde_json::to_vec(&catalog).expect("test catalog serializes");
    let cache_file = cache_file(&directory);
    fs::create_dir_all(cache_file.parent().expect("cache file has a parent"))
        .expect("test cache directory should be created");
    let envelope = "THRN-CATALOG-CACHE 1\nsource=https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json\nrefreshed_at_unix_seconds=1\n\n";
    let mut contents = envelope.as_bytes().to_vec();
    contents.extend_from_slice(&bytes);
    fs::write(cache_file, contents).expect("stale test cache should be written");

    let output = command_in(&directory, &["skills", "search", "stale-entry"])
        .output()
        .expect("CLI process starts");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Catalog source: local cache; freshness: stale"));
    assert!(stdout.contains("stale-entry"));
    assert!(output.stderr.is_empty());
}

#[test]
fn process_refresh_failure_uses_only_the_local_proxy_and_preserves_cache() {
    let directory = TestDir::new();
    let mut catalog: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/catalog/test-only-valid-v1.json"))
            .expect("test fixture is valid JSON");
    catalog["resources"][0]["id"] = serde_json::Value::String("persisted-entry".to_owned());
    let bytes = serde_json::to_vec(&catalog).expect("test catalog serializes");
    write_cache(&directory, &bytes);
    let cache_path = cache_file(&directory);
    let old_cache = fs::read(&cache_path).expect("cache fixture is present");

    let output = command_in(&directory, &["skills", "search", "--refresh"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("thrn:"));
    assert!(stderr.contains("HTTPS catalog"));
    assert_eq!(
        fs::read(cache_path).expect("cache remains readable"),
        old_cache
    );

    let offline = command_in(&directory, &["skills", "search"])
        .output()
        .expect("offline CLI process starts");
    assert_eq!(offline.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&offline.stdout).contains("persisted-entry"));
    assert!(offline.stderr.is_empty());
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
