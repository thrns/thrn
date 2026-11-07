//! Strict schema-v1 installation ownership and integrity record.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};

use crate::{
    install::{destination::FileIdentity, inventory::StagedInventory, plan::InstallPlan},
    models::catalog::{
        CommitSha, GithubOwner, GithubRepository, ResourceVersion, SkillId, SkillName, SourcePath,
    },
    sources::archive::{ArchiveLimits, ValidatedArchivePath},
};

pub(crate) const SCHEMA_VERSION: u64 = 1;
pub(crate) const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_RECEIPTS: usize = 4_096;
pub(crate) const MAX_RECEIPT_PATH_BYTES: usize = 32_768;
pub(crate) const MAX_METADATA_BYTES: usize = 1_024;
pub(crate) const MAX_TOTAL_CONTENT_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallationReceipt {
    schema_version: u64,
    catalog_resource_id: String,
    skill_name: String,
    agent: String,
    scope: String,
    canonical_project_root: String,
    project_root_identity: ReceiptProjectIdentity,
    destination: String,
    installed_at_unix_seconds: u64,
    source_owner: String,
    source_repository: String,
    source_commit: String,
    source_directory: String,
    resource_version: String,
    directories: Vec<String>,
    files: Vec<ReceiptFile>,
    file_count: u64,
    byte_count: u64,
    content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReceiptProjectIdentity {
    volume: u64,
    object: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReceiptFile {
    path: String,
    byte_count: u64,
    sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ReceiptValidationError {
    #[error("receipt JSON is malformed or contains unsupported fields")]
    MalformedJson,
    #[error("receipt schema version {0} is unsupported")]
    UnsupportedSchema(u64),
    #[error("receipt exceeds the {maximum}-byte input limit")]
    InputTooLarge { maximum: usize },
    #[error("receipt metadata is missing, too long, or invalid")]
    InvalidMetadata,
    #[error("receipt project paths are not canonical absolute paths")]
    InvalidProjectPath,
    #[error("receipt destination does not match the Codex project skill path")]
    DestinationMismatch,
    #[error("receipt file or directory inventory is invalid")]
    InvalidInventory,
    #[error("receipt content digest does not match its canonical inventory")]
    ContentDigestMismatch,
    #[error("receipt identity digest could not be computed")]
    IdentityDigest,
}

impl InstallationReceipt {
    pub(crate) fn from_committed_plan(
        plan: &InstallPlan,
        inventory: &StagedInventory,
        project_identity: FileIdentity,
        installed_at_unix_seconds: u64,
    ) -> Result<Self, ReceiptValidationError> {
        let mut directories = inventory
            .directories()
            .iter()
            .map(|path| relative_path_string(path))
            .collect::<Result<Vec<_>, _>>()?;
        directories.sort();

        let mut files = inventory
            .files()
            .iter()
            .map(|file| {
                Ok(ReceiptFile {
                    path: relative_path_string(file.relative_path())?,
                    byte_count: file.expected_bytes(),
                    sha256: hex_digest(file.expected_sha256()),
                })
            })
            .collect::<Result<Vec<_>, ReceiptValidationError>>()?;
        files.sort_by(|left, right| left.path.cmp(&right.path));

        let mut receipt = Self {
            schema_version: SCHEMA_VERSION,
            catalog_resource_id: plan.resource_id().as_str().to_owned(),
            skill_name: plan.skill_name().as_str().to_owned(),
            agent: plan.agent().as_str().to_owned(),
            scope: plan.scope().as_str().to_owned(),
            canonical_project_root: path_to_string(plan.project_root())?,
            project_root_identity: ReceiptProjectIdentity {
                volume: project_identity.volume(),
                object: project_identity.object(),
            },
            destination: path_to_string(plan.destination())?,
            installed_at_unix_seconds,
            source_owner: plan.source_owner().as_str().to_owned(),
            source_repository: plan.source_repository().as_str().to_owned(),
            source_commit: plan.source_commit().as_str().to_owned(),
            source_directory: plan.source_directory().as_str().to_owned(),
            resource_version: plan.resource_version().as_str().to_owned(),
            directories,
            file_count: u64::try_from(files.len())
                .map_err(|_| ReceiptValidationError::InvalidInventory)?,
            byte_count: inventory.total_bytes(),
            files,
            content_digest: String::new(),
        };
        receipt.content_digest = receipt.calculated_content_digest()?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, ReceiptValidationError> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(ReceiptValidationError::InputTooLarge {
                maximum: MAX_RECEIPT_BYTES,
            });
        }
        let receipt: Self =
            serde_json::from_slice(bytes).map_err(|_| ReceiptValidationError::MalformedJson)?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn to_json(&self) -> Result<Vec<u8>, ReceiptValidationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| ReceiptValidationError::MalformedJson)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(ReceiptValidationError::InputTooLarge {
                maximum: MAX_RECEIPT_BYTES,
            });
        }
        Ok(bytes)
    }

    pub(crate) fn validate(&self) -> Result<(), ReceiptValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ReceiptValidationError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        if self.agent != "codex" || self.scope != "project" {
            return Err(ReceiptValidationError::InvalidMetadata);
        }
        SkillId::from_str(&self.catalog_resource_id)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        let skill_name = SkillName::from_str(&self.skill_name)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        GithubOwner::from_str(&self.source_owner)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        GithubRepository::from_str(&self.source_repository)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        CommitSha::from_str(&self.source_commit)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        SourcePath::from_str(&self.source_directory)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;
        ResourceVersion::from_str(&self.resource_version)
            .map_err(|_| ReceiptValidationError::InvalidMetadata)?;

        if self.canonical_project_root.len() > MAX_RECEIPT_PATH_BYTES
            || self.destination.len() > MAX_RECEIPT_PATH_BYTES
            || self.source_owner.len() > MAX_METADATA_BYTES
            || self.source_repository.len() > MAX_METADATA_BYTES
            || self.source_directory.len() > MAX_METADATA_BYTES
            || self.resource_version.len() > MAX_METADATA_BYTES
        {
            return Err(ReceiptValidationError::InvalidMetadata);
        }

        let root = validate_absolute_path(&self.canonical_project_root)?;
        let destination = validate_absolute_path(&self.destination)?;
        let expected_destination = root
            .join(".agents")
            .join("skills")
            .join(skill_name.as_str());
        if destination != expected_destination {
            return Err(ReceiptValidationError::DestinationMismatch);
        }

        if self.files.is_empty()
            || self.files.len() > ArchiveLimits::DEFAULT.max_entries
            || self.directories.len() + self.files.len() > ArchiveLimits::DEFAULT.max_entries
        {
            return Err(ReceiptValidationError::InvalidInventory);
        }
        if self.file_count != self.files.len() as u64
            || self.byte_count > MAX_TOTAL_CONTENT_BYTES
            || self.directories.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .files
                .windows(2)
                .any(|pair| pair[0].path >= pair[1].path)
        {
            return Err(ReceiptValidationError::InvalidInventory);
        }

        let mut paths = BTreeMap::<String, String>::new();
        let mut directory_set = BTreeSet::new();
        for directory in &self.directories {
            validate_relative_path(directory)?;
            let collision_key = directory.to_ascii_lowercase();
            if paths.insert(collision_key, directory.clone()).is_some() {
                return Err(ReceiptValidationError::InvalidInventory);
            }
            directory_set.insert(directory.as_str());
        }

        let mut total = 0_u64;
        let mut skill_file_present = false;
        for file in &self.files {
            validate_relative_path(&file.path)?;
            if file.byte_count > ArchiveLimits::DEFAULT.max_file_uncompressed_bytes
                || !is_sha256(&file.sha256)
            {
                return Err(ReceiptValidationError::InvalidInventory);
            }
            let collision_key = file.path.to_ascii_lowercase();
            if paths.insert(collision_key, file.path.clone()).is_some() {
                return Err(ReceiptValidationError::InvalidInventory);
            }
            for parent in relative_parent_strings(&file.path) {
                if !directory_set.contains(parent.as_str()) {
                    return Err(ReceiptValidationError::InvalidInventory);
                }
            }
            skill_file_present |= file.path == "SKILL.md";
            total = total
                .checked_add(file.byte_count)
                .filter(|sum| *sum <= MAX_TOTAL_CONTENT_BYTES)
                .ok_or(ReceiptValidationError::InvalidInventory)?;
        }
        for directory in &self.directories {
            for parent in relative_parent_strings(directory) {
                if !directory_set.contains(parent.as_str()) {
                    return Err(ReceiptValidationError::InvalidInventory);
                }
            }
        }
        if !skill_file_present || total != self.byte_count {
            return Err(ReceiptValidationError::InvalidInventory);
        }
        if !is_sha256(&self.content_digest) {
            return Err(ReceiptValidationError::ContentDigestMismatch);
        }
        if self.calculated_content_digest()? != self.content_digest {
            return Err(ReceiptValidationError::ContentDigestMismatch);
        }
        Ok(())
    }

    pub(crate) fn receipt_filename(&self) -> Result<String, ReceiptValidationError> {
        let digest = self.identity_digest()?;
        let digest = digest
            .strip_prefix("sha256:")
            .ok_or(ReceiptValidationError::IdentityDigest)?;
        Ok(format!("v1-{digest}.json"))
    }

    pub(crate) fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.canonical_project_root)
    }

    pub(crate) fn destination_path(&self) -> PathBuf {
        PathBuf::from(&self.destination)
    }

    pub(crate) fn project_identity(&self) -> FileIdentity {
        FileIdentity::new(
            self.project_root_identity.volume,
            self.project_root_identity.object,
        )
    }

    pub(crate) fn catalog_resource_id(&self) -> &str {
        &self.catalog_resource_id
    }

    pub(crate) fn skill_name(&self) -> &str {
        &self.skill_name
    }

    pub(crate) fn agent(&self) -> &str {
        &self.agent
    }

    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    pub(crate) fn source_owner(&self) -> &str {
        &self.source_owner
    }

    pub(crate) fn source_repository(&self) -> &str {
        &self.source_repository
    }

    pub(crate) fn source_commit(&self) -> &str {
        &self.source_commit
    }

    pub(crate) fn installed_at_unix_seconds(&self) -> u64 {
        self.installed_at_unix_seconds
    }

    pub(crate) fn files(&self) -> &[ReceiptFile] {
        &self.files
    }

    pub(crate) fn file_count(&self) -> u64 {
        self.file_count
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.byte_count
    }

    pub(crate) fn directories(&self) -> &[String] {
        &self.directories
    }

    pub(crate) fn same_destination(&self, other: &Self) -> bool {
        let same_path = self.destination.to_lowercase() == other.destination.to_lowercase();
        let same_identity = self.project_root_identity == other.project_root_identity
            && self.skill_name.eq_ignore_ascii_case(&other.skill_name);
        self.agent == other.agent && (same_path || same_identity)
    }

    fn identity_digest(&self) -> Result<String, ReceiptValidationError> {
        let mut context = Context::new(&SHA256);
        context.update(b"THRN-RECEIPT-IDENTITY-V1\0");
        update_field(&mut context, self.canonical_project_root.as_bytes())?;
        update_field(&mut context, self.agent.as_bytes())?;
        update_field(&mut context, self.catalog_resource_id.as_bytes())?;
        Ok(hex_digest(context.finish().as_ref()))
    }

    fn calculated_content_digest(&self) -> Result<String, ReceiptValidationError> {
        let mut context = Context::new(&SHA256);
        context.update(b"THRN-CONTENT-INVENTORY-V1\0");
        context.update(&(self.directories.len() as u64).to_be_bytes());
        for directory in &self.directories {
            context.update(b"D");
            update_field(&mut context, directory.as_bytes())?;
        }
        context.update(&(self.files.len() as u64).to_be_bytes());
        for file in &self.files {
            context.update(b"F");
            update_field(&mut context, file.path.as_bytes())?;
            context.update(&file.byte_count.to_be_bytes());
            let digest =
                decode_sha256(&file.sha256).ok_or(ReceiptValidationError::InvalidInventory)?;
            context.update(&digest);
        }
        Ok(hex_digest(context.finish().as_ref()))
    }
}

impl ReceiptFile {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.byte_count
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }
}

fn path_to_string(path: &Path) -> Result<String, ReceiptValidationError> {
    path.to_str()
        .map(str::to_owned)
        .filter(|value| !value.is_empty() && value.len() <= MAX_RECEIPT_PATH_BYTES)
        .ok_or(ReceiptValidationError::InvalidProjectPath)
}

fn validate_absolute_path(value: &str) -> Result<PathBuf, ReceiptValidationError> {
    if value.is_empty()
        || value.len() > MAX_RECEIPT_PATH_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ReceiptValidationError::InvalidProjectPath);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() || path.to_str() != Some(value) {
        return Err(ReceiptValidationError::InvalidProjectPath);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(ReceiptValidationError::InvalidProjectPath);
            }
        }
    }
    if normalized.to_str() != Some(value) {
        return Err(ReceiptValidationError::InvalidProjectPath);
    }
    Ok(path)
}

fn relative_path_string(path: &Path) -> Result<String, ReceiptValidationError> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(ReceiptValidationError::InvalidInventory);
        };
        let value = value
            .to_str()
            .ok_or(ReceiptValidationError::InvalidInventory)?;
        components.push(value);
    }
    if components.is_empty() {
        return Err(ReceiptValidationError::InvalidInventory);
    }
    Ok(components.join("/"))
}

fn validate_relative_path(value: &str) -> Result<(), ReceiptValidationError> {
    ValidatedArchivePath::parse(value, &ArchiveLimits::DEFAULT)
        .map(|_| ())
        .map_err(|_| ReceiptValidationError::InvalidInventory)
}

fn relative_parent_strings(path: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut end = 0;
    for (index, byte) in path.bytes().enumerate() {
        if byte == b'/' {
            result.push(path[..index].to_owned());
            end = index + 1;
        }
    }
    if end == path.len() {
        result.push(path[..path.len().saturating_sub(1)].to_owned());
    }
    result
}

fn update_field(context: &mut Context, bytes: &[u8]) -> Result<(), ReceiptValidationError> {
    let length = u64::try_from(bytes.len()).map_err(|_| ReceiptValidationError::IdentityDigest)?;
    context.update(&length.to_be_bytes());
    context.update(bytes);
    Ok(())
}

fn hex_digest(digest: &[u8]) -> String {
    let mut value = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn is_sha256(value: &str) -> bool {
    value.starts_with("sha256:")
        && value.len() == 7 + 64
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_sha256(value: &str) -> Option<[u8; 32]> {
    if !is_sha256(value) {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, byte) in value.as_bytes()[7..].as_chunks::<2>().0.iter().enumerate() {
        let high = hex_nibble(byte[0])?;
        let low = hex_nibble(byte[1])?;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Cursor, Write},
        path::{Path, PathBuf},
    };

    use ring::digest::{SHA256, digest};
    use tempfile::tempdir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        install::{
            destination::{FileIdentity, inspect_destination},
            inventory::inspect_staged_skill,
            plan::build_install_plan,
        },
        registry::catalog::parse_catalog_json,
        sources::{
            github::{GithubResponse, GithubTransport, fetch_and_stage_skill_with_factory},
            staging::{StagedSkill, TempDirFactory},
        },
    };

    use super::{InstallationReceipt, ReceiptValidationError};

    struct FakeTransport(Vec<u8>);
    impl GithubTransport for FakeTransport {
        fn get(&self, url: &str) -> Result<GithubResponse, crate::sources::github::TransportError> {
            assert!(url.starts_with("https://github.com/Example-Org/catalog-test-fixtures/archive/2f68bee292a8c68346b4294fb31d9563eefe1ade.zip"));
            Ok(GithubResponse::new(
                200,
                Some("application/zip"),
                Some(self.0.len() as u64),
                None,
                Cursor::new(self.0.clone()),
            ))
        }
    }

    struct TestFactory(std::path::PathBuf);
    impl TempDirFactory for TestFactory {
        fn create_private(&self) -> std::io::Result<tempfile::TempDir> {
            tempfile::Builder::new()
                .prefix("m205-stage-")
                .tempdir_in(&self.0)
        }
    }

    fn archive() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let root = "catalog-test-fixtures-2f68bee292a8c68346b4294fb31d9563eefe1ade/";
        let skill = format!("{root}skills/uncharted-ideation/");
        let mut zip = ZipWriter::new(&mut bytes);
        zip.add_directory(root, options).expect("root");
        zip.add_directory(format!("{root}skills/"), options)
            .expect("skills");
        zip.add_directory(&skill, options).expect("skill");
        zip.add_directory(format!("{skill}references/"), options)
            .expect("references");
        for (name, value) in [
            (
                format!("{skill}SKILL.md"),
                b"---\nname: uncharted-ideation\ndescription: Receipt test fixture skill.\n---\n"
                    .as_slice(),
            ),
            (format!("{skill}references/guide.md"), b"guide".as_slice()),
        ] {
            zip.start_file(name, options).expect("file");
            zip.write_all(value).expect("content");
        }
        zip.finish().expect("zip");
        bytes.into_inner()
    }

    fn staged_skill(parent: &Path) -> StagedSkill {
        let catalog = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog");
        let resource = &catalog.resources()[0];
        fetch_and_stage_skill_with_factory(
            resource,
            &FakeTransport(archive()),
            &TestFactory(parent.to_path_buf()),
        )
        .expect("stage")
    }

    fn receipt_fixture() -> InstallationReceipt {
        let parent = tempdir().expect("parent");
        let project = parent.path().join("project");
        fs::create_dir(&project).expect("project");
        let staged = staged_skill(parent.path());
        let inventory = inspect_staged_skill(&staged).expect("inventory");
        let destination =
            inspect_destination(None, &project, staged.skill_name().as_str()).expect("target");
        let plan = build_install_plan(&staged, &inventory, destination).expect("plan");
        let root_identity = FileIdentity::new(7, 9);
        InstallationReceipt::from_committed_plan(&plan, &inventory, root_identity, 1_800_000_000)
            .expect("receipt")
    }

    #[test]
    fn schema_v1_round_trips_and_keeps_catalog_id_distinct_from_skill_name() {
        let receipt = receipt_fixture();
        let bytes = receipt.to_json().expect("serialization");
        let parsed = InstallationReceipt::from_json(&bytes).expect("parse");
        assert_eq!(parsed, receipt);
        assert_eq!(parsed.catalog_resource_id(), "uncharted");
        assert_eq!(parsed.skill_name(), "uncharted-ideation");
        assert_eq!(parsed.agent(), "codex");
        assert_eq!(parsed.scope(), "project");
        assert_eq!(parsed.installed_at_unix_seconds(), 1_800_000_000);
    }

    #[test]
    fn receipt_key_and_content_digest_are_stable_and_domain_separated() {
        let first = receipt_fixture();
        let second = first.clone();
        assert_eq!(
            first.receipt_filename().expect("first key"),
            second.receipt_filename().expect("second key")
        );
        assert_eq!(first.content_digest, second.content_digest);
        let mut another_resource = first.clone();
        another_resource.catalog_resource_id = "creative-lab".to_owned();
        assert_ne!(
            first.receipt_filename().expect("first key"),
            another_resource.receipt_filename().expect("different key")
        );
        let mut another_project = first.clone();
        let root = PathBuf::from(&first.canonical_project_root).join("other-project");
        another_project.canonical_project_root = root.to_string_lossy().into_owned();
        another_project.destination = root
            .join(".agents/skills/uncharted-ideation")
            .to_string_lossy()
            .into_owned();
        another_project.project_root_identity.object += 1;
        assert_ne!(
            first.receipt_filename().expect("first key"),
            another_project
                .receipt_filename()
                .expect("different project key")
        );
        assert_ne!(
            first.receipt_filename().expect("key"),
            "sha256:untrusted.json"
        );
        assert!(first.content_digest.starts_with("sha256:"));
    }

    #[test]
    fn per_file_sha256_matches_the_exact_inventory_digest() {
        let receipt = receipt_fixture();
        let file = receipt
            .files()
            .iter()
            .find(|file| file.path() == "SKILL.md")
            .expect("SKILL.md");
        let expected = digest(
            &SHA256,
            b"---\nname: uncharted-ideation\ndescription: Receipt test fixture skill.\n---\n",
        );
        let expected_hex = expected
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(file.sha256(), format!("sha256:{expected_hex}"));
        assert_eq!(file.byte_count(), 74);
    }

    #[test]
    fn unsupported_schema_and_malformed_truncated_or_oversized_json_fail() {
        let receipt = receipt_fixture();
        let mut value: serde_json::Value =
            serde_json::from_slice(&receipt.to_json().expect("json")).expect("value");
        value["schemaVersion"] = serde_json::json!(2);
        assert!(matches!(
            InstallationReceipt::from_json(&serde_json::to_vec(&value).expect("json")),
            Err(ReceiptValidationError::UnsupportedSchema(2))
        ));
        assert!(matches!(
            InstallationReceipt::from_json(b"{\"schemaVersion\":1"),
            Err(ReceiptValidationError::MalformedJson)
        ));
        assert!(matches!(
            InstallationReceipt::from_json(b"{}"),
            Err(ReceiptValidationError::MalformedJson)
        ));
        assert!(matches!(
            InstallationReceipt::from_json(&vec![b' '; super::MAX_RECEIPT_BYTES + 1]),
            Err(ReceiptValidationError::InputTooLarge { .. })
        ));
    }

    #[test]
    fn missing_extra_invalid_fields_and_identity_mismatches_fail() {
        let receipt = receipt_fixture();
        let mut value: serde_json::Value =
            serde_json::from_slice(&receipt.to_json().expect("json")).expect("value");
        value.as_object_mut().expect("object").remove("agent");
        assert!(matches!(
            InstallationReceipt::from_json(&serde_json::to_vec(&value).expect("json")),
            Err(ReceiptValidationError::MalformedJson)
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&receipt.to_json().expect("json")).expect("value");
        value["unsolicited"] = serde_json::json!("extra");
        assert!(matches!(
            InstallationReceipt::from_json(&serde_json::to_vec(&value).expect("json")),
            Err(ReceiptValidationError::MalformedJson)
        ));

        let mut wrong_agent = receipt.clone();
        wrong_agent.agent = "cursor".to_owned();
        assert!(matches!(
            wrong_agent.validate(),
            Err(ReceiptValidationError::InvalidMetadata)
        ));
        let mut wrong_scope = receipt.clone();
        wrong_scope.scope = "user".to_owned();
        assert!(matches!(
            wrong_scope.validate(),
            Err(ReceiptValidationError::InvalidMetadata)
        ));
        let mut wrong_destination = receipt.clone();
        wrong_destination.destination = PathBuf::from(&receipt.canonical_project_root)
            .join("another-project/.agents/skills/uncharted-ideation")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(
            wrong_destination.validate(),
            Err(ReceiptValidationError::DestinationMismatch)
        ));

        let noncanonical = format!("{}/./nested", receipt.canonical_project_root);
        assert!(matches!(
            super::validate_absolute_path(&noncanonical),
            Err(ReceiptValidationError::InvalidProjectPath)
        ));
        let control_path = format!("{}\nforged", receipt.canonical_project_root);
        assert!(matches!(
            super::validate_absolute_path(&control_path),
            Err(ReceiptValidationError::InvalidProjectPath)
        ));
    }

    #[test]
    fn traversal_duplicate_paths_and_case_collisions_are_rejected() {
        let receipt = receipt_fixture();
        let mut traversal = receipt.clone();
        traversal.files[0].path = "../escape".to_owned();
        assert!(matches!(
            traversal.validate(),
            Err(ReceiptValidationError::InvalidInventory)
        ));

        let mut collision = receipt.clone();
        collision.files.push(collision.files[0].clone());
        collision
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        collision.file_count = collision.files.len() as u64;
        collision.content_digest = collision.calculated_content_digest().expect("digest");
        assert!(matches!(
            collision.validate(),
            Err(ReceiptValidationError::InvalidInventory)
        ));

        let mut bad_digest = receipt;
        bad_digest.files[0].sha256 =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        assert!(matches!(
            bad_digest.validate(),
            Err(ReceiptValidationError::ContentDigestMismatch)
        ));
    }
}
