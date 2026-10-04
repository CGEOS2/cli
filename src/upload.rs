//! Bounded object-storage PUT used by media uploads; replaces the former external `curl`.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest tolerated gap while the server is not accepting or answering bytes.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_TOTAL_SECONDS: u64 = 120;

/// PUT `bytes` to a presigned URL with the headers the presign response required.
///
/// Errors never include the URL, headers or provider body because the URL carries a signature.
pub fn put_bytes(bytes: &[u8], upload: &Value, upload_url: &str, total_seconds: u64) -> Result<()> {
    let parsed = url::Url::parse(upload_url).map_err(|_| anyhow!("upload address is invalid"))?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(anyhow!("upload address must use https"));
    }
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(STALL_TIMEOUT)
        .timeout_write(STALL_TIMEOUT)
        .timeout(Duration::from_secs(total_seconds.max(1)))
        .build();
    let mut request = agent.put(upload_url);
    if let Some(headers) = upload
        .get("requiredHeaders")
        .or_else(|| upload.get("required_headers"))
        .and_then(Value::as_object)
    {
        for (key, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.set(key, value);
            }
        }
    }
    match request.send_bytes(bytes) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, _)) => {
            Err(anyhow!("media upload rejected with HTTP status {status}"))
        }
        Err(ureq::Error::Transport(transport)) => Err(anyhow!(
            "media upload failed or exceeded its deadline ({:?})",
            transport.kind()
        )),
    }
}
