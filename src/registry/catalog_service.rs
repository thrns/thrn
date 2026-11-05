use std::{
    io::Read,
    time::{Duration, SystemTime},
};

use thiserror::Error;

use crate::models::catalog::Catalog;

use super::{
    cache::{CacheError, CatalogCache},
    catalog::{CatalogError, MAX_CATALOG_BYTES, parse_catalog_json},
    discovery::builtin_catalog,
    remote::{CatalogTransport, HttpResponse, TransportError, UreqCatalogTransport},
};

/// Catalogs refreshed within the last 24 hours are labeled fresh.
pub const FRESHNESS_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Which locally available catalog supplied discovery results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSource {
    Bundled,
    Cache,
    Refreshed,
}

impl CatalogSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bundled => "bundled metadata",
            Self::Cache => "local cache",
            Self::Refreshed => "trusted HTTPS refresh",
        }
    }
}

/// Freshness of the selected catalog, measured against the injected clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

/// The validated catalog chosen for a search or info command.
#[derive(Debug)]
pub struct CatalogSelection {
    pub catalog: Catalog,
    pub source: CatalogSource,
    pub freshness: Freshness,
    pub cache_warning: Option<String>,
}

impl CatalogSelection {
    pub fn source_label(&self) -> &'static str {
        self.source.label()
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Chooses a validated local catalog or refreshes it from the fixed trusted endpoint.
pub struct CatalogService {
    cache: Option<CatalogCache>,
    cache_setup_problem: Option<String>,
    transport: Box<dyn CatalogTransport>,
    clock: Box<dyn Clock>,
}

impl CatalogService {
    pub fn new(
        cache: CatalogCache,
        transport: Box<dyn CatalogTransport>,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            cache: Some(cache),
            cache_setup_problem: None,
            transport,
            clock,
        }
    }

    pub fn for_system() -> Self {
        match CatalogCache::for_system() {
            Ok(cache) => Self::new(
                cache,
                Box::new(UreqCatalogTransport::new()),
                Box::new(SystemClock),
            ),
            Err(error) => Self {
                cache: None,
                cache_setup_problem: Some(error.to_string()),
                transport: Box::new(UreqCatalogTransport::new()),
                clock: Box::new(SystemClock),
            },
        }
    }

    /// Read cached data if it is present and valid; otherwise use embedded metadata.
    /// This method never calls the transport.
    pub fn discover(&self) -> Result<CatalogSelection, CatalogError> {
        if let Some(cache) = self.cache.as_ref() {
            match cache.load() {
                Ok(Some(cached)) => match parse_catalog_json(&cached.payload) {
                    Ok(catalog) => {
                        let freshness = freshness(cached.refreshed_at, self.clock.now());
                        return Ok(CatalogSelection {
                            catalog,
                            source: CatalogSource::Cache,
                            freshness,
                            cache_warning: None,
                        });
                    }
                    Err(error) => {
                        return self.bundled(Some(format!(
                            "cached catalog failed validation and was ignored ({error})"
                        )));
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    return self.bundled(Some(format!(
                        "cached catalog could not be read and was ignored ({error})"
                    )));
                }
            }
        } else if let Some(problem) = self.cache_setup_problem.as_ref() {
            return self.bundled(Some(format!(
                "operating-system cache directory is unavailable ({problem})"
            )));
        }
        self.bundled(None)
    }

    /// Fetch, bound, validate, and persist a replacement catalog.
    pub fn refresh(&self) -> Result<CatalogSelection, CatalogRefreshError> {
        let response = self.transport.fetch_catalog()?;
        let payload = validated_response_bytes(response)?;
        let catalog = parse_catalog_json(&payload)?;
        let refreshed_at = self.clock.now();
        let cache = self
            .cache
            .as_ref()
            .ok_or(CatalogRefreshError::CacheUnavailable)?;
        cache.store(&payload, refreshed_at)?;
        Ok(CatalogSelection {
            catalog,
            source: CatalogSource::Refreshed,
            freshness: Freshness::Fresh,
            cache_warning: None,
        })
    }

    fn bundled(&self, cache_warning: Option<String>) -> Result<CatalogSelection, CatalogError> {
        Ok(CatalogSelection {
            catalog: builtin_catalog()?,
            source: CatalogSource::Bundled,
            freshness: Freshness::Unknown,
            cache_warning,
        })
    }
}

fn freshness(refreshed_at: SystemTime, now: SystemTime) -> Freshness {
    match now.duration_since(refreshed_at) {
        Ok(age) if age <= FRESHNESS_THRESHOLD => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
        Err(_) => Freshness::Unknown,
    }
}

fn validated_response_bytes(mut response: HttpResponse) -> Result<Vec<u8>, CatalogRefreshError> {
    if (300..400).contains(&response.status) {
        return Err(CatalogRefreshError::Redirect(response.status));
    }
    if response.status != 200 {
        return Err(CatalogRefreshError::HttpStatus(response.status));
    }
    if response
        .content_length
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(CatalogRefreshError::TooLarge);
    }

    let mut contents = Vec::with_capacity(
        response
            .content_length
            .unwrap_or(0)
            .min(MAX_CATALOG_BYTES as u64) as usize,
    );
    response
        .body
        .by_ref()
        .take((MAX_CATALOG_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|_| CatalogRefreshError::BodyRead)?;
    if contents.len() > MAX_CATALOG_BYTES {
        return Err(CatalogRefreshError::TooLarge);
    }
    if let Some(expected) = response.content_length
        && expected != contents.len() as u64
    {
        return Err(CatalogRefreshError::ContentLengthMismatch {
            expected,
            received: contents.len(),
        });
    }
    std::str::from_utf8(&contents).map_err(|_| CatalogRefreshError::InvalidUtf8)?;
    Ok(contents)
}

#[derive(Debug, Error)]
pub enum CatalogRefreshError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("catalog endpoint attempted an HTTP redirect (status {0}); redirects are not followed")]
    Redirect(u16),
    #[error("catalog endpoint returned unsuccessful HTTP status {0}")]
    HttpStatus(u16),
    #[error("catalog response exceeded the 1 MiB size limit")]
    TooLarge,
    #[error("could not read the catalog response body before the request timed out or ended")]
    BodyRead,
    #[error(
        "catalog Content-Length was {expected} bytes but the response body had {received} bytes"
    )]
    ContentLengthMismatch { expected: u64, received: usize },
    #[error("catalog response is not valid UTF-8")]
    InvalidUtf8,
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("catalog cache directory is unavailable; cannot save a successful refresh")]
    CacheUnavailable,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Cursor, Read},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde_json::Value;

    use crate::registry::{
        cache::CatalogCache,
        catalog::MAX_CATALOG_BYTES,
        discovery::CatalogResolver,
        remote::{CatalogTransport, HttpResponse, TransportError},
    };

    use super::{
        CatalogRefreshError, CatalogService, CatalogSource, Clock, FRESHNESS_THRESHOLD, Freshness,
    };

    const VALID: &[u8] = include_bytes!("../../tests/fixtures/catalog/test-only-valid-v1.json");

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "thrn-service-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("test cache directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    struct FakeTransport {
        calls: Arc<AtomicUsize>,
        result: std::sync::Mutex<Option<Result<HttpResponse, TransportError>>>,
    }

    impl FakeTransport {
        fn response(status: u16, length: Option<u64>, bytes: Vec<u8>) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                result: std::sync::Mutex::new(Some(Ok(HttpResponse::new(
                    status,
                    length,
                    Cursor::new(bytes),
                )))),
            }
        }

        fn failure(error: TransportError) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                result: std::sync::Mutex::new(Some(Err(error))),
            }
        }
    }

    impl CatalogTransport for FakeTransport {
        fn fetch_catalog(&self) -> Result<HttpResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result
                .lock()
                .map_err(|_| TransportError::RequestFailed)?
                .take()
                .unwrap_or(Err(TransportError::Connection))
        }
    }

    struct RepeatingReader {
        remaining: usize,
        byte: u8,
    }

    impl Read for RepeatingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.remaining);
            buffer[..count].fill(self.byte);
            self.remaining -= count;
            Ok(count)
        }
    }

    struct FailedReader;

    impl Read for FailedReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "injected"))
        }
    }

    fn service(directory: &Path, transport: FakeTransport, now: SystemTime) -> CatalogService {
        CatalogService::new(
            CatalogCache::new(directory.to_path_buf()),
            Box::new(transport),
            Box::new(FixedClock(now)),
        )
    }

    fn fixture_json() -> Value {
        serde_json::from_slice(VALID).expect("fixture is JSON")
    }

    fn response_service(
        directory: &Path,
        status: u16,
        length: Option<u64>,
        payload: Vec<u8>,
        now: SystemTime,
    ) -> CatalogService {
        service(
            directory,
            FakeTransport::response(status, length, payload),
            now,
        )
    }

    #[test]
    fn default_discovery_uses_bundled_data_without_network() {
        let directory = TestDir::new();
        let transport = FakeTransport::response(200, Some(VALID.len() as u64), VALID.to_vec());
        let calls = Arc::clone(&transport.calls);
        let service = service(
            directory.path(),
            transport,
            UNIX_EPOCH + Duration::from_secs(10),
        );

        let selected = service.discover().expect("bundled catalog is valid");
        assert_eq!(selected.source, CatalogSource::Bundled);
        assert_eq!(selected.freshness, Freshness::Unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_persists_and_reloads_catalog_data_that_differs_from_the_bundle() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let mut changed = fixture_json();
        changed["resources"][0]["id"] = Value::String("remote-only".to_owned());
        let payload = serde_json::to_vec(&changed).expect("fixture serialization succeeds");
        let refresh = response_service(
            directory.path(),
            200,
            Some(payload.len() as u64),
            payload,
            now,
        );

        let refreshed = refresh.refresh().expect("valid remote catalog refreshes");
        assert_eq!(refreshed.source, CatalogSource::Refreshed);
        assert_eq!(refreshed.freshness, Freshness::Fresh);
        assert_eq!(
            CatalogResolver::new(&refreshed.catalog)
                .resolve("remote-only")
                .expect("remote catalog was selected")
                .id()
                .as_str(),
            "remote-only"
        );

        let later_run = service(
            directory.path(),
            FakeTransport::failure(TransportError::Connection),
            now + Duration::from_secs(60),
        );
        let cached = later_run.discover().expect("cache can be reloaded");
        assert_eq!(cached.source, CatalogSource::Cache);
        assert_eq!(cached.freshness, Freshness::Fresh);
        let resolver = CatalogResolver::new(&cached.catalog);
        assert!(resolver.resolve("remote-only").is_ok());
        assert!(resolver.resolve("frontend-design").is_err());
    }

    #[test]
    fn cache_is_fresh_at_the_threshold_and_stale_after_it() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let cache = CatalogCache::new(directory.path().to_path_buf());
        cache
            .store(VALID, now - FRESHNESS_THRESHOLD)
            .expect("cache write succeeds");
        let transport = FakeTransport::failure(TransportError::Connection);
        let fresh = CatalogService::new(
            CatalogCache::new(directory.path().to_path_buf()),
            Box::new(transport),
            Box::new(FixedClock(now)),
        )
        .discover()
        .expect("cache is valid");
        assert_eq!(fresh.source, CatalogSource::Cache);
        assert_eq!(fresh.freshness, Freshness::Fresh);

        let stale_service = CatalogService::new(
            CatalogCache::new(directory.path().to_path_buf()),
            Box::new(FakeTransport::failure(TransportError::Connection)),
            Box::new(FixedClock(now + Duration::from_secs(1))),
        );
        let stale = stale_service
            .discover()
            .expect("stale cache remains usable");
        assert_eq!(stale.source, CatalogSource::Cache);
        assert_eq!(stale.freshness, Freshness::Stale);
    }

    #[test]
    fn future_dated_cache_is_labeled_unknown() {
        let directory = TestDir::new();
        let cache = CatalogCache::new(directory.path().to_path_buf());
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        cache
            .store(VALID, now + Duration::from_secs(5))
            .expect("cache write succeeds");
        let selected = service(
            directory.path(),
            FakeTransport::failure(TransportError::Connection),
            now,
        )
        .discover()
        .expect("future cache can be read");
        assert_eq!(selected.freshness, Freshness::Unknown);
    }

    #[test]
    fn missing_and_corrupt_caches_fall_back_to_bundled_catalog() {
        let directory = TestDir::new();
        let missing = service(
            directory.path(),
            FakeTransport::failure(TransportError::Connection),
            UNIX_EPOCH,
        )
        .discover()
        .expect("embedded catalog is valid");
        assert_eq!(missing.source, CatalogSource::Bundled);
        assert!(missing.cache_warning.is_none());
        assert!(
            CatalogResolver::new(&missing.catalog)
                .resolve("frontend-design")
                .is_ok()
        );

        let cache = CatalogCache::new(directory.path().to_path_buf());
        let truncated = "THRN-CATALOG-CACHE 1\nsource=https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json\nrefreshed_at_unix_seconds=1800000000\n\n{";
        fs::write(directory.path().join("catalog-v1.cache"), truncated)
            .expect("truncated cache fixture write succeeds");
        let truncated = service(
            directory.path(),
            FakeTransport::failure(TransportError::Connection),
            UNIX_EPOCH,
        )
        .discover()
        .expect("truncated cache falls back to bundled metadata");
        assert_eq!(truncated.source, CatalogSource::Bundled);
        assert!(truncated.cache_warning.is_some());

        fs::write(directory.path().join("catalog-v1.cache"), b"corrupt")
            .expect("corrupt fixture write succeeds");
        let corrupt = service(
            directory.path(),
            FakeTransport::failure(TransportError::Connection),
            UNIX_EPOCH,
        )
        .discover()
        .expect("bundled catalog remains available");
        assert_eq!(corrupt.source, CatalogSource::Bundled);
        assert!(corrupt.cache_warning.is_some());
        assert!(cache.load().is_err());
    }

    #[test]
    fn refresh_rejects_http_errors_redirects_transport_timeout_tls_and_read_failures() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for (status, expected) in [
            (404, "unsuccessful HTTP status 404"),
            (503, "unsuccessful HTTP status 503"),
            (302, "redirect"),
        ] {
            let result = response_service(directory.path(), status, Some(0), vec![], now).refresh();
            assert!(
                result
                    .expect_err("bad status must fail")
                    .to_string()
                    .contains(expected)
            );
        }
        for (transport_error, expected) in [
            (TransportError::Timeout, "timed out"),
            (TransportError::Tls, "TLS"),
            (TransportError::Connection, "connection failed"),
            (TransportError::InvalidContentLength, "Content-Length"),
        ] {
            let result = service(
                directory.path(),
                FakeTransport::failure(transport_error),
                now,
            )
            .refresh();
            assert!(
                result
                    .expect_err("transport failure must fail")
                    .to_string()
                    .contains(expected)
            );
        }

        let read_error = FakeTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            result: std::sync::Mutex::new(Some(Ok(HttpResponse::new(200, None, FailedReader)))),
        };
        assert!(matches!(
            service(directory.path(), read_error, now).refresh(),
            Err(CatalogRefreshError::BodyRead)
        ));
    }

    #[test]
    fn refresh_enforces_streaming_limits_for_chunked_and_misreported_lengths() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let chunked = FakeTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            result: std::sync::Mutex::new(Some(Ok(HttpResponse::new(
                200,
                None,
                RepeatingReader {
                    remaining: MAX_CATALOG_BYTES + 1,
                    byte: b'x',
                },
            )))),
        };
        assert!(matches!(
            service(directory.path(), chunked, now).refresh(),
            Err(CatalogRefreshError::TooLarge)
        ));

        let declared_oversize = response_service(
            directory.path(),
            200,
            Some(MAX_CATALOG_BYTES as u64 + 1),
            Vec::new(),
            now,
        );
        assert!(matches!(
            declared_oversize.refresh(),
            Err(CatalogRefreshError::TooLarge)
        ));

        let incorrect_length = response_service(
            directory.path(),
            200,
            Some(VALID.len() as u64 + 1),
            VALID.to_vec(),
            now,
        );
        assert!(matches!(
            incorrect_length.refresh(),
            Err(CatalogRefreshError::ContentLengthMismatch { .. })
        ));

        let underreported_length = response_service(
            directory.path(),
            200,
            Some(VALID.len() as u64 - 1),
            VALID.to_vec(),
            now,
        );
        assert!(matches!(
            underreported_length.refresh(),
            Err(CatalogRefreshError::ContentLengthMismatch { .. })
        ));
    }

    #[test]
    fn refresh_rejects_invalid_utf8_malformed_json_and_unsupported_schema() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let invalid_utf8 = response_service(directory.path(), 200, None, vec![0xff], now);
        assert!(matches!(
            invalid_utf8.refresh(),
            Err(CatalogRefreshError::InvalidUtf8)
        ));

        let malformed = response_service(directory.path(), 200, None, b"{".to_vec(), now);
        assert!(matches!(
            malformed.refresh(),
            Err(CatalogRefreshError::Catalog(_))
        ));

        let mut unsupported = fixture_json();
        unsupported["schemaVersion"] = Value::from(9);
        let bytes = serde_json::to_vec(&unsupported).expect("fixture serialization succeeds");
        let result = response_service(directory.path(), 200, None, bytes, now).refresh();
        assert!(matches!(result, Err(CatalogRefreshError::Catalog(_))));
    }

    #[test]
    fn refresh_revalidates_duplicate_ids_aliases_commits_and_paths() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        let duplicate_id = {
            let mut value = fixture_json();
            let duplicate = value["resources"][0].clone();
            value["resources"]
                .as_array_mut()
                .expect("resources array")
                .push(duplicate);
            serde_json::to_vec(&value).expect("fixture serialization succeeds")
        };
        let result = response_service(directory.path(), 200, None, duplicate_id, now).refresh();
        assert!(matches!(result, Err(CatalogRefreshError::Catalog(_))));

        let duplicate_alias = {
            let mut value = fixture_json();
            value["resources"][0]["aliases"] = serde_json::json!(["idea-lab", "IDEA-LAB"]);
            serde_json::to_vec(&value).expect("fixture serialization succeeds")
        };
        let result = response_service(directory.path(), 200, None, duplicate_alias, now).refresh();
        assert!(matches!(result, Err(CatalogRefreshError::Catalog(_))));

        let invalid_commit = {
            let mut value = fixture_json();
            value["resources"][0]["source"]["commit"] = Value::String("abc".to_owned());
            serde_json::to_vec(&value).expect("fixture serialization succeeds")
        };
        let result = response_service(directory.path(), 200, None, invalid_commit, now).refresh();
        assert!(matches!(result, Err(CatalogRefreshError::Catalog(_))));

        let invalid_path = {
            let mut value = fixture_json();
            value["resources"][0]["source"]["path"] = Value::String("../escape".to_owned());
            serde_json::to_vec(&value).expect("fixture serialization succeeds")
        };
        let result = response_service(directory.path(), 200, None, invalid_path, now).refresh();
        assert!(matches!(result, Err(CatalogRefreshError::Catalog(_))));
    }

    #[test]
    fn failed_refresh_preserves_a_previous_valid_cache() {
        let directory = TestDir::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let cache = CatalogCache::new(directory.path().to_path_buf());
        cache
            .store(VALID, now)
            .expect("initial cache write succeeds");
        let original =
            fs::read(directory.path().join("catalog-v1.cache")).expect("cache can be read");

        let result = service(
            directory.path(),
            FakeTransport::failure(TransportError::Timeout),
            now + Duration::from_secs(5),
        )
        .refresh();
        assert!(result.is_err());
        assert_eq!(
            fs::read(directory.path().join("catalog-v1.cache")).expect("cache remains readable"),
            original
        );
    }
}
