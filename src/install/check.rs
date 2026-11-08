//! Read-only classification of Codex project installations against local catalog pins.

use crate::{
    install::{receipt::InstallationReceipt, receipt_store::ProjectInstallationInspection},
    models::catalog::{Catalog, Resource},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckStatus {
    UpToDate,
    UpdateAvailable,
    NotInCatalog,
    SourceMismatch,
    LegacyReceipt,
    ModifiedOrUnsafe,
}

impl CheckStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::UpToDate => "Up to date",
            Self::UpdateAvailable => "Update available",
            Self::NotInCatalog => "Not in catalog",
            Self::SourceMismatch => "Source mismatch",
            Self::LegacyReceipt => "Legacy receipt",
            Self::ModifiedOrUnsafe => "Modified or unsafe installation",
        }
    }

    pub(crate) fn explanation(self) -> &'static str {
        match self {
            Self::UpToDate => "receipt identity, installed tree, and catalog pin match",
            Self::UpdateAvailable => {
                "the catalog pins a different revision; Git ancestry is not inferred"
            }
            Self::NotInCatalog => "the canonical resource ID is absent from this local catalog",
            Self::SourceMismatch => "catalog identity differs; automatic update is not eligible",
            Self::LegacyReceipt => {
                "schema-v1 evidence lacks installation-time directory identity; automatic update is not eligible"
            }
            Self::ModifiedOrUnsafe => {
                "the receipt-backed directory identity, contents, or structure did not verify; automatic update is not eligible"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckFinding {
    receipt: InstallationReceipt,
    catalog_commit: Option<String>,
    status: CheckStatus,
}

impl CheckFinding {
    pub(crate) fn receipt(&self) -> &InstallationReceipt {
        &self.receipt
    }

    pub(crate) fn catalog_commit(&self) -> Option<&str> {
        self.catalog_commit.as_deref()
    }

    pub(crate) fn status(&self) -> CheckStatus {
        self.status
    }
}

pub(crate) fn classify(
    catalog: &Catalog,
    inspections: Vec<ProjectInstallationInspection>,
) -> Vec<CheckFinding> {
    let mut findings = inspections
        .into_iter()
        .map(|inspection| {
            let receipt = inspection.receipt;
            let resource = catalog
                .resources()
                .iter()
                .find(|resource| resource.id().as_str() == receipt.catalog_resource_id());
            let catalog_commit =
                resource.map(|resource| resource.source().commit().as_str().to_owned());
            let status =
                if inspection.content != super::receipt_store::ReceiptContentState::Verified {
                    CheckStatus::ModifiedOrUnsafe
                } else if receipt.schema_version() == 1 {
                    CheckStatus::LegacyReceipt
                } else if let Some(resource) = resource {
                    if !same_update_identity(resource, &receipt) {
                        CheckStatus::SourceMismatch
                    } else if resource.source().commit().as_str() == receipt.source_commit() {
                        CheckStatus::UpToDate
                    } else {
                        CheckStatus::UpdateAvailable
                    }
                } else {
                    CheckStatus::NotInCatalog
                };
            CheckFinding {
                receipt,
                catalog_commit,
                status,
            }
        })
        .collect::<Vec<_>>();
    findings.sort_by(|left, right| {
        left.receipt
            .catalog_resource_id()
            .cmp(right.receipt.catalog_resource_id())
            .then_with(|| left.receipt.skill_name().cmp(right.receipt.skill_name()))
    });
    findings
}

fn same_update_identity(resource: &Resource, receipt: &InstallationReceipt) -> bool {
    resource.id().as_str() == receipt.catalog_resource_id()
        && resource.skill_name().as_str() == receipt.skill_name()
        && resource.source().owner().as_str() == receipt.source_owner()
        && resource.source().repository().as_str() == receipt.source_repository()
        && resource.source().path().as_str() == receipt.source_directory()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use ring::digest::{Context, SHA256, digest};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::{
        install::{
            commit::SecureDir,
            journal::{JournalIntent, JournalStore, NoJournalFaults, new_staging_name},
            receipt::InstallationReceipt,
            receipt_store::{ReceiptContentState, ReceiptStore},
            removal::{RemovalError, RemovalFaultInjector, RemovalOperation},
        },
        registry::catalog::parse_catalog_json,
    };

    use super::{CheckStatus, classify};

    const INSTALLED_SHA: &str = "2f68bee292a8c68346b4294fb31d9563eefe1ade";
    const OTHER_SHA: &str = "3f68bee292a8c68346b4294fb31d9563eefe1ade";
    const CONTENT: &[u8] = b"abc";

    struct Fixture {
        _root: TempDir,
        data: std::path::PathBuf,
        project: std::path::PathBuf,
        store: ReceiptStore,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary fixture root");
            let root_path = fs::canonicalize(root.path()).expect("canonical fixture root");
            let project = root_path.join("project");
            let data = root_path.join("application-data");
            fs::create_dir(&project).expect("create project");
            let store = ReceiptStore::new(data.join("receipts"));
            Self {
                _root: root,
                data,
                project,
                store,
            }
        }

        fn add_installation(
            &self,
            project: &Path,
            resource_id: &str,
            skill_name: &str,
            source_commit: &str,
            legacy: bool,
        ) -> InstallationReceipt {
            let project = fs::canonicalize(project).expect("canonical project");
            let destination = project.join(".agents/skills").join(skill_name);
            fs::create_dir_all(&destination).expect("create installed skill");
            fs::write(destination.join("SKILL.md"), CONTENT).expect("write installed skill");
            let project_identity = SecureDir::open_absolute(&project)
                .expect("open project")
                .identity()
                .expect("project identity");
            let destination_identity = SecureDir::open_absolute(&destination)
                .expect("open installed directory")
                .identity()
                .expect("installed directory identity");
            let content_sha = sha256_hex(CONTENT);
            let inventory_digest =
                inventory_digest_v1("SKILL.md", CONTENT.len() as u64, &content_sha);
            let project_text = project.to_str().expect("UTF-8 fixture path");
            let destination_text = destination.to_str().expect("UTF-8 fixture path");
            let value = json!({
                "schemaVersion": 1,
                "catalogResourceId": resource_id,
                "skillName": skill_name,
                "agent": "codex",
                "scope": "project",
                "canonicalProjectRoot": project_text,
                "projectRootIdentity": {
                    "volume": project_identity.volume(),
                    "object": project_identity.object()
                },
                "destination": destination_text,
                "installedAtUnixSeconds": 1,
                "sourceOwner": "Example-Org",
                "sourceRepository": "catalog-test-fixtures",
                "sourceCommit": source_commit,
                "sourceDirectory": format!("skills/{skill_name}"),
                "resourceVersion": "1.0.0",
                "directories": [],
                "files": [{
                    "path": "SKILL.md",
                    "byteCount": CONTENT.len(),
                    "sha256": format!("sha256:{content_sha}")
                }],
                "fileCount": 1,
                "byteCount": CONTENT.len(),
                "contentDigest": inventory_digest
            });
            let json = serde_json::to_vec(&value).expect("serialize receipt fixture");
            let receipt = InstallationReceipt::from_json(&json).expect("valid legacy receipt");
            let receipt = if legacy {
                receipt
            } else {
                receipt
                    .with_installed_directory_identity(destination_identity)
                    .expect("upgrade fixture to schema v2")
            };
            fs::create_dir_all(self.store_directory()).expect("create receipt store");
            let lock_path = self.store_directory().join(".writer.lock");
            if !lock_path.exists() {
                fs::write(lock_path, []).expect("create persistent writer lock fixture");
            }
            fs::write(
                self.store_directory()
                    .join(receipt.receipt_filename().expect("receipt identity")),
                receipt.to_json().expect("serialize receipt"),
            )
            .expect("write receipt");
            receipt
        }

        fn store_directory(&self) -> std::path::PathBuf {
            self.data.join("receipts")
        }

        fn inspect(
            &self,
            project: Option<&Path>,
        ) -> Vec<crate::install::receipt_store::ProjectInstallationInspection> {
            self.store
                .inspect_project(project, &self.project)
                .expect("project check snapshot")
                .installations
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        digest(&SHA256, bytes)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn inventory_digest_v1(path: &str, bytes: u64, sha: &str) -> String {
        let mut context = Context::new(&SHA256);
        context.update(b"THRN-CONTENT-INVENTORY-V1\0");
        context.update(&0_u64.to_be_bytes());
        context.update(&1_u64.to_be_bytes());
        context.update(b"F");
        context.update(&(path.len() as u64).to_be_bytes());
        context.update(path.as_bytes());
        context.update(&bytes.to_be_bytes());
        for byte in (0..32).map(|index| u8::from_str_radix(&sha[index * 2..index * 2 + 2], 16)) {
            context.update(&[byte.expect("hex digest")]);
        }
        format!("sha256:{}", hex(context.finish().as_ref()))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn catalog() -> crate::models::catalog::Catalog {
        parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("valid catalog fixture")
    }

    fn catalog_edit(edit: impl FnOnce(&mut Value)) -> crate::models::catalog::Catalog {
        let mut value: Value = serde_json::from_slice(include_bytes!(
            "../../tests/fixtures/catalog/test-only-valid-v1.json"
        ))
        .expect("catalog JSON fixture");
        edit(&mut value);
        parse_catalog_json(&serde_json::to_vec(&value).expect("serialize catalog fixture"))
            .expect("edited catalog remains valid")
    }

    fn status(
        catalog: &crate::models::catalog::Catalog,
        inspections: Vec<crate::install::receipt_store::ProjectInstallationInspection>,
    ) -> Vec<super::CheckFinding> {
        classify(catalog, inspections)
    }

    #[test]
    fn classifies_up_to_date_and_a_different_catalog_pinned_revision() {
        let up_to_date = Fixture::new();
        up_to_date.add_installation(
            &up_to_date.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let findings = status(&catalog(), up_to_date.inspect(None));
        assert_eq!(findings[0].status(), CheckStatus::UpToDate);
        assert_eq!(findings[0].catalog_commit(), Some(INSTALLED_SHA));

        let different = Fixture::new();
        different.add_installation(
            &different.project,
            "uncharted",
            "uncharted-ideation",
            OTHER_SHA,
            false,
        );
        let findings = status(&catalog(), different.inspect(None));
        assert_eq!(findings[0].status(), CheckStatus::UpdateAvailable);
        assert_eq!(findings[0].catalog_commit(), Some(INSTALLED_SHA));
    }

    #[test]
    fn missing_resource_and_source_identity_changes_are_not_update_candidates() {
        let fixture = Fixture::new();
        fixture.add_installation(
            &fixture.project,
            "uncharted",
            "uncharted-ideation",
            OTHER_SHA,
            false,
        );
        let empty = parse_catalog_json(include_bytes!(
            "../../tests/fixtures/catalog/test-only-empty-v1.json"
        ))
        .expect("empty catalog fixture");
        assert_eq!(
            status(&empty, fixture.inspect(None))[0].status(),
            CheckStatus::NotInCatalog
        );

        for (field, value) in [
            ("owner", "Changed-Org"),
            ("repository", "changed-repository"),
            ("path", "skills/renamed-path"),
            ("skillName", "renamed-skill"),
        ] {
            let catalog = catalog_edit(|catalog| {
                let resource = &mut catalog["resources"][0];
                if field == "skillName" {
                    resource["skillName"] = json!(value);
                    resource["source"]["path"] = json!("skills/renamed-skill");
                } else {
                    resource["source"][field] = json!(value);
                }
                resource["source"]["commit"] = json!(OTHER_SHA);
            });
            let findings = status(&catalog, fixture.inspect(None));
            assert_eq!(findings[0].status(), CheckStatus::SourceMismatch, "{field}");
            assert_eq!(findings[0].catalog_commit(), Some(OTHER_SHA));
        }
    }

    #[test]
    fn verified_legacy_receipts_are_labeled_without_repair() {
        let fixture = Fixture::new();
        let receipt = fixture.add_installation(
            &fixture.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            true,
        );
        let receipt_bytes = receipt.to_json().expect("legacy receipt bytes");
        let findings = status(&catalog(), fixture.inspect(None));
        assert_eq!(findings[0].status(), CheckStatus::LegacyReceipt);
        assert_eq!(receipt.schema_version(), 1);
        assert_eq!(
            receipt.to_json().expect("receipt still serializes"),
            receipt_bytes
        );
    }

    #[test]
    fn modified_same_length_extra_missing_and_replaced_trees_are_ineligible() {
        for mutation in ["same-length", "extra", "missing", "replacement"] {
            let fixture = Fixture::new();
            fixture.add_installation(
                &fixture.project,
                "uncharted",
                "uncharted-ideation",
                INSTALLED_SHA,
                false,
            );
            let destination = fixture.project.join(".agents/skills/uncharted-ideation");
            match mutation {
                "same-length" => fs::write(destination.join("SKILL.md"), b"xyz").unwrap(),
                "extra" => fs::write(destination.join("extra.txt"), b"extra").unwrap(),
                "missing" => fs::remove_file(destination.join("SKILL.md")).unwrap(),
                "replacement" => {
                    let old = fixture.project.join("old-skill-directory");
                    fs::rename(&destination, &old).unwrap();
                    fs::create_dir(&destination).unwrap();
                    fs::write(destination.join("SKILL.md"), CONTENT).unwrap();
                    fs::remove_dir_all(old).unwrap();
                }
                _ => unreachable!(),
            }
            let findings = status(&catalog(), fixture.inspect(None));
            assert_eq!(
                findings[0].status(),
                CheckStatus::ModifiedOrUnsafe,
                "{mutation}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_file_is_an_ineligible_installation() {
        let fixture = Fixture::new();
        fixture.add_installation(
            &fixture.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        fs::remove_file(destination.join("SKILL.md")).unwrap();
        let outside = fixture._root.path().join("outside.txt");
        fs::write(&outside, CONTENT).unwrap();
        std::os::unix::fs::symlink(outside, destination.join("SKILL.md")).unwrap();
        assert_eq!(
            status(&catalog(), fixture.inspect(None))[0].status(),
            CheckStatus::ModifiedOrUnsafe
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_destination_junction_is_ineligible() {
        let fixture = Fixture::new();
        fixture.add_installation(
            &fixture.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let destination = fixture.project.join(".agents/skills/uncharted-ideation");
        let external = fixture._root.path().join("external-skill");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("SKILL.md"), CONTENT).unwrap();
        fs::remove_dir_all(&destination).unwrap();
        crate::install::windows_test_support::create_junction(&destination, &external);
        assert_eq!(
            status(&catalog(), fixture.inspect(None))[0].status(),
            CheckStatus::ModifiedOrUnsafe
        );
    }

    #[test]
    fn multiple_resources_are_sorted_and_other_projects_are_excluded() {
        let fixture = Fixture::new();
        let second_project = fixture._root.path().join("second-project");
        fs::create_dir(&second_project).unwrap();
        fixture.add_installation(&fixture.project, "zeta", "zeta-skill", OTHER_SHA, false);
        fixture.add_installation(&fixture.project, "alpha", "alpha-skill", OTHER_SHA, false);
        fixture.add_installation(
            &second_project,
            "second-project-only",
            "second-skill",
            OTHER_SHA,
            false,
        );
        let inspections = fixture.inspect(Some(&fixture.project));
        assert_eq!(inspections.len(), 2);

        let multi_catalog = catalog_edit(|catalog| {
            let first = catalog["resources"][0].clone();
            let mut second = first.clone();
            second["id"] = json!("alpha");
            second["aliases"] = json!([]);
            second["skillName"] = json!("alpha-skill");
            second["source"]["path"] = json!("skills/alpha-skill");
            second["source"]["commit"] = json!(OTHER_SHA);
            let mut third = first;
            third["id"] = json!("zeta");
            third["aliases"] = json!([]);
            third["skillName"] = json!("zeta-skill");
            third["source"]["path"] = json!("skills/zeta-skill");
            third["source"]["commit"] = json!(OTHER_SHA);
            catalog["resources"] = json!([second, third]);
        });
        let findings = status(&multi_catalog, inspections);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].receipt().catalog_resource_id(), "alpha");
        assert_eq!(findings[1].receipt().catalog_resource_id(), "zeta");
        assert!(
            findings
                .iter()
                .all(|finding| finding.status() == CheckStatus::UpToDate)
        );
    }

    #[test]
    fn corrupt_and_conflicting_receipt_storage_fails_closed() {
        let corrupt = Fixture::new();
        fs::create_dir_all(corrupt.store_directory()).unwrap();
        fs::write(corrupt.store_directory().join(".writer.lock"), []).unwrap();
        fs::write(
            corrupt
                .store_directory()
                .join(format!("v1-{}.json", "a".repeat(64))),
            b"not json",
        )
        .unwrap();
        assert!(
            corrupt
                .store
                .inspect_project(None, &corrupt.project)
                .is_err()
        );

        let conflict = Fixture::new();
        conflict.add_installation(
            &conflict.project,
            "first-resource",
            "same-skill",
            INSTALLED_SHA,
            false,
        );
        conflict.add_installation(
            &conflict.project,
            "second-resource",
            "same-skill",
            INSTALLED_SHA,
            false,
        );
        assert!(
            conflict
                .store
                .inspect_project(None, &conflict.project)
                .is_err()
        );
    }

    #[test]
    fn active_writer_and_pending_install_or_removal_fail_without_recovery() {
        let active = Fixture::new();
        active.add_installation(
            &active.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(active.store_directory().join(".writer.lock"))
            .unwrap();
        lock.try_lock().expect("acquire simulated active writer");
        assert!(active.store.inspect_project(None, &active.project).is_err());
        lock.unlock().unwrap();

        let installing = Fixture::new();
        let receipt = installing.add_installation(
            &installing.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        JournalStore::new(installing.data.join("operations"))
            .prepare(
                JournalIntent {
                    receipt: receipt.clone().into_legacy_for_test(),
                    staging_name: new_staging_name().unwrap(),
                    create_agents_directory: false,
                    create_skills_directory: false,
                    agents_identity: None,
                    skills_identity: None,
                },
                &NoJournalFaults,
            )
            .unwrap();
        let before = tree_snapshot(installing._root.path());
        assert!(
            installing
                .store
                .inspect_project(None, &installing.project)
                .is_err()
        );
        assert_eq!(tree_snapshot(installing._root.path()), before);

        let removing = Fixture::new();
        removing.add_installation(
            &removing.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let error = removing
            .store
            .remove_by_id("uncharted", None, &removing.project, &FailBeforeQuarantine)
            .expect_err("fault leaves a valid pending removal journal");
        assert!(matches!(error, RemovalError::Incomplete { .. }));
        let before = tree_snapshot(removing._root.path());
        assert!(
            removing
                .store
                .inspect_project(None, &removing.project)
                .is_err()
        );
        assert_eq!(tree_snapshot(removing._root.path()), before);
    }

    struct FailBeforeQuarantine;

    impl RemovalFaultInjector for FailBeforeQuarantine {
        fn before(&self, operation: RemovalOperation) -> std::io::Result<()> {
            if operation == RemovalOperation::RenameToQuarantine {
                Err(std::io::Error::other("injected before quarantine"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn inspection_itself_does_not_create_or_change_data_or_project_files() {
        let fixture = Fixture::new();
        fixture.add_installation(
            &fixture.project,
            "uncharted",
            "uncharted-ideation",
            INSTALLED_SHA,
            false,
        );
        let before = tree_snapshot(fixture._root.path());
        let findings = fixture.inspect(None);
        assert_eq!(findings[0].content, ReceiptContentState::Verified);
        assert_eq!(tree_snapshot(fixture._root.path()), before);

        let empty = Fixture::new();
        let before = tree_snapshot(empty._root.path());
        assert!(
            empty
                .store
                .inspect_project(None, &empty.project)
                .unwrap()
                .installations
                .is_empty()
        );
        assert_eq!(tree_snapshot(empty._root.path()), before);
    }

    fn tree_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut result = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    stack.push(path.clone());
                    result.push((path.to_string_lossy().into_owned(), Vec::new()));
                } else if kind.is_file() {
                    result.push((path.to_string_lossy().into_owned(), fs::read(path).unwrap()));
                }
            }
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        result
    }
}
