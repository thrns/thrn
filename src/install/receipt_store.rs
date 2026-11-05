use std::{fs, io, path::PathBuf};

use thiserror::Error;

/// Read-only inspection of the future THRN-managed installation receipt location.
///
/// This milestone recognizes only missing or empty storage. M2-05 owns the durable receipt schema
/// and ownership design; until then, any existing content is reported as unsupported.
pub struct ReceiptStore {
    directory: PathBuf,
}

impl ReceiptStore {
    pub fn for_system() -> Result<Self, ReceiptDiscoveryError> {
        let data_directory = if let Some(path) = std::env::var_os("THRN_DATA_DIR") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(ReceiptDiscoveryError::RelativeDirectoryOverride);
            }
            path
        } else {
            directories::ProjectDirs::from("im", "thrn", "thrn")
                .ok_or(ReceiptDiscoveryError::DirectoryUnavailable)?
                .data_dir()
                .to_path_buf()
        };

        Ok(Self::new(data_directory.join("receipts")))
    }

    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Return success only when receipt storage is absent or empty.
    ///
    /// No directory is created and no entry contents are read. Since M2-05 has not defined a receipt
    /// format, the presence of any content must not be mistaken for an empty managed state.
    pub fn require_empty(&self) -> Result<(), ReceiptDiscoveryError> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(ReceiptDiscoveryError::UnsafeDirectory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ReceiptDiscoveryError::Io {
                    action: "inspect",
                    source,
                });
            }
        }

        let mut entries =
            fs::read_dir(&self.directory).map_err(|source| ReceiptDiscoveryError::Io {
                action: "read",
                source,
            })?;
        match entries.next() {
            None => Ok(()),
            Some(Err(source)) => Err(ReceiptDiscoveryError::Io {
                action: "read",
                source,
            }),
            Some(Ok(_)) => Err(ReceiptDiscoveryError::UnrecognizedContent),
        }
    }
}

#[derive(Debug, Error)]
pub enum ReceiptDiscoveryError {
    #[error("no operating-system application-data directory is available")]
    DirectoryUnavailable,
    #[error("THRN_DATA_DIR must be an absolute path")]
    RelativeDirectoryOverride,
    #[error("could not {action} THRN receipt storage ({source})")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("THRN receipt storage is not a real directory")]
    UnsafeDirectory,
    #[error(
        "THRN receipt storage contains unrecognized content; durable installation receipts are not implemented yet"
    )]
    UnrecognizedContent,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{ReceiptDiscoveryError, ReceiptStore};

    #[test]
    fn missing_receipt_directory_is_empty_without_creating_it() {
        let temporary = tempdir().expect("temporary data directory should be created");
        let receipts = temporary.path().join("receipts");

        ReceiptStore::new(receipts.clone())
            .require_empty()
            .expect("missing receipt storage represents an empty state");

        assert!(!receipts.exists());
    }

    #[test]
    fn empty_receipt_directory_is_read_only() {
        let temporary = tempdir().expect("temporary directory should be created");
        let receipts = temporary.path().join("receipts");
        fs::create_dir(&receipts).expect("empty receipt directory should be created");

        ReceiptStore::new(receipts.clone())
            .require_empty()
            .expect("empty receipt storage is supported");

        assert_eq!(
            fs::read_dir(receipts)
                .expect("directory remains readable")
                .count(),
            0
        );
    }

    #[test]
    fn unexpected_receipt_content_fails_without_modifying_it() {
        let temporary = tempdir().expect("temporary directory should be created");
        let receipts = temporary.path().join("receipts");
        fs::create_dir(&receipts).expect("receipt directory should be created");
        let unexpected = receipts.join("install.json");
        fs::write(&unexpected, b"future receipt data")
            .expect("unrecognized content should be written");

        let error = ReceiptStore::new(receipts)
            .require_empty()
            .expect_err("unsupported content cannot be reported as empty");

        assert!(matches!(error, ReceiptDiscoveryError::UnrecognizedContent));
        assert_eq!(
            fs::read(unexpected).expect("content remains readable"),
            b"future receipt data"
        );
    }

    #[test]
    fn non_directory_receipt_storage_fails_clearly() {
        let temporary = tempdir().expect("temporary directory should be created");
        let path = temporary.path().join("receipts");
        fs::write(&path, b"unexpected file").expect("file should be written");

        assert!(matches!(
            ReceiptStore::new(path).require_empty(),
            Err(ReceiptDiscoveryError::UnsafeDirectory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_receipt_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().expect("temporary directory should be created");
        let real = temporary.path().join("real-receipts");
        let linked = temporary.path().join("receipts");
        fs::create_dir(&real).expect("real receipt directory should be created");
        symlink(&real, &linked).expect("receipt directory symlink should be created");

        assert!(matches!(
            ReceiptStore::new(linked).require_empty(),
            Err(ReceiptDiscoveryError::UnsafeDirectory)
        ));
    }
}
