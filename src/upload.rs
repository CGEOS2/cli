//! Bounded object-storage PUT used by media uploads; replaces the former external `curl`.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest tolerated gap while the server is not accepting or answering bytes.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_TOTAL_SECONDS: u64 = 120;

/// RFC 9110 field-name: a non-empty run of `tchar`.
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// A field value may not carry CR, LF, NUL or any other ASCII control byte: a server-chosen
/// header must never be able to end the header block or add a request line of its own.
fn valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b) || b >= 0x80)
}

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
            let Some(value) = value.as_str() else {
                continue;
            };
            // The presign response is the far end's word; validate both parts before they can
            // shape the request. Anything else is refused, not sanitized, and never echoed back.
            if !valid_header_name(key) || !valid_header_value(value) {
                return Err(anyhow!("presign upload headers are invalid"));
            }
            request = request.set(key, value);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    /// One loopback endpoint that records the first request within `deadline`, or `None` when the
    /// client never connects. Non-blocking accept keeps the "nothing was sent" case bounded.
    fn capture(deadline: Duration) -> (String, thread::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let until = Instant::now() + deadline;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut received = Vec::new();
                        let mut buffer = [0_u8; 4096];
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    received.extend_from_slice(&buffer[..read]);
                                    if received.ends_with(b"payload") {
                                        break;
                                    }
                                }
                            }
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return Some(String::from_utf8_lossy(&received).into_owned());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= until {
                            return None;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return None,
                }
            }
        });
        (format!("http://{address}/signed?sig=private"), handle)
    }

    #[test]
    fn required_headers_are_sent_unchanged() {
        let (url, server) = capture(Duration::from_secs(2));
        put_bytes(
            b"payload",
            &json!({"requiredHeaders": {"content-type": "image/png", "x-amz-acl": "private"}}),
            &url,
            10,
        )
        .unwrap();
        let raw = server.join().unwrap().expect("the request must be sent");
        assert!(raw.contains("content-type: image/png"), "{raw}");
        assert!(raw.contains("x-amz-acl: private"), "{raw}");
    }

    #[test]
    fn a_header_that_would_split_the_request_never_reaches_the_wire() {
        for headers in [
            // A CRLF in the name would end the header block and add a request of its own.
            json!({"x-a\r\nX-Injected: 1\r\n\r\nGET /smuggled HTTP/1.1\r\nx-b": "1"}),
            json!({"content-type": "image/png\r\nX-Injected: 1"}),
            json!({"content-type": "image/png\nX-Injected: 1"}),
            json!({"": "image/png"}),
            json!({"content type": "image/png"}),
        ] {
            let (url, server) = capture(Duration::from_millis(400));
            let error = put_bytes(
                b"payload",
                &json!({ "requiredHeaders": headers }),
                &url,
                10,
            )
            .unwrap_err()
            .to_string();
            assert_eq!(error, "presign upload headers are invalid");
            assert!(
                !error.contains("Injected") && !error.contains(&url),
                "{error}"
            );
            assert!(
                server.join().unwrap().is_none(),
                "an invalid presign header must be refused before any byte is sent"
            );
        }
    }

    #[test]
    fn only_https_or_loopback_addresses_are_accepted() {
        let upload = json!({});
        for url in [
            "http://api.example.test/upload",
            "ftp://api.example.test/upload",
            "not a url",
        ] {
            assert!(put_bytes(b"payload", &upload, url, 10).is_err(), "{url}");
        }
    }
}

