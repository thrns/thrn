use std::{
    collections::VecDeque,
    fs,
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use zip::{CompressionMethod, System, ZipWriter, write::SimpleFileOptions};

#[path = "../src/models/catalog.rs"]
pub mod catalog_model;
#[path = "../src/registry/catalog.rs"]
pub mod registry_catalog;
#[path = "../src/skills/validator.rs"]
pub mod skill_validator;
mod models {
    pub use crate::catalog_model as catalog;
}
mod registry {
    pub use crate::registry_catalog as catalog;
}
mod skills {
    pub use crate::skill_validator as validator;
}
#[allow(dead_code)]
#[path = "../src/sources/mod.rs"]
mod sources;

use models::catalog::Resource;
use registry::catalog::parse_catalog_json;
use sources::{
    github::{
        FetchError, GithubResponse, GithubTransport, TransportError, fetch_and_stage_skill,
        fetch_and_stage_skill_with_factory,
    },
    selection::SelectionError,
    staging::{StagedSkill, TempDirFactory},
};

const REVISION: &str = "ca45fc496a3436251f191354091d557da31c15b5";
const SKILL_BODY: &[u8] = b"---\nname: demo-skill\ndescription: A synthetic test skill.\n---\nInstructions are untrusted.\n";

#[derive(Clone)]
enum ZipEntry {
    File(String, Vec<u8>),
    Directory(String),
    Symlink(String, Vec<u8>),
    Special(String, Vec<u8>),
}

fn resource(id: &str, skill_name: &str, source_path: &str) -> Resource {
    let json = format!(
        r#"{{"schemaVersion":1,"resources":[{{"id":"{id}","kind":"skill","displayName":"Demo","skillName":"{skill_name}","description":"test","version":"1","aliases":[],"source":{{"provider":"github","owner":"anthropics","repository":"skills","path":"{source_path}","commit":"{REVISION}"}}}}]}}"#
    );
    parse_catalog_json(json.as_bytes())
        .expect("resource fixture should be accepted by the real catalog parser")
        .resources()[0]
        .clone()
}

fn default_entries(root: &str, selected_directory: &str, metadata: &[u8]) -> Vec<ZipEntry> {
    let mut entries = vec![
        ZipEntry::Directory(format!("{root}/")),
        ZipEntry::File(
            format!("{root}/README.md"),
            b"unrelated repository file".to_vec(),
        ),
    ];
    let prefix = format!("{root}/{selected_directory}");
    let skill_folder = prefix.rsplit('/').next().unwrap_or("demo-skill");
    entries.extend([
        ZipEntry::Directory(format!("{root}/skills/")),
        ZipEntry::Directory(format!("{prefix}/")),
        ZipEntry::File(format!("{prefix}/SKILL.md"), metadata.to_vec()),
        ZipEntry::File(
            format!("{prefix}/references/guide.md"),
            b"reference text".to_vec(),
        ),
        ZipEntry::File(format!("{prefix}/assets/blob.bin"), vec![0, 1, 2, 255]),
        ZipEntry::File(
            format!("{prefix}/scripts/not-run.sh"),
            format!(
                "#!/bin/sh\ntouch /private/tmp/thrn-no-exec-{}\n",
                std::process::id()
            )
            .into_bytes(),
        ),
        ZipEntry::File(
            format!("{prefix}/license.txt"),
            format!("license for {skill_folder}").into_bytes(),
        ),
    ]);
    entries
}

fn archive(entries: &[ZipEntry]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries {
        match entry {
            ZipEntry::Directory(name) => writer
                .add_directory(
                    name,
                    SimpleFileOptions::default()
                        .compression_method(CompressionMethod::STORE)
                        .system(System::Unix)
                        .unix_permissions(0o040755),
                )
                .expect("fixture directory is written"),
            ZipEntry::File(name, contents) => {
                writer
                    .start_file(name, file_options())
                    .expect("fixture file starts");
                writer
                    .write_all(contents)
                    .expect("fixture content is written");
            }
            ZipEntry::Symlink(name, target) => {
                writer
                    .start_file(
                        name,
                        SimpleFileOptions::default()
                            .compression_method(CompressionMethod::STORE)
                            .system(System::Unix)
                            .external_attributes(0o120777 << 16),
                    )
                    .expect("fixture symlink starts");
                writer
                    .write_all(target)
                    .expect("fixture symlink target is written");
            }
            ZipEntry::Special(name, contents) => {
                writer
                    .start_file(
                        name,
                        SimpleFileOptions::default()
                            .compression_method(CompressionMethod::STORE)
                            .system(System::Unix)
                            .external_attributes(0o010644 << 16),
                    )
                    .expect("fixture special file starts");
                writer
                    .write_all(contents)
                    .expect("fixture special data is written");
            }
        }
    }
    writer
        .finish()
        .expect("fixture archive is complete")
        .into_inner()
}

fn file_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::DEFLATE)
        .system(System::Unix)
        .unix_permissions(0o100644)
}

fn root_name() -> String {
    format!("skills-{REVISION}")
}

fn valid_archive() -> Vec<u8> {
    archive(&default_entries(
        &root_name(),
        "skills/demo-skill",
        SKILL_BODY,
    ))
}

fn metadata_empty_dos_archive() -> Vec<u8> {
    let root = root_name();
    let skill = format!("{root}/skills/demo-skill");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let directory_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::STORE)
        .system(System::Dos)
        .external_attributes(0x10);
    for path in [
        format!("{root}/"),
        format!("{root}/skills/"),
        format!("{skill}/"),
    ] {
        writer
            .add_directory(path, directory_options)
            .expect("DOS directory is written");
    }
    let file_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::STORE)
        .system(System::Dos)
        .external_attributes(0);
    for (path, contents) in [
        (format!("{skill}/SKILL.md"), SKILL_BODY.to_vec()),
        (format!("{skill}/assets/blob.bin"), vec![0, 1, 2, 255]),
        (
            format!("{skill}/scripts/check.sh"),
            b"#!/bin/sh\nexit 0\n".to_vec(),
        ),
    ] {
        writer
            .start_file(path, file_options)
            .expect("DOS file starts");
        writer.write_all(&contents).expect("DOS file contents");
    }
    writer
        .finish()
        .expect("DOS archive is complete")
        .into_inner()
}

fn github_url(resource: &Resource) -> String {
    let source = resource.source();
    format!(
        "https://github.com/{}/{}/archive/{}.zip",
        source.owner().as_str(),
        source.repository().as_str(),
        source.commit().as_str()
    )
}

fn codeload_url(resource: &Resource) -> String {
    let source = resource.source();
    format!(
        "https://codeload.github.com/{}/{}/zip/{}",
        source.owner().as_str(),
        source.repository().as_str(),
        source.commit().as_str()
    )
}

struct FakeTransport {
    expected_urls: Mutex<VecDeque<String>>,
    responses: Mutex<VecDeque<Result<GithubResponse, TransportError>>>,
}

impl FakeTransport {
    fn new(
        expected_urls: Vec<String>,
        responses: Vec<Result<GithubResponse, TransportError>>,
    ) -> Self {
        Self {
            expected_urls: Mutex::new(expected_urls.into()),
            responses: Mutex::new(responses.into()),
        }
    }

    fn direct(resource: &Resource, response: Result<GithubResponse, TransportError>) -> Self {
        Self::new(vec![github_url(resource)], vec![response])
    }

    fn redirected(resource: &Resource, bytes: Vec<u8>) -> Self {
        let redirect = GithubResponse::new(
            302,
            None,
            None,
            Some(&codeload_url(resource)),
            Cursor::new(Vec::<u8>::new()),
        );
        Self::new(
            vec![github_url(resource), codeload_url(resource)],
            vec![Ok(redirect), Ok(zip_response(bytes, Some(200)))],
        )
    }
}

impl GithubTransport for FakeTransport {
    fn get(&self, url: &str) -> Result<GithubResponse, TransportError> {
        let expected = self
            .expected_urls
            .lock()
            .expect("test queue lock")
            .pop_front()
            .expect("unexpected extra request");
        assert_eq!(url, expected, "request used an unexpected source URL");
        self.responses
            .lock()
            .expect("test response lock")
            .pop_front()
            .expect("test response queue is complete")
    }
}

fn zip_response(bytes: Vec<u8>, status: Option<u16>) -> GithubResponse {
    let length = u64::try_from(bytes.len()).expect("fixture length fits in u64");
    GithubResponse::new(
        status.unwrap_or(200),
        Some("application/zip"),
        Some(length),
        None,
        Cursor::new(bytes),
    )
}

struct TempIn {
    parent: PathBuf,
}

impl TempDirFactory for TempIn {
    fn create_private(&self) -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix("op-")
            .tempdir_in(&self.parent)
    }
}

fn result_with_archive(bytes: Vec<u8>) -> Result<StagedSkill, FetchError> {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    fetch_and_stage_skill(&resource, &FakeTransport::redirected(&resource, bytes))
}

fn assert_selection_error(result: Result<StagedSkill, FetchError>, expected: SelectionError) {
    assert!(matches!(result, Err(FetchError::Selection(found)) if found == expected));
}

#[test]
fn stages_exact_pinned_skill_with_nested_assets_scripts_and_binary_files() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let bytes = valid_archive();
    let staged = fetch_and_stage_skill(&resource, &FakeTransport::redirected(&resource, bytes))
        .expect("pinned skill stages");

    assert_eq!(staged.catalog_id().as_str(), "catalog-short-id");
    assert_eq!(staged.skill_name().as_str(), "demo-skill");
    assert_eq!(staged.revision().as_str(), REVISION);
    assert_eq!(staged.source().repository().as_str(), "skills");
    assert_eq!(staged.metadata().name, "demo-skill");
    assert!(staged.content_directory().join("SKILL.md").is_file());
    assert_eq!(
        fs::read(staged.content_directory().join("references/guide.md")).expect("reference"),
        b"reference text"
    );
    assert_eq!(
        fs::read(staged.content_directory().join("assets/blob.bin")).expect("asset"),
        [0, 1, 2, 255]
    );
    assert_eq!(
        fs::read(staged.content_directory().join("scripts/not-run.sh")).expect("inert script"),
        format!(
            "#!/bin/sh\ntouch /private/tmp/thrn-no-exec-{}\n",
            std::process::id()
        )
        .as_bytes()
    );
    assert!(!Path::new(&format!("/private/tmp/thrn-no-exec-{}", std::process::id())).exists());
    assert!(!staged.content_directory().join("README.md").exists());
    assert!(staged.content_directory().join("license.txt").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(staged.content_directory().join("scripts/not-run.sh"))
                .expect("script metadata")
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}

#[test]
fn stages_metadata_empty_dos_files_as_ordinary_non_executable_content() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let staged = fetch_and_stage_skill(
        &resource,
        &FakeTransport::redirected(&resource, metadata_empty_dos_archive()),
    )
    .expect("narrow DOS file fallback supports GitHub-style files");
    assert_eq!(staged.metadata().name, "demo-skill");
    assert_eq!(
        fs::read(staged.content_directory().join("assets/blob.bin")).expect("binary asset"),
        [0, 1, 2, 255]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(staged.content_directory().join("scripts/check.sh"))
                .expect("script metadata")
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}

#[test]
fn selects_one_skill_from_a_repository_with_multiple_skills_and_distinct_catalog_id() {
    let resource = resource("frontend", "demo-skill", "skills/demo-skill");
    let mut entries = default_entries(&root_name(), "skills/demo-skill", SKILL_BODY);
    entries.extend([
        ZipEntry::Directory(format!("{}/skills/another-skill/", root_name())),
        ZipEntry::File(
            format!("{}/skills/another-skill/SKILL.md", root_name()),
            b"---\nname: another-skill\ndescription: Other skill.\n---\n".to_vec(),
        ),
    ]);
    let staged = fetch_and_stage_skill(
        &resource,
        &FakeTransport::redirected(&resource, archive(&entries)),
    )
    .expect("exact catalog source path is staged");
    assert_eq!(staged.catalog_id().as_str(), "frontend");
    assert_eq!(staged.metadata().name, "demo-skill");
    assert!(!staged.content_directory().join("another-skill").exists());
}

#[test]
fn temporary_content_lives_with_the_result_and_is_removed_when_dropped() {
    let parent = tempfile::tempdir().expect("test parent");
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let factory = TempIn {
        parent: parent.path().to_path_buf(),
    };
    let staged = fetch_and_stage_skill_with_factory(
        &resource,
        &FakeTransport::direct(&resource, Ok(zip_response(valid_archive(), None))),
        &factory,
    )
    .expect("valid content stages");
    assert!(staged.content_directory().exists());
    assert_eq!(fs::read_dir(parent.path()).expect("parent").count(), 1);
    drop(staged);
    assert_eq!(fs::read_dir(parent.path()).expect("parent").count(), 0);
}

#[test]
fn archive_validation_precedes_temporary_directory_creation() {
    struct ShouldNotCreate;
    impl TempDirFactory for ShouldNotCreate {
        fn create_private(&self) -> io::Result<tempfile::TempDir> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "staging must not start for an invalid archive",
            ))
        }
    }

    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let result = fetch_and_stage_skill_with_factory(
        &resource,
        &FakeTransport::direct(
            &resource,
            Ok(zip_response(b"not a ZIP archive".to_vec(), None)),
        ),
        &ShouldNotCreate,
    );
    assert!(matches!(result, Err(FetchError::Archive(_))));
}

#[test]
fn rejects_http_errors_timeouts_and_truncated_transfers_without_a_result() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let http = fetch_and_stage_skill(
        &resource,
        &FakeTransport::direct(
            &resource,
            Ok(GithubResponse::new(503, None, None, None, Cursor::new([]))),
        ),
    );
    assert!(matches!(http, Err(FetchError::HttpStatus { status: 503 })));

    let timeout = fetch_and_stage_skill(
        &resource,
        &FakeTransport::direct(&resource, Err(TransportError::Timeout)),
    );
    assert!(matches!(
        timeout,
        Err(FetchError::Transport(TransportError::Timeout))
    ));

    let bytes = valid_archive();
    let truncated = GithubResponse::new(
        200,
        Some("application/zip"),
        Some(u64::try_from(bytes.len()).expect("length") + 1),
        None,
        Cursor::new(bytes),
    );
    let result = fetch_and_stage_skill(&resource, &FakeTransport::direct(&resource, Ok(truncated)));
    assert!(matches!(result, Err(FetchError::IncompleteTransfer)));
}

#[test]
fn rejects_untrusted_redirects_redirect_chains_and_unexpected_hosts() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let cases = [
        "https://evil.example/skills.zip",
        "http://codeload.github.com/anthropics/skills/zip/ca45fc496a3436251f191354091d557da31c15b5",
        "https://codeload.github.com.evil.example/anthropics/skills/zip/ca45fc496a3436251f191354091d557da31c15b5",
        "https://codeload.github.com/anthropics/skills/zip/another-revision",
    ];
    for location in cases {
        let response = GithubResponse::new(302, None, None, Some(location), Cursor::new([]));
        let result =
            fetch_and_stage_skill(&resource, &FakeTransport::direct(&resource, Ok(response)));
        assert!(matches!(result, Err(FetchError::UnauthorizedRedirect)));
    }

    let first = GithubResponse::new(
        302,
        None,
        None,
        Some(&codeload_url(&resource)),
        Cursor::new([]),
    );
    let second = GithubResponse::new(
        302,
        None,
        None,
        Some("https://evil.example/final.zip"),
        Cursor::new([]),
    );
    let chain = FakeTransport::new(
        vec![github_url(&resource), codeload_url(&resource)],
        vec![Ok(first), Ok(second)],
    );
    assert!(matches!(
        fetch_and_stage_skill(&resource, &chain),
        Err(FetchError::UnauthorizedRedirect)
    ));
}

#[test]
fn rejects_overlarge_content_length_chunked_body_and_inconsistent_length_headers() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let declared_large = GithubResponse::new(
        200,
        Some("application/zip"),
        Some(sources::archive::MAX_COMPRESSED_ARCHIVE_BYTES + 1),
        None,
        Cursor::new([]),
    );
    assert!(matches!(
        fetch_and_stage_skill(
            &resource,
            &FakeTransport::direct(&resource, Ok(declared_large))
        ),
        Err(FetchError::DownloadTooLarge { .. })
    ));

    struct ChunkedLarge(u64);
    impl Read for ChunkedLarge {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let count = usize::try_from(self.0.min(output.len() as u64)).unwrap_or(output.len());
            output[..count].fill(0x5a);
            self.0 -= count as u64;
            Ok(count)
        }
    }
    let chunked = GithubResponse::new(
        200,
        Some("application/zip"),
        None,
        None,
        ChunkedLarge(sources::archive::MAX_COMPRESSED_ARCHIVE_BYTES + 1),
    );
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::direct(&resource, Ok(chunked))),
        Err(FetchError::DownloadTooLarge { .. })
    ));

    let mut inconsistent = GithubResponse::new(
        200,
        Some("application/zip"),
        None,
        None,
        Cursor::new(valid_archive()),
    );
    inconsistent.content_lengths = vec!["10".to_owned(), "11".to_owned()];
    assert!(matches!(
        fetch_and_stage_skill(
            &resource,
            &FakeTransport::direct(&resource, Ok(inconsistent))
        ),
        Err(FetchError::InvalidContentLength)
    ));
}

#[test]
fn rejects_unexpected_content_type_and_content_encoding() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let html = GithubResponse::new(200, Some("text/html"), None, None, Cursor::new(b"html"));
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::direct(&resource, Ok(html))),
        Err(FetchError::UnexpectedContentType)
    ));

    let mut compressed = GithubResponse::new(
        200,
        Some("application/zip"),
        None,
        None,
        Cursor::new(valid_archive()),
    );
    compressed.content_encoding = Some("gzip".to_owned());
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::direct(&resource, Ok(compressed))),
        Err(FetchError::UnexpectedContentEncoding)
    ));
}

#[test]
fn rejects_missing_ambiguous_or_incorrect_repository_roots() {
    let no_explicit_root = archive(&[
        ZipEntry::Directory(format!("{}/skills/", root_name())),
        ZipEntry::Directory(format!("{}/skills/demo-skill/", root_name())),
        ZipEntry::File(
            format!("{}/skills/demo-skill/SKILL.md", root_name()),
            SKILL_BODY.to_vec(),
        ),
    ]);
    assert_selection_error(
        result_with_archive(no_explicit_root),
        SelectionError::InvalidRepositoryRoot,
    );

    let mut ambiguous = default_entries(&root_name(), "skills/demo-skill", SKILL_BODY);
    ambiguous.push(ZipEntry::File(
        "unexpected-root/file.txt".to_owned(),
        b"x".to_vec(),
    ));
    assert_selection_error(
        result_with_archive(archive(&ambiguous)),
        SelectionError::AmbiguousRepositoryRoots,
    );

    let wrong_root = archive(&default_entries(
        "skills-main",
        "skills/demo-skill",
        SKILL_BODY,
    ));
    assert_selection_error(
        result_with_archive(wrong_root),
        SelectionError::InvalidRepositoryRoot,
    );
}

#[test]
fn rejects_missing_case_mismatched_and_file_source_paths() {
    let bytes = valid_archive();
    let missing_resource = resource("missing", "demo-skill", "skills/not-present");
    assert_selection_error(
        fetch_and_stage_skill(
            &missing_resource,
            &FakeTransport::redirected(&missing_resource, bytes.clone()),
        ),
        SelectionError::MissingSourceDirectory,
    );

    let case_resource = resource("case", "demo-skill", "Skills/demo-skill");
    assert_selection_error(
        fetch_and_stage_skill(
            &case_resource,
            &FakeTransport::redirected(&case_resource, bytes),
        ),
        SelectionError::SourcePathCaseMismatch,
    );

    let file_resource = resource("file", "demo-skill", "skills/demo-skill");
    let file_path = archive(&[
        ZipEntry::Directory(format!("{}/", root_name())),
        ZipEntry::Directory(format!("{}/skills/", root_name())),
        ZipEntry::File(
            format!("{}/skills/demo-skill", root_name()),
            b"not a directory".to_vec(),
        ),
    ]);
    assert_selection_error(
        fetch_and_stage_skill(
            &file_resource,
            &FakeTransport::redirected(&file_resource, file_path),
        ),
        SelectionError::SourcePathIsFile,
    );
}

#[test]
fn rejects_missing_case_mismatched_malformed_and_catalog_mismatched_skill_metadata() {
    let base_resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let mut missing = default_entries(&root_name(), "skills/demo-skill", b"not used");
    missing
        .retain(|entry| !matches!(entry, ZipEntry::File(path, _) if path.ends_with("/SKILL.md")));
    assert_selection_error(
        result_with_archive(archive(&missing)),
        SelectionError::MissingSkillFile,
    );

    let mut wrong_case = default_entries(&root_name(), "skills/demo-skill", SKILL_BODY);
    wrong_case
        .retain(|entry| !matches!(entry, ZipEntry::File(path, _) if path.ends_with("/SKILL.md")));
    wrong_case.push(ZipEntry::File(
        format!("{}/skills/demo-skill/skill.md", root_name()),
        SKILL_BODY.to_vec(),
    ));
    assert_selection_error(
        result_with_archive(archive(&wrong_case)),
        SelectionError::SkillFileCaseMismatch,
    );

    let malformed = b"---\nname: demo-skill\ndescription: [broken\n---\n";
    let malformed_entries = default_entries(&root_name(), "skills/demo-skill", malformed);
    assert!(
        malformed_entries
            .iter()
            .any(|entry| matches!(entry, ZipEntry::File(path, _) if path.ends_with("/SKILL.md")))
    );
    let result = fetch_and_stage_skill(
        &base_resource,
        &FakeTransport::redirected(&base_resource, archive(&malformed_entries)),
    );
    assert!(matches!(result, Err(FetchError::Staging(_))));

    let mismatched_resource = resource("catalog-short-id", "expected-name", "skills/demo-skill");
    let result = fetch_and_stage_skill(
        &mismatched_resource,
        &FakeTransport::redirected(&mismatched_resource, valid_archive()),
    );
    assert!(matches!(result, Err(FetchError::Staging(_))));
}

#[test]
fn rejects_traversal_symlink_special_duplicate_casefold_and_corrupt_archives() {
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let root = root_name();
    let traversal = archive(&[
        ZipEntry::Directory(format!("{root}/")),
        ZipEntry::File("../outside.txt".to_owned(), b"escape".to_vec()),
    ]);
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::redirected(&resource, traversal)),
        Err(FetchError::Archive(_))
    ));

    for hostile in [
        ZipEntry::Symlink(
            format!("{root}/skills/demo-skill/link"),
            b"../../outside".to_vec(),
        ),
        ZipEntry::Special(format!("{root}/skills/demo-skill/pipe"), Vec::new()),
    ] {
        let mut entries = default_entries(&root, "skills/demo-skill", SKILL_BODY);
        entries.push(hostile);
        assert!(matches!(
            fetch_and_stage_skill(
                &resource,
                &FakeTransport::redirected(&resource, archive(&entries))
            ),
            Err(FetchError::Archive(_))
        ));
    }

    let mut collision = default_entries(&root, "skills/demo-skill", SKILL_BODY);
    collision.extend([
        ZipEntry::File(
            format!("{root}/skills/demo-skill/Assets/a.bin"),
            b"a".to_vec(),
        ),
        ZipEntry::File(
            format!("{root}/skills/demo-skill/assets/b.bin"),
            b"b".to_vec(),
        ),
    ]);
    assert!(matches!(
        fetch_and_stage_skill(
            &resource,
            &FakeTransport::redirected(&resource, archive(&collision))
        ),
        Err(FetchError::Archive(_))
    ));

    let mut duplicate = archive(&[
        ZipEntry::Directory(format!("{root}/")),
        ZipEntry::File(format!("{root}/file-one"), b"one".to_vec()),
        ZipEntry::File(format!("{root}/file-two"), b"two".to_vec()),
    ]);
    duplicate_names(
        &mut duplicate,
        &format!("{root}/file-one"),
        &format!("{root}/file-two"),
    );
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::redirected(&resource, duplicate)),
        Err(FetchError::Archive(_))
    ));

    let mut corrupt = valid_archive();
    let index = corrupt.len() / 2;
    corrupt[index] ^= 0x40;
    assert!(matches!(
        fetch_and_stage_skill(&resource, &FakeTransport::redirected(&resource, corrupt)),
        Err(FetchError::Archive(_))
    ));
}

fn duplicate_names(bytes: &mut [u8], first_name: &str, second_name: &str) {
    assert_eq!(first_name.len(), second_name.len());
    replace_name_in_headers(
        bytes,
        [0x50, 0x4b, 0x03, 0x04],
        26,
        30,
        first_name,
        second_name,
    );
    replace_name_in_headers(
        bytes,
        [0x50, 0x4b, 0x01, 0x02],
        28,
        46,
        first_name,
        second_name,
    );
}

fn replace_name_in_headers(
    bytes: &mut [u8],
    signature: [u8; 4],
    name_length_offset: usize,
    name_offset: usize,
    replacement: &str,
    target: &str,
) {
    let mut replaced = false;
    for position in signature_positions(bytes, signature) {
        let length = usize::from(u16::from_le_bytes([
            bytes[position + name_length_offset],
            bytes[position + name_length_offset + 1],
        ]));
        let name_start = position + name_offset;
        let name_end = name_start + length;
        if bytes.get(name_start..name_end) == Some(target.as_bytes()) {
            bytes[name_start..name_end].copy_from_slice(replacement.as_bytes());
            replaced = true;
        }
    }
    assert!(
        replaced,
        "target ZIP entry name should exist in each header table"
    );
}

fn signature_positions(bytes: &[u8], signature: [u8; 4]) -> Vec<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter_map(|(index, candidate)| (candidate == signature).then_some(index))
        .collect()
}

#[cfg(unix)]
#[test]
fn cleans_failed_validation_and_refuses_a_symlink_at_the_staging_directory() {
    use std::os::unix::fs::symlink;

    struct SymlinkTempFactory {
        parent: PathBuf,
        outside: PathBuf,
    }
    impl TempDirFactory for SymlinkTempFactory {
        fn create_private(&self) -> io::Result<tempfile::TempDir> {
            let temporary = tempfile::Builder::new()
                .prefix("op-")
                .tempdir_in(&self.parent)?;
            symlink(&self.outside, temporary.path().join("demo-skill"))?;
            Ok(temporary)
        }
    }

    let parent = tempfile::tempdir().expect("isolated parent");
    let outside = tempfile::tempdir().expect("outside sentinel");
    let resource = resource("catalog-short-id", "demo-skill", "skills/demo-skill");
    let factory = SymlinkTempFactory {
        parent: parent.path().to_path_buf(),
        outside: outside.path().to_path_buf(),
    };
    let result = fetch_and_stage_skill_with_factory(
        &resource,
        &FakeTransport::direct(&resource, Ok(zip_response(valid_archive(), None))),
        &factory,
    );
    assert!(matches!(result, Err(FetchError::Staging(_))));
    assert_eq!(
        fs::read_dir(parent.path()).expect("staging parent").count(),
        0
    );
    assert_eq!(
        fs::read_dir(outside.path())
            .expect("outside target")
            .count(),
        0
    );
}
