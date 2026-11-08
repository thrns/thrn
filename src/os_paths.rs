//! Central OS-specific locations used by THRN.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OsPaths {
    user_home: PathBuf,
    application_config_dir: PathBuf,
    application_data_dir: PathBuf,
    catalog_cache_dir: PathBuf,
}

impl OsPaths {
    pub(crate) fn for_system() -> Result<Self, OsPathError> {
        #[cfg(windows)]
        {
            let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
            let roaming = std::env::var_os("APPDATA")
                .map(|value| absolute_environment_path("APPDATA", value))
                .transpose()?;
            let local = std::env::var_os("LOCALAPPDATA")
                .map(|value| absolute_environment_path("LOCALAPPDATA", value))
                .transpose()?;
            let base = if profile.is_none() || roaming.is_none() || local.is_none() {
                Some(directories::BaseDirs::new().ok_or(OsPathError::DirectoryUnavailable)?)
            } else {
                None
            };
            let user_home = profile
                .or_else(|| base.as_ref().map(|dirs| dirs.home_dir().to_path_buf()))
                .ok_or(OsPathError::DirectoryUnavailable)?;
            let roaming = roaming
                .or_else(|| base.as_ref().map(|dirs| dirs.config_dir().to_path_buf()))
                .ok_or(OsPathError::DirectoryUnavailable)?;
            let local = local
                .or_else(|| base.as_ref().map(|dirs| dirs.cache_dir().to_path_buf()))
                .ok_or(OsPathError::DirectoryUnavailable)?;
            let app_fragment = Path::new("thrn").join("thrn");
            let application_data_dir = std::env::var_os("THRN_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| roaming.join(&app_fragment).join("data"));
            let catalog_cache_dir = std::env::var_os("THRN_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| local.join(&app_fragment).join("cache"));
            return Ok(Self {
                user_home,
                application_config_dir: roaming.join(app_fragment).join("config"),
                application_data_dir,
                catalog_cache_dir,
            });
        }

        #[cfg(not(windows))]
        {
            let base = directories::BaseDirs::new().ok_or(OsPathError::DirectoryUnavailable)?;
            let project = directories::ProjectDirs::from("im", "thrn", "thrn")
                .ok_or(OsPathError::DirectoryUnavailable)?;
            let user_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| base.home_dir().to_path_buf());
            let application_data_dir = std::env::var_os("THRN_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| project.data_dir().to_path_buf());
            let catalog_cache_dir = std::env::var_os("THRN_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| project.cache_dir().to_path_buf());
            Ok(Self {
                user_home,
                application_config_dir: project.config_dir().to_path_buf(),
                application_data_dir,
                catalog_cache_dir,
            })
        }
    }

    pub(crate) fn user_home(&self) -> &Path {
        &self.user_home
    }

    #[allow(dead_code)]
    pub(crate) fn application_config_dir(&self) -> &Path {
        &self.application_config_dir
    }

    pub(crate) fn application_data_dir(&self) -> Result<PathBuf, OsPathError> {
        if !self.application_data_dir.is_absolute() {
            return Err(OsPathError::RelativeDataOverride);
        }
        resolve_data_directory(&self.application_data_dir)
    }

    pub(crate) fn catalog_cache_dir(&self) -> Result<&Path, OsPathError> {
        if !self.catalog_cache_dir.is_absolute() {
            return Err(OsPathError::RelativeCacheOverride);
        }
        Ok(&self.catalog_cache_dir)
    }

    #[cfg(test)]
    pub(crate) fn for_test(user_home: PathBuf) -> Self {
        Self {
            user_home,
            application_config_dir: PathBuf::new(),
            application_data_dir: PathBuf::new(),
            catalog_cache_dir: PathBuf::new(),
        }
    }
}

#[cfg(windows)]
fn absolute_environment_path(
    variable: &'static str,
    value: OsString,
) -> Result<PathBuf, OsPathError> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(OsPathError::RelativePlatformDirectory { variable });
    }
    Ok(path)
}

/// Resolve the data-root aliases used by the existing receipt store without creating paths.
/// Existing ancestors are canonicalized (including OS aliases); the configured data root itself
/// is rejected when it is a symbolic link or reparse point.
pub(crate) fn resolve_data_directory(path: &Path) -> Result<PathBuf, OsPathError> {
    let mut current = path;
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if missing.is_empty() && is_link_or_reparse(&metadata) {
                    return Err(OsPathError::UnsafeDataDirectory);
                }
                let mut resolved = current
                    .canonicalize()
                    .map_err(|error| path_io("resolve application data", error))?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .ok_or(OsPathError::UnsafeDataDirectory)?;
                missing.push(name.to_os_string());
                current = current.parent().ok_or(OsPathError::UnsafeDataDirectory)?;
            }
            Err(error) => return Err(path_io("inspect application data", error)),
        }
    }
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0;
    }
    #[allow(unreachable_code)]
    false
}

fn path_io(action: &'static str, error: io::Error) -> OsPathError {
    OsPathError::Io {
        action,
        kind: error.kind(),
    }
}

#[derive(Debug, Error)]
pub(crate) enum OsPathError {
    #[error("OS configuration, data, cache, or home directory is unavailable")]
    DirectoryUnavailable,
    #[error("THRN_DATA_DIR must be an absolute path")]
    RelativeDataOverride,
    #[error("THRN_CACHE_DIR must be an absolute path")]
    RelativeCacheOverride,
    #[cfg(windows)]
    #[error("{variable} must be an absolute path")]
    RelativePlatformDirectory { variable: &'static str },
    #[error("application data directory is unsafe")]
    UnsafeDataDirectory,
    #[error("could not {action} a THRN operating-system path ({kind})")]
    Io {
        action: &'static str,
        kind: io::ErrorKind,
    },
}
