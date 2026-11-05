use std::{
    io::{self, Read},
    time::Duration,
};

use thiserror::Error;

use crate::models::catalog::{Resource, ResourceKind, SourceProvider};

use super::{
    archive::{ArchiveError, MAX_COMPRESSED_ARCHIVE_BYTES, ValidatedArchive, validate_zip_archive},
    selection::{SelectedSkill, SelectionError, select_skill_directory},
    staging::{StageError, StagedSkill, TempDirFactory, stage_selected_skill},
};

/// The GitHub-hosted commit archive endpoint. Its only allowed redirect is the exact codeload
/// location returned for the same owner, repository, and full commit SHA.
pub const GITHUB_ARCHIVE_HOST: &str = "github.com";
pub const GITHUB_ARCHIVE_REDIRECT_HOST: &str = "codeload.github.com";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(30);
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// HTTP response fields needed by the strict archive boundary.
pub struct GithubResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub content_lengths: Vec<String>,
    pub location: Option<String>,
    pub body: Box<dyn Read + Send>,
}

impl GithubResponse {
    pub fn new(
        status: u16,
        content_type: Option<&str>,
        content_length: Option<u64>,
        location: Option<&str>,
        body: impl Read + Send + 'static,
    ) -> Self {
        Self {
            status,
            content_type: content_type.map(str::to_owned),
            content_encoding: None,
            content_lengths: content_length
                .map(|length| vec![length.to_string()])
                .unwrap_or_default(),
            location: location.map(str::to_owned),
            body: Box::new(body),
        }
    }
}

/// Injectable request boundary for deterministic offline source-fetch tests.
pub trait GithubTransport: Send + Sync {
    fn get(&self, url: &str) -> Result<GithubResponse, TransportError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TransportError {
    #[error("GitHub source request timed out")]
    Timeout,
    #[error("GitHub source TLS verification or handshake failed")]
    Tls,
    #[error("GitHub source connection failed")]
    Connection,
    #[error("GitHub source response contains an invalid HTTP header")]
    InvalidHeader,
    #[error("GitHub source request failed")]
    RequestFailed,
}

/// Blocking HTTPS transport with certificate verification and automatic redirects disabled.
/// No GitHub authorization or credential header is set for public sources.
pub struct UreqGithubTransport {
    agent: ureq::Agent,
}

impl UreqGithubTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_body(Some(RECEIVE_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqGithubTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubTransport for UreqGithubTransport {
    fn get(&self, url: &str) -> Result<GithubResponse, TransportError> {
        let response = self.agent.get(url).call().map_err(map_ureq_error)?;
        let headers = response.headers();
        let content_type = one_header(headers, "content-type")?;
        let content_encoding = one_header(headers, "content-encoding")?;
        let location = one_header(headers, "location")?;
        let content_lengths = headers
            .get_all("content-length")
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| TransportError::InvalidHeader)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GithubResponse {
            status: response.status().as_u16(),
            content_type,
            content_encoding,
            content_lengths,
            location,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

fn one_header(
    headers: &ureq::http::HeaderMap,
    name: &'static str,
) -> Result<Option<String>, TransportError> {
    let values = headers.get_all(name).iter().collect::<Vec<_>>();
    match values.as_slice() {
        [] => Ok(None),
        [value] => value
            .to_str()
            .map(|value| Some(value.to_owned()))
            .map_err(|_| TransportError::InvalidHeader),
        _ => Err(TransportError::InvalidHeader),
    }
}

fn map_ureq_error(error: ureq::Error) -> TransportError {
    match error {
        ureq::Error::Timeout(_) => TransportError::Timeout,
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => TransportError::Tls,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => TransportError::Connection,
        _ => TransportError::RequestFailed,
    }
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("only catalog skill resources can be fetched")]
    NotSkill,
    #[error("the catalog source provider is not GitHub")]
    UnsupportedProvider,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("GitHub returned an unauthorized or unexpected redirect")]
    UnauthorizedRedirect,
    #[error("GitHub returned HTTP status {status}")]
    HttpStatus { status: u16 },
    #[error("GitHub source response is not a ZIP archive")]
    UnexpectedContentType,
    #[error("GitHub source response uses unsupported content encoding")]
    UnexpectedContentEncoding,
    #[error("GitHub source response has malformed or inconsistent Content-Length metadata")]
    InvalidContentLength,
    #[error("GitHub source archive exceeds the {maximum_bytes} byte download limit")]
    DownloadTooLarge { maximum_bytes: u64 },
    #[error("GitHub source archive transfer was incomplete")]
    IncompleteTransfer,
    #[error("GitHub source archive body could not be read ({kind})")]
    ReadBody { kind: io::ErrorKind },
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error(transparent)]
    Staging(#[from] StageError),
}

/// Fetch a catalog skill at its exact pinned GitHub commit, validate its archive, select its
/// configured directory, and return privately staged inert files.
pub fn fetch_and_stage_skill<T: GithubTransport>(
    resource: &Resource,
    transport: &T,
) -> Result<StagedSkill, FetchError> {
    fetch_and_stage_skill_with_factory(resource, transport, &super::staging::SystemTempDirFactory)
}

/// Variant with an injectable private-tempdir factory for deterministic tests and embedding.
pub fn fetch_and_stage_skill_with_factory<T: GithubTransport>(
    resource: &Resource,
    transport: &T,
    tempdir_factory: &dyn TempDirFactory,
) -> Result<StagedSkill, FetchError> {
    if resource.kind() != ResourceKind::Skill {
        return Err(FetchError::NotSkill);
    }
    let source = resource.source();
    if source.provider() != SourceProvider::Github {
        return Err(FetchError::UnsupportedProvider);
    }

    let primary_url = format!(
        "https://{GITHUB_ARCHIVE_HOST}/{}/{}/archive/{}.zip",
        source.owner().as_str(),
        source.repository().as_str(),
        source.commit().as_str()
    );
    let redirect_url = format!(
        "https://{GITHUB_ARCHIVE_REDIRECT_HOST}/{}/{}/zip/{}",
        source.owner().as_str(),
        source.repository().as_str(),
        source.commit().as_str()
    );

    let response = transport.get(&primary_url)?;
    let response = match response.status {
        200 => response,
        301 | 302 | 303 | 307 | 308 => {
            if response.status != 302 || response.location.as_deref() != Some(redirect_url.as_str())
            {
                return Err(FetchError::UnauthorizedRedirect);
            }
            let redirected = transport.get(&redirect_url)?;
            if matches!(redirected.status, 301 | 302 | 303 | 307 | 308) {
                return Err(FetchError::UnauthorizedRedirect);
            }
            redirected
        }
        status => return Err(FetchError::HttpStatus { status }),
    };

    let bytes = read_archive_response(response)?;
    let manifest: ValidatedArchive = validate_zip_archive(&bytes)?;
    let selection: SelectedSkill = select_skill_directory(resource, &manifest)?;
    stage_selected_skill(resource, &bytes, &manifest, &selection, tempdir_factory)
        .map_err(FetchError::from)
}

fn read_archive_response(mut response: GithubResponse) -> Result<Vec<u8>, FetchError> {
    if response.status != 200 {
        return Err(FetchError::HttpStatus {
            status: response.status,
        });
    }
    if response
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_none_or(|value| !value.trim().eq_ignore_ascii_case("application/zip"))
    {
        return Err(FetchError::UnexpectedContentType);
    }
    if response
        .content_encoding
        .as_deref()
        .is_some_and(|value| !value.trim().eq_ignore_ascii_case("identity"))
    {
        return Err(FetchError::UnexpectedContentEncoding);
    }

    let declared_length = match response.content_lengths.as_slice() {
        [] => None,
        [value] if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => Some(
            value
                .parse::<u64>()
                .map_err(|_| FetchError::InvalidContentLength)?,
        ),
        _ => return Err(FetchError::InvalidContentLength),
    };
    if declared_length.is_some_and(|length| length > MAX_COMPRESSED_ARCHIVE_BYTES) {
        return Err(FetchError::DownloadTooLarge {
            maximum_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
        });
    }

    let capacity = declared_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = response.body.read(&mut buffer).map_err(|error| {
            if declared_length.is_some() || error.kind() == io::ErrorKind::UnexpectedEof {
                FetchError::IncompleteTransfer
            } else {
                FetchError::ReadBody { kind: error.kind() }
            }
        })?;
        if read == 0 {
            break;
        }
        let new_length = bytes
            .len()
            .checked_add(read)
            .ok_or(FetchError::DownloadTooLarge {
                maximum_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
            })?;
        let new_length = u64::try_from(new_length).map_err(|_| FetchError::DownloadTooLarge {
            maximum_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
        })?;
        if new_length > MAX_COMPRESSED_ARCHIVE_BYTES {
            return Err(FetchError::DownloadTooLarge {
                maximum_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
            });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if let Some(length) = declared_length {
        let actual = u64::try_from(bytes.len()).map_err(|_| FetchError::DownloadTooLarge {
            maximum_bytes: MAX_COMPRESSED_ARCHIVE_BYTES,
        })?;
        if length != actual {
            return Err(FetchError::IncompleteTransfer);
        }
    }
    Ok(bytes)
}
