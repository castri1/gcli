//! One shared HTTP agent (keep-alive connection pool, proxy from the environment, rustls) for
//! every call: Google APIs, the OAuth token endpoint and GitHub (updates).

use std::sync::OnceLock;
use std::time::Duration;

use ureq::Agent;

use super::error::{GcpError, Result};

pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .user_agent(concat!("gcli/", env!("CARGO_PKG_VERSION")))
            .build();
        Agent::new_with_config(config)
    })
}

/// Sends a request and reads the whole response, whatever its status. Transport failures
/// (DNS, refused or reset connections, timeouts) are `GcpError::Network`.
pub fn send(method: &str, url: &str, headers: &[(String, String)], body: Vec<u8>) -> Result<RawResponse> {
    let mut builder = ureq::http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder.body(body).map_err(|e| GcpError::Network(e.to_string()))?;
    let mut response = agent().run(request).map_err(|e| GcpError::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| GcpError::Network(e.to_string()))?;
    Ok(RawResponse { status, body })
}
