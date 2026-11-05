use serde_json::{Value, json};
#[path = "../src/registry/catalog.rs"]
mod catalog_parser;
#[path = "../src/models/mod.rs"]
mod models;

use catalog_parser::{CatalogError, JsonProblem, MAX_ALIASES_PER_RESOURCE, parse_catalog_json};
use models::catalog::{ResourceKind, SourceProvider};

const VALID_FIXTURE: &str = include_str!("fixtures/catalog/test-only-valid-v1.json");
const MULTIPLE_FIXTURE: &str =
    include_str!("fixtures/catalog/test-only-multiple-resources-v1.json");

fn parse_value(value: &Value) -> Result<models::catalog::Catalog, CatalogError> {
    let bytes = serde_json::to_vec(value).expect("test catalog serializes");
    parse_catalog_json(&bytes)
}

fn valid_value() -> Value {
    serde_json::from_str(VALID_FIXTURE).expect("valid fixture JSON")
}

fn first_resource_mut(value: &mut Value) -> &mut Value {
    &mut value["resources"][0]
}

fn assert_invalid_field<T: std::fmt::Debug>(
    result: Result<T, CatalogError>,
    expected: &str,
) -> String {
    match result {
        Err(CatalogError::InvalidField { field, .. }) => {
            assert_eq!(field, expected);
            field
        }
        other => panic!("expected invalid field {expected}, got {other:?}"),
    }
}

fn assert_duplicate_identifier<T: std::fmt::Debug>(result: Result<T, CatalogError>) -> String {
    match result {
        Err(error @ CatalogError::DuplicateIdentifier { .. }) => error.to_string(),
        other => panic!("expected duplicate identifier error, got {other:?}"),
    }
}

#[test]
fn parses_the_public_api_from_a_valid_fixture() {
    let catalog = parse_catalog_json(VALID_FIXTURE.as_bytes()).expect("valid v1 catalog");

    assert_eq!(catalog.schema_version(), 1);
    assert_eq!(catalog.resources().len(), 1);
    let resource = &catalog.resources()[0];
    assert_eq!(resource.kind(), ResourceKind::Skill);
    assert_eq!(resource.id().as_str(), "uncharted");
    assert_eq!(resource.display_name(), "Uncharted Ideation");
    assert_eq!(resource.skill_name().as_str(), "uncharted-ideation");
    assert_ne!(resource.id().as_str(), resource.skill_name().as_str());
    assert!(resource.description().contains("test-only"));
    assert_eq!(resource.version().as_str(), "1.0.0");
    assert_eq!(resource.aliases()[0].as_str(), "idea-lab");
    assert_eq!(resource.aliases()[1].as_str(), "creative-session");
    assert_eq!(resource.source().provider(), SourceProvider::Github);
    assert_eq!(resource.source().owner().as_str(), "Example-Org");
    assert_eq!(
        resource.source().repository().as_str(),
        "catalog-test-fixtures"
    );
    assert_eq!(
        resource.source().path().as_str(),
        "skills/uncharted-ideation"
    );
    assert_eq!(resource.source().path().depth(), 2);
    assert_eq!(resource.source().commit().as_str().len(), 40);
    assert_eq!(
        resource.source().commit().as_str(),
        "2f68bee292a8c68346b4294fb31d9563eefe1ade"
    );
}

#[test]
fn parses_multiple_independent_resources_and_omitted_aliases() {
    let catalog = parse_catalog_json(MULTIPLE_FIXTURE.as_bytes()).expect("valid resource list");

    assert_eq!(catalog.resources().len(), 2);
    assert_eq!(catalog.resources()[0].id().as_str(), "research-helper");
    assert_eq!(
        catalog.resources()[0].skill_name().as_str(),
        "research-assistant"
    );
    assert!(catalog.resources()[0].aliases().is_empty());
    assert_eq!(catalog.resources()[1].aliases().len(), 0);
    assert_eq!(catalog.resources()[1].source().commit().as_str().len(), 40);
}

#[test]
fn malformed_json_and_missing_required_fields_return_errors() {
    let malformed = parse_catalog_json(b"{not json}").expect_err("malformed JSON rejected");
    assert!(malformed.to_string().contains("catalog JSON is malformed"));
    if let CatalogError::Json(error) = malformed {
        assert_eq!(error.problem(), JsonProblem::Malformed);
        assert!(error.line() > 0);
        assert!(error.column() > 0);
        assert_eq!(error.field_path(), None);
    } else {
        panic!("malformed input should return a JSON error");
    }

    let mut missing_version = valid_value();
    missing_version
        .as_object_mut()
        .expect("catalog object")
        .remove("schemaVersion");
    let error = parse_value(&missing_version).expect_err("schemaVersion is required");
    assert!(error.to_string().contains("schemaVersion"));

    let mut missing_resource_field = valid_value();
    first_resource_mut(&mut missing_resource_field)
        .as_object_mut()
        .expect("resource object")
        .remove("source");
    let error = parse_value(&missing_resource_field).expect_err("source is required");
    assert!(error.to_string().contains("source"));

    let required_resource_fields = [
        ("id", None),
        ("kind", None),
        ("displayName", None),
        ("skillName", None),
        ("description", None),
        ("version", None),
        ("source", None),
        ("provider", Some("source")),
        ("owner", Some("source")),
        ("repository", Some("source")),
        ("path", Some("source")),
        ("commit", Some("source")),
    ];
    for (required, parent) in required_resource_fields {
        let mut value = valid_value();
        let target = first_resource_mut(&mut value);
        let object = match parent {
            Some(parent) => target[parent].as_object_mut().expect("source object"),
            None => target.as_object_mut().expect("resource object"),
        };
        object.remove(required);
        let error = parse_value(&value).expect_err("required field is absent");
        assert!(
            error.to_string().contains(required),
            "missing {required} should be identified: {error}"
        );
    }
}

#[test]
fn rejects_unsupported_or_malformed_schema_versions() {
    for unsupported in [0, 2, u64::MAX] {
        let mut value = valid_value();
        value["schemaVersion"] = json!(unsupported);
        assert!(matches!(
            parse_value(&value),
            Err(CatalogError::UnsupportedSchemaVersion { found, .. }) if found == unsupported
        ));
    }

    let mut non_integer = valid_value();
    non_integer["schemaVersion"] = json!(1.0);
    assert!(parse_value(&non_integer).is_err());
}

#[test]
fn rejects_unknown_fields_to_keep_schema_versioning_explicit() {
    let mut value = valid_value();
    first_resource_mut(&mut value)["futureField"] = json!(true);

    let error = parse_value(&value).expect_err("unknown field rejected");
    assert!(error.to_string().contains("unknown object field"));
    assert!(!error.to_string().contains("futureField"));
}

#[test]
fn schema_errors_do_not_echo_untrusted_json_values() {
    let secret = "do-not-echo-this-secret-value";
    let input = format!(r#"{{"schemaVersion":1,"resources":"{secret}"}}"#);
    let error = parse_catalog_json(input.as_bytes()).expect_err("wrong JSON type rejected");

    assert!(error.to_string().contains("resources"));
    assert!(!error.to_string().contains(secret));
    let CatalogError::Json(json_error) = error else {
        panic!("wrong JSON shape should be a structured JSON error");
    };
    assert_eq!(json_error.problem(), JsonProblem::InvalidShape);
    assert_eq!(json_error.field_path(), Some("resources"));
}

#[test]
fn rejects_duplicate_canonical_ids_case_insensitively() {
    let mut value: Value = serde_json::from_str(MULTIPLE_FIXTURE).expect("fixture JSON");
    value["resources"][1]["id"] = json!("RESEARCH-HELPER");

    let error = assert_duplicate_identifier(parse_value(&value));
    assert!(error.contains("resources[0].id"));
    assert!(error.contains("resources[1].id"));
}

#[test]
fn rejects_duplicate_aliases_and_alias_collisions_with_canonical_ids() {
    let mut duplicate_alias = valid_value();
    first_resource_mut(&mut duplicate_alias)["aliases"] = json!(["Idea-Lab", "idea-lab"]);
    let error = assert_duplicate_identifier(parse_value(&duplicate_alias));
    assert!(error.contains("resources[0].aliases[0]"));
    assert!(error.contains("resources[0].aliases[1]"));

    let mut alias_conflicts_with_id = valid_value();
    first_resource_mut(&mut alias_conflicts_with_id)["aliases"] = json!(["UNCHARTED"]);
    let error = assert_duplicate_identifier(parse_value(&alias_conflicts_with_id));
    assert!(error.contains("resources[0].id"));
    assert!(error.contains("resources[0].aliases[0]"));

    let mut alias_conflicts_with_other_id: Value =
        serde_json::from_str(MULTIPLE_FIXTURE).expect("fixture JSON");
    alias_conflicts_with_other_id["resources"][0]["aliases"] = json!(["REFACTOR-HELPER"]);
    let error = assert_duplicate_identifier(parse_value(&alias_conflicts_with_other_id));
    assert!(error.contains("resources[1].id"));
    assert!(error.contains("resources[0].aliases[0]"));
}

#[test]
fn rejects_invalid_ids_names_and_alias_grammar() {
    for invalid in [
        "",
        "-leading",
        "trailing-",
        "two--hyphens",
        "has space",
        "éclair",
    ] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["id"] = json!(invalid);
        assert_invalid_field(parse_value(&value), "resources[0].id");
    }

    for invalid in ["", "Uppercase", "two--hyphens", "leading-", "has space"] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["skillName"] = json!(invalid);
        assert_invalid_field(parse_value(&value), "resources[0].skillName");
    }

    let mut invalid_alias = valid_value();
    first_resource_mut(&mut invalid_alias)["aliases"] = json!(["bad--alias"]);
    assert_invalid_field(parse_value(&invalid_alias), "resources[0].aliases[0]");
}

#[test]
fn rejects_unsupported_kind_and_provider() {
    let mut invalid_kind = valid_value();
    first_resource_mut(&mut invalid_kind)["kind"] = json!("tool");
    assert_invalid_field(parse_value(&invalid_kind), "resources[0].kind");

    let mut invalid_provider = valid_value();
    first_resource_mut(&mut invalid_provider)["source"]["provider"] = json!("gitlab");
    assert_invalid_field(
        parse_value(&invalid_provider),
        "resources[0].source.provider",
    );
}

#[test]
fn rejects_malformed_github_owner_and_repository_identifiers() {
    for owner in [
        "",
        "bad/owner",
        "-leading",
        "trailing-",
        "two--hyphens",
        "has space",
    ] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["source"]["owner"] = json!(owner);
        assert_invalid_field(parse_value(&value), "resources[0].source.owner");
    }

    for repository in [
        "",
        "bad/repository",
        ".leading",
        "trailing.",
        "two..dots",
        "has space",
    ] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["source"]["repository"] = json!(repository);
        assert_invalid_field(parse_value(&value), "resources[0].source.repository");
    }
}

#[test]
fn rejects_malformed_commit_hashes() {
    for commit in [
        "deadbeef",
        "0123456789abcdef0123456789abcdef0123456",
        "0123456789abcdef0123456789abcdef012345678",
        "FULL_40_CHARACTER_GIT_COMMIT_SHA",
        "g123456789abcdef0123456789abcdef01234567",
        "0000000000000000000000000000000000000000",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "0123456789abcdef0123456789abcdef01234567",
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    ] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["source"]["commit"] = json!(commit);
        assert_invalid_field(parse_value(&value), "resources[0].source.commit");
    }
}

#[test]
fn rejects_hostile_source_paths_lexically() {
    let hostile_paths = [
        "/absolute/path",
        "skills/../outside",
        ".",
        "C:/skills/item",
        "C:skills/item",
        "\\\\server\\share",
        "skills\\..\\outside",
        "skills//item",
        "skills/item/",
        "skills/./item",
        "skills/\u{0000}item",
    ];

    for path in hostile_paths {
        let mut value = valid_value();
        first_resource_mut(&mut value)["source"]["path"] = json!(path);
        assert_invalid_field(parse_value(&value), "resources[0].source.path");
    }
}

#[test]
fn rejects_empty_control_and_overlong_descriptive_fields() {
    for display_name in ["", " \t ", "line\nbreak"] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["displayName"] = json!(display_name);
        assert_invalid_field(parse_value(&value), "resources[0].displayName");
    }

    let mut long_display = valid_value();
    first_resource_mut(&mut long_display)["displayName"] = json!("x".repeat(129));
    assert_invalid_field(parse_value(&long_display), "resources[0].displayName");

    for description in ["", " \t ", "contains\u{0001}control"] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["description"] = json!(description);
        assert_invalid_field(parse_value(&value), "resources[0].description");
    }

    let mut long_description = valid_value();
    first_resource_mut(&mut long_description)["description"] = json!("x".repeat(2049));
    assert_invalid_field(parse_value(&long_description), "resources[0].description");

    for version in ["", "1.0 with spaces", "?invalid"] {
        let mut value = valid_value();
        first_resource_mut(&mut value)["version"] = json!(version);
        assert_invalid_field(parse_value(&value), "resources[0].version");
    }
}

#[test]
fn rejects_excessive_lengths_for_identity_and_source_fields() {
    let cases = [
        ("id", "resources[0].id", 65, false),
        ("skillName", "resources[0].skillName", 65, false),
        ("version", "resources[0].version", 65, false),
        ("owner", "resources[0].source.owner", 40, true),
        ("repository", "resources[0].source.repository", 101, true),
        ("path", "resources[0].source.path", 1025, true),
    ];

    for (field, expected, length, source_field) in cases {
        let mut value = valid_value();
        let resource = first_resource_mut(&mut value);
        if source_field {
            resource["source"][field] = json!("a".repeat(length));
        } else {
            resource[field] = json!("a".repeat(length));
        }
        assert_invalid_field(parse_value(&value), expected);
    }
}

#[test]
fn rejects_too_many_aliases() {
    let aliases = (0..=MAX_ALIASES_PER_RESOURCE)
        .map(|index| format!("alias-{index}"))
        .collect::<Vec<_>>();
    let mut value = valid_value();
    first_resource_mut(&mut value)["aliases"] = json!(aliases);

    let error = parse_value(&value).expect_err("alias limit enforced while decoding");
    assert!(error.to_string().contains("maximum of 32 entries"));
}
