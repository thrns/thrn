use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tempfile::NamedTempFile;
use thiserror::Error;

use super::{catalog::MAX_CATALOG_BYTES, remote::TRUSTED_CATALOG_URL};

const CACHE_FILE_NAME: &str = "catalog-v1.cache";
const CACHE_MAGIC: &str = "THRN-CATALOG-CACHE 1";
const MAX_CACHE_HEADER_BYTES: usize = 256;
const MAX_CACHE_FILE_BYTES: usize = MAX_CATALOG_BYTES + MAX_CACHE_HEADER_BYTES;

/// A catalog payload and timestamp read from the cache envelope.
pub struct CachedCatalogBytes {
    pub payload: Vec<u8>,
    pub refreshed_at: SystemTime,
}

/// Persistent, single-file catalog cache under the operating system cache directory.
pub struct CatalogCache {
    directory: PathBuf,
    committer: Box<dyn CacheCommitter>,
}

impl CatalogCache {
    pub fn for_system() -> Result<Self, CacheError> {
        let paths = crate::os_paths::OsPaths::for_system().map_err(map_os_path_error)?;
        Ok(Self::new(
            paths
                .catalog_cache_dir()
                .map_err(map_os_path_error)?
                .to_path_buf(),
        ))
    }

    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            committer: Box::new(AtomicCacheCommitter),
        }
    }

    #[cfg(test)]
    fn with_committer(directory: PathBuf, committer: Box<dyn CacheCommitter>) -> Self {
        Self {
            directory,
            committer,
        }
    }

    fn file_path(&self) -> PathBuf {
        self.directory.join(CACHE_FILE_NAME)
    }

    /// Read the envelope and source metadata. Catalog JSON is validated by the caller.
    pub fn load(&self) -> Result<Option<CachedCatalogBytes>, CacheError> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(CacheError::UnsafeDirectory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CacheError::Io {
                    action: "inspect the cache directory",
                    source,
                });
            }
        }
        let path = self.file_path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CacheError::Io {
                    action: "inspect",
                    source,
                });
            }
        };
        if !metadata.file_type().is_file() {
            return Err(CacheError::UnsafeFile);
        }

        let mut file = open_cache_file(&path)?;
        let file_type = file
            .metadata()
            .map_err(|source| CacheError::Io {
                action: "inspect",
                source,
            })?
            .file_type();
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(CacheError::UnsafeFile);
        }

        let mut contents = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_CACHE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut contents)
            .map_err(|source| CacheError::Io {
                action: "read",
                source,
            })?;
        if contents.len() > MAX_CACHE_FILE_BYTES {
            return Err(CacheError::TooLarge);
        }

        decode_envelope(&contents).map(Some)
    }

    /// Store a payload that the catalog validator has already accepted.
    pub fn store(&self, payload: &[u8], refreshed_at: SystemTime) -> Result<(), CacheError> {
        if payload.len() > MAX_CATALOG_BYTES {
            return Err(CacheError::TooLarge);
        }
        let timestamp = refreshed_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CacheError::InvalidTimestamp)?
            .as_secs();
        let envelope = encode_envelope(payload, timestamp);

        fs::create_dir_all(&self.directory).map_err(|source| CacheError::Io {
            action: "create the cache directory",
            source,
        })?;
        let directory_metadata =
            fs::symlink_metadata(&self.directory).map_err(|source| CacheError::Io {
                action: "inspect the cache directory",
                source,
            })?;
        if !directory_metadata.file_type().is_dir() || directory_metadata.file_type().is_symlink() {
            return Err(CacheError::UnsafeDirectory);
        }

        let target = self.file_path();
        match fs::symlink_metadata(&target) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(CacheError::UnsafeFile);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CacheError::Io {
                    action: "inspect",
                    source,
                });
            }
        }

        self.committer
            .commit(&self.directory, &target, &envelope)
            .map_err(|source| CacheError::Io {
                action: "replace the catalog cache",
                source,
            })
    }
}

fn open_cache_file(path: &Path) -> Result<File, CacheError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself so the post-open regular-file check rejects links.
        options.custom_flags(0x0020_0000);
    }
    options.open(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            CacheError::UnsafeFile
        } else {
            CacheError::Io {
                action: "open",
                source,
            }
        }
    })
}

fn encode_envelope(payload: &[u8], timestamp: u64) -> Vec<u8> {
    let header = format!(
        "{CACHE_MAGIC}\nsource={TRUSTED_CATALOG_URL}\nrefreshed_at_unix_seconds={timestamp}\n\n"
    );
    let mut output = Vec::with_capacity(header.len().saturating_add(payload.len()));
    output.extend_from_slice(header.as_bytes());
    output.extend_from_slice(payload);
    output
}

fn decode_envelope(contents: &[u8]) -> Result<CachedCatalogBytes, CacheError> {
    let separator = contents
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .ok_or(CacheError::MalformedHeader)?;
    if separator > MAX_CACHE_HEADER_BYTES {
        return Err(CacheError::MalformedHeader);
    }
    let header =
        std::str::from_utf8(&contents[..separator]).map_err(|_| CacheError::MalformedHeader)?;
    let mut lines = header.lines();
    if lines.next() != Some(CACHE_MAGIC) {
        return Err(CacheError::UnsupportedFormat);
    }
    let source = lines
        .next()
        .and_then(|line| line.strip_prefix("source="))
        .ok_or(CacheError::MalformedHeader)?;
    if source != TRUSTED_CATALOG_URL {
        return Err(CacheError::WrongSource);
    }
    let timestamp = lines
        .next()
        .and_then(|line| line.strip_prefix("refreshed_at_unix_seconds="))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(CacheError::InvalidTimestamp)?;
    if lines.next().is_some() {
        return Err(CacheError::MalformedHeader);
    }
    let refreshed_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(timestamp))
        .ok_or(CacheError::InvalidTimestamp)?;
    let payload_start = separator + 2;
    let payload = contents
        .get(payload_start..)
        .ok_or(CacheError::MalformedHeader)?
        .to_vec();
    if payload.len() > MAX_CATALOG_BYTES {
        return Err(CacheError::TooLarge);
    }
    Ok(CachedCatalogBytes {
        payload,
        refreshed_at,
    })
}

trait CacheCommitter: Send + Sync {
    fn commit(&self, directory: &Path, target: &Path, contents: &[u8]) -> io::Result<()>;
}

struct AtomicCacheCommitter;

impl CacheCommitter for AtomicCacheCommitter {
    fn commit(&self, directory: &Path, target: &Path, contents: &[u8]) -> io::Result<()> {
        let mut temporary = NamedTempFile::new_in(directory)?;
        temporary.write_all(contents)?;
        temporary.as_file().sync_all()?;
        temporary.persist(target).map_err(|error| error.error)?;
        if let Ok(directory_file) = File::open(directory) {
            let _ = directory_file.sync_all();
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("no operating-system cache directory is available")]
    DirectoryUnavailable,
    #[error("THRN_CACHE_DIR must be an absolute path")]
    RelativeDirectoryOverride,
    #[error("could not resolve the operating-system catalog cache path: {0}")]
    PathResolution(String),
    #[error("could not {action} the catalog cache ({source})")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("catalog cache file is a symlink or not a regular file")]
    UnsafeFile,
    #[error("catalog cache directory is a symlink or not a directory")]
    UnsafeDirectory,
    #[error("catalog cache exceeds the supported size limit")]
    TooLarge,
    #[error("catalog cache header is malformed")]
    MalformedHeader,
    #[error("catalog cache format is incompatible with this version")]
    UnsupportedFormat,
    #[error("catalog cache identifies an unexpected source")]
    WrongSource,
    #[error("catalog cache timestamp is invalid")]
    InvalidTimestamp,
}

fn map_os_path_error(error: crate::os_paths::OsPathError) -> CacheError {
    match error {
        crate::os_paths::OsPathError::DirectoryUnavailable => CacheError::DirectoryUnavailable,
        crate::os_paths::OsPathError::RelativeCacheOverride => {
            CacheError::RelativeDirectoryOverride
        }
        other => CacheError::PathResolution(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::Path,
        time::{Duration, UNIX_EPOCH},
    };

    use super::{
        CACHE_FILE_NAME, CACHE_MAGIC, CacheCommitter, CacheError, CatalogCache,
        MAX_CACHE_FILE_BYTES, TRUSTED_CATALOG_URL, encode_envelope,
    };

    const VALID_CATALOG: &[u8] =
        include_bytes!("../../tests/fixtures/catalog/test-only-valid-v1.json");

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "thrn-cache-test-{}-{:?}",
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

    struct FailingCommitter;

    impl CacheCommitter for FailingCommitter {
        fn commit(&self, _directory: &Path, _target: &Path, _contents: &[u8]) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure",
            ))
        }
    }

    #[test]
    fn stores_and_reloads_a_single_cache_envelope() {
        let directory = TestDir::new();
        let cache = CatalogCache::new(directory.path().to_path_buf());
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        cache
            .store(VALID_CATALOG, timestamp)
            .expect("cache write succeeds");
        let loaded = cache
            .load()
            .expect("cache read succeeds")
            .expect("cache exists");
        assert_eq!(loaded.payload, VALID_CATALOG);
        assert_eq!(loaded.refreshed_at, timestamp);
    }

    #[test]
    fn rejects_incompatible_truncated_and_corrupt_cache_envelopes() {
        let directory = TestDir::new();
        let path = directory.path().join(CACHE_FILE_NAME);
        let cache = CatalogCache::new(directory.path().to_path_buf());

        fs::write(&path, b"broken").expect("fixture write succeeds");
        assert!(matches!(cache.load(), Err(CacheError::MalformedHeader)));

        fs::write(
            &path,
            format!(
                "THRN-CATALOG-CACHE 2\nsource={TRUSTED_CATALOG_URL}\nrefreshed_at_unix_seconds=1\n\n"
            ),
        )
        .expect("fixture write succeeds");
        assert!(matches!(cache.load(), Err(CacheError::UnsupportedFormat)));

        let mut truncated = encode_envelope(VALID_CATALOG, 1_800_000_000);
        truncated.truncate(truncated.len() - 2);
        fs::write(&path, truncated).expect("fixture write succeeds");
        let loaded = cache.load().expect("envelope itself remains readable");
        assert!(loaded.is_some());
    }

    #[test]
    fn rejects_cache_files_above_the_catalog_size_limit() {
        let directory = TestDir::new();
        let path = directory.path().join(CACHE_FILE_NAME);
        fs::write(&path, vec![b'x'; MAX_CACHE_FILE_BYTES + 1])
            .expect("oversized cache fixture should be written");
        let cache = CatalogCache::new(directory.path().to_path_buf());
        assert!(matches!(cache.load(), Err(CacheError::TooLarge)));
    }

    #[test]
    fn rejects_cache_files_that_are_not_regular_files() {
        let directory = TestDir::new();
        let path = directory.path().join(CACHE_FILE_NAME);
        fs::create_dir(&path).expect("nonregular fixture should be created");
        let cache = CatalogCache::new(directory.path().to_path_buf());
        assert!(matches!(cache.load(), Err(CacheError::UnsafeFile)));
        assert!(matches!(
            cache.store(VALID_CATALOG, UNIX_EPOCH),
            Err(CacheError::UnsafeFile)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_as_the_cache_directory() {
        use std::os::unix::fs::symlink;

        let directory = TestDir::new();
        let real = directory.path().join("real");
        let linked = directory.path().join("linked");
        fs::create_dir_all(&real).expect("real cache directory is created");
        symlink(&real, &linked).expect("cache directory symlink is created");
        let cache = CatalogCache::new(linked);
        assert!(matches!(cache.load(), Err(CacheError::UnsafeDirectory)));
        assert!(matches!(
            cache.store(VALID_CATALOG, UNIX_EPOCH),
            Err(CacheError::UnsafeDirectory)
        ));
    }

    #[test]
    fn reports_cache_directory_write_failures() {
        let directory = TestDir::new();
        let blocked = directory.path().join("not-a-directory");
        fs::write(&blocked, b"fixture").expect("blocking fixture should be written");
        let cache = CatalogCache::new(blocked.join("cache"));
        assert!(matches!(
            cache.store(VALID_CATALOG, UNIX_EPOCH),
            Err(CacheError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_cache_file_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let directory = TestDir::new();
        let outside = directory.path().join("outside.json");
        let target = directory.path().join(CACHE_FILE_NAME);
        fs::write(&outside, VALID_CATALOG).expect("fixture write succeeds");
        symlink(&outside, &target).expect("symlink fixture should be created");
        let cache = CatalogCache::new(directory.path().to_path_buf());

        assert!(matches!(cache.load(), Err(CacheError::UnsafeFile)));
        assert!(matches!(
            cache.store(VALID_CATALOG, UNIX_EPOCH),
            Err(CacheError::UnsafeFile)
        ));
        assert_eq!(
            fs::read(outside).expect("external target remains readable"),
            VALID_CATALOG
        );
    }

    #[test]
    fn failed_atomic_replacement_keeps_the_previous_valid_cache() {
        let directory = TestDir::new();
        let first = CatalogCache::new(directory.path().to_path_buf());
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        first
            .store(VALID_CATALOG, timestamp)
            .expect("initial cache write succeeds");
        let original =
            fs::read(directory.path().join(CACHE_FILE_NAME)).expect("original cache can be read");

        let failing = CatalogCache::with_committer(
            directory.path().to_path_buf(),
            Box::new(FailingCommitter),
        );
        assert!(matches!(
            failing.store(b"replacement", timestamp),
            Err(CacheError::Io { .. })
        ));
        assert_eq!(
            fs::read(directory.path().join(CACHE_FILE_NAME)).expect("cache remains readable"),
            original
        );
    }

    #[test]
    fn rejects_caches_from_a_different_source_and_invalid_timestamps() {
        let directory = TestDir::new();
        let path = directory.path().join(CACHE_FILE_NAME);
        let cache = CatalogCache::new(directory.path().to_path_buf());
        let wrong_source = format!(
            "{CACHE_MAGIC}\nsource=https://example.invalid/catalog.json\nrefreshed_at_unix_seconds=1\n\n"
        );
        fs::write(&path, wrong_source).expect("fixture write succeeds");
        assert!(matches!(cache.load(), Err(CacheError::WrongSource)));

        let invalid_time = format!(
            "{CACHE_MAGIC}\nsource={TRUSTED_CATALOG_URL}\nrefreshed_at_unix_seconds=not-a-time\n\n"
        );
        fs::write(&path, invalid_time).expect("fixture write succeeds");
        assert!(matches!(cache.load(), Err(CacheError::InvalidTimestamp)));
    }
}
