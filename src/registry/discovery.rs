use std::{collections::HashMap, str::FromStr};

use thiserror::Error;

use crate::models::catalog::{Catalog, IdentifierError, Resource, SkillId};

use super::catalog::{CatalogError, parse_catalog_json};

const BUILTIN_CATALOG_JSON: &str = include_str!("builtin-v1.json");

/// Parse the compile-time bundled catalog through the same validator used for external catalogs.
pub fn builtin_catalog() -> Result<Catalog, CatalogError> {
    parse_catalog_json(BUILTIN_CATALOG_JSON.as_bytes())
}

/// Exact catalog ID and explicit alias lookup plus deterministic text search.
pub struct CatalogResolver<'catalog> {
    catalog: &'catalog Catalog,
    lookup: HashMap<String, usize>,
}

impl<'catalog> CatalogResolver<'catalog> {
    /// Build the index from a catalog returned by the validated catalog parser.
    pub fn new(catalog: &'catalog Catalog) -> Self {
        let mut lookup = HashMap::new();
        for (index, resource) in catalog.resources().iter().enumerate() {
            lookup.insert(resource.id().lookup_key(), index);
            for alias in resource.aliases() {
                lookup.insert(alias.lookup_key(), index);
            }
        }
        Self { catalog, lookup }
    }

    /// Resolve a canonical catalog ID or explicit alias using ASCII case-insensitive equality.
    pub fn resolve(&self, identifier: &str) -> Result<&'catalog Resource, ResolutionError> {
        let id = SkillId::from_str(identifier).map_err(ResolutionError::InvalidIdentifier)?;
        self.lookup
            .get(&id.lookup_key())
            .and_then(|index| self.catalog.resources().get(*index))
            .ok_or_else(|| ResolutionError::UnknownIdentifier {
                identifier: identifier.to_owned(),
            })
    }

    /// Search catalog text and return matches in canonical ID order.
    pub fn search(&self, query: Option<&str>) -> Vec<&'catalog Resource> {
        let query = query.map(str::to_lowercase);
        let mut matches = self
            .catalog
            .resources()
            .iter()
            .filter(|resource| {
                query.as_ref().is_none_or(|query| {
                    resource.id().as_str().to_lowercase().contains(query)
                        || resource
                            .skill_name()
                            .as_str()
                            .to_lowercase()
                            .contains(query)
                        || resource.display_name().to_lowercase().contains(query)
                        || resource.description().to_lowercase().contains(query)
                        || resource
                            .aliases()
                            .iter()
                            .any(|alias| alias.as_str().to_lowercase().contains(query))
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|resource| resource.id().lookup_key());
        matches
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ResolutionError {
    #[error("invalid skill identifier: {0}")]
    InvalidIdentifier(#[source] IdentifierError),
    #[error(
        "no catalog entry matches {identifier:?}; run `thrn skills search` to see available IDs and aliases"
    )]
    UnknownIdentifier { identifier: String },
}

#[cfg(test)]
mod tests {
    use crate::registry::catalog::parse_catalog_json;

    use super::{CatalogResolver, builtin_catalog};

    const VALID_FIXTURE: &str =
        include_str!("../../tests/fixtures/catalog/test-only-valid-v1.json");
    const MULTIPLE_FIXTURE: &str =
        include_str!("../../tests/fixtures/catalog/test-only-multiple-resources-v1.json");
    const EMPTY_FIXTURE: &str =
        include_str!("../../tests/fixtures/catalog/test-only-empty-v1.json");
    const UNCHARTED_ALIAS_FIXTURE: &str =
        include_str!("../../tests/fixtures/catalog/test-only-uncharted-alias-v1.json");

    fn fixture_catalog(json: &str) -> crate::models::catalog::Catalog {
        parse_catalog_json(json.as_bytes()).expect("test fixture is a valid catalog")
    }

    #[test]
    fn built_in_catalog_is_validated_and_has_explicit_alias() {
        let catalog = builtin_catalog().expect("bundled catalog passes the schema validator");
        let resolver = CatalogResolver::new(&catalog);

        let by_alias = resolver.resolve("FRONTEND").expect("alias resolves");
        let by_id = resolver
            .resolve("FRONTEND-DESIGN")
            .expect("canonical ID resolves case-insensitively");
        assert_eq!(by_alias.id().as_str(), "frontend-design");
        assert_eq!(by_alias, by_id);
    }

    #[test]
    fn explicit_uncharted_alias_resolves_to_the_fixture_canonical_record() {
        let catalog = fixture_catalog(UNCHARTED_ALIAS_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);

        let resource = resolver
            .resolve("uncharted")
            .expect("explicit alias resolves");
        assert_eq!(resource.id().as_str(), "creative-lab");
        assert_eq!(resource.skill_name().as_str(), "uncharted-ideation");
        assert_eq!(
            resource,
            resolver.resolve("CREATIVE-LAB").expect("canonical ID")
        );
    }

    #[test]
    fn skill_directory_name_is_searchable_but_not_an_implicit_lookup_alias() {
        let catalog = fixture_catalog(VALID_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);

        assert_eq!(
            resolver.search(Some("ideation"))[0].skill_name().as_str(),
            "uncharted-ideation"
        );
        assert!(resolver.resolve("uncharted-ideation").is_err());
    }

    #[test]
    fn text_search_matches_descriptions_and_sorts_by_canonical_id() {
        let catalog = fixture_catalog(MULTIPLE_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);
        let results = resolver.search(Some("synthetic"));

        let ids = results
            .iter()
            .map(|resource| resource.id().as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["refactor-helper", "research-helper"]);

        let all = resolver.search(None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id().as_str(), "refactor-helper");

        let canonical_id_search = resolver.search(Some("RESEARCH-HELPER"));
        assert_eq!(canonical_id_search.len(), 1);
        assert_eq!(canonical_id_search[0].id().as_str(), "research-helper");
    }

    #[test]
    fn text_search_matches_explicit_aliases_case_insensitively() {
        let catalog = fixture_catalog(UNCHARTED_ALIAS_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);

        let alias_results = resolver.search(Some("IDEA-WORKSHOP"));
        assert_eq!(alias_results.len(), 1);
        assert_eq!(alias_results[0].id().as_str(), "creative-lab");

        let name_results = resolver.search(Some("IDEATION"));
        assert_eq!(name_results.len(), 1);
        assert_eq!(name_results[0].skill_name().as_str(), "uncharted-ideation");

        let display_results = resolver.search(Some("CREATIVE LAB"));
        assert_eq!(display_results.len(), 1);

        let description_results = resolver.search(Some("IMAGINATIVE"));
        assert_eq!(description_results.len(), 1);
    }

    #[test]
    fn empty_catalog_fixture_returns_empty_search_results() {
        let catalog = fixture_catalog(EMPTY_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);
        assert!(resolver.search(None).is_empty());
        assert!(resolver.search(Some("anything")).is_empty());
    }

    #[test]
    fn invalid_and_unknown_identifiers_return_distinct_actionable_errors() {
        let catalog = fixture_catalog(VALID_FIXTURE);
        let resolver = CatalogResolver::new(&catalog);

        let invalid = resolver.resolve("bad--id").expect_err("invalid grammar");
        assert!(invalid.to_string().contains("invalid skill identifier"));
        assert!(invalid.to_string().contains("single hyphens"));

        let unknown = resolver.resolve("no-such-skill").expect_err("unknown ID");
        assert!(unknown.to_string().contains("thrn skills search"));
    }
}
