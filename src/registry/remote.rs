use std::{io::Read, time::Duration};

use thiserror::Error;

/// The only endpoint THRN may use for runtime catalog refreshes.
pub const TRUSTED_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BODY_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Streaming HTTP response supplied to the catalog validation service.
pub struct HttpResponse {
    pub status: u16,
    pub content_length: Option<u64>,
    pub body: Box<dyn Read + Send>,
}

impl HttpResponse {
    pub fn new(status: u16, content_length: Option<u64>, body: impl Read + Send + 'static) -> Self {
        Self {
            status,
            content_length,
            body: Box::new(body),
        }
    }
}

/// Injectable transport for an explicitly requested catalog refresh.
pub trait CatalogTransport: Send + Sync {
    fn fetch_catalog(&self) -> Result<HttpResponse, TransportError>;
}

/// Failure classes exposed by the HTTPS client without leaking proxy details or credentials.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TransportError {
    #[error("HTTPS catalog request timed out; retry the refresh")]
    Timeout,
    #[error("HTTPS catalog TLS verification or handshake failed")]
    Tls,
    #[error("HTTPS catalog connection failed; check connectivity and retry")]
    Connection,
    #[error("HTTPS catalog response had an invalid Content-Length header")]
    InvalidContentLength,
    #[error("HTTPS catalog request failed; check connectivity and retry")]
    RequestFailed,
}

/// ureq-backed transport with certificate verification and no redirect following.
pub struct UreqCatalogTransport {
    agent: ureq::Agent,
}

impl UreqCatalogTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqCatalogTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogTransport for UreqCatalogTransport {
    fn fetch_catalog(&self) -> Result<HttpResponse, TransportError> {
        let response = self
            .agent
            .get(TRUSTED_CATALOG_URL)
            .header("Accept", "application/json")
            .call()
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => TransportError::Timeout,
                ureq::Error::Tls(_) | ureq::Error::Rustls(_) => TransportError::Tls,
                ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
                    TransportError::Connection
                }
                _ => TransportError::RequestFailed,
            })?;
        let status = response.status().as_u16();
        let content_length = match response.headers().get("content-length") {
            None => None,
            Some(value) => Some(
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or(TransportError::InvalidContentLength)?,
            ),
        };
        let body = response.into_body().into_reader();
        Ok(HttpResponse {
            status,
            content_length,
            body: Box::new(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{CatalogTransport, TransportError, UreqCatalogTransport};

    #[test]
    fn transport_is_bound_to_the_fixed_https_endpoint() {
        let transport = UreqCatalogTransport::new();
        let _transport_contract: &dyn CatalogTransport = &transport;
        assert!(super::TRUSTED_CATALOG_URL.starts_with("https://"));
        assert!(super::TRUSTED_CATALOG_URL.contains("raw.githubusercontent.com/thrns/thrn/"));
        let response = super::HttpResponse::new(200, Some(3), Cursor::new(b"abc"));
        assert_eq!(response.status, 200);
        assert_eq!(response.content_length, Some(3));
    }

    #[test]
    fn transport_error_messages_do_not_contain_urls_or_environment_data() {
        let messages = [
            TransportError::Timeout.to_string(),
            TransportError::Tls.to_string(),
            TransportError::Connection.to_string(),
            TransportError::InvalidContentLength.to_string(),
        ];
        assert!(messages.iter().all(|message| !message.contains("https://")));
    }
}
