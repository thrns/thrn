# Catalog parser fixtures — test-only

Every JSON document in this directory is synthetic test input, including the empty catalog and
explicit `uncharted` alias fixture used to test catalog resolution. Resource names, GitHub owner
and repository values, paths, and versions are not a published catalog. The static full-length
hexadecimal commit strings exist only to exercise lexical validation; they are not verified
against the fixtures' synthetic source identities. These fixtures must not be copied into a
production catalog.
