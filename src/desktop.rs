//! `cgeos2 client desktop --stdio`: resident UTF-8 JSON Lines bridge for the desktop workbench.
//!
//! stdout carries the protocol only; diagnostics never include credentials, tokens or signed URLs.
//! Credentials stay in this process's memory for the lifetime of one login.
//!
//! Request:  `{"id":"<opaque>","cmd":"product.save","args":{...}}`
//! Response: `{"id":"<opaque>","ok":true,"data":{...}}` or
//!           `{"id":"<opaque>","ok":false,"error":{"code":"CONFLICT","message":"...","outcome_unknown":false}}`
//! A single `{"event":"ready",...}` line is written at start-up.

use super::{read_media_snapshot, resolve_terminal_address, wait_until_ready, MemoryStore};
use crate::upload;
use anyhow::Result;
use cgeos_sdk_shared::login::{AuthEndpoint, LoginClient};
use cgeos_sdk_shared::native::Runtime;
use cgeos_sdk_shared::protocol::{Action, ClientKind, Code, Outcome};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
/// Upper bound for one request line; bounds memory if a peer never sends a newline.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ID_CHARS: usize = 128;
const DEFAULT_PUBLISH_WAIT_SECONDS: u64 = 120;

/// Failure reported to the desktop peer.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopError {
    pub code: String,
    pub message: String,
    /// True when the server may or may not have applied the request; the peer must verify.
    pub outcome_unknown: bool,
    pub detail: Option<Value>,
}

impl DesktopError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            outcome_unknown: false,
            detail: None,
        }
    }
    fn unknown(code: &str, message: impl Into<String>, detail: Option<Value>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            outcome_unknown: true,
            detail,
        }
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new("BAD_REQUEST", message)
    }
    fn to_value(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message,
            "outcome_unknown": self.outcome_unknown});
        if let Some(detail) = &self.detail {
            error["detail"] = detail.clone();
        }
        error
    }
}

type Reply = std::result::Result<Value, DesktopError>;

/// Network side of the bridge; replaced by a scripted double in tests.
pub trait Gateway {
    fn request_code(&mut self, base_url: &str, phone: &str) -> std::result::Result<String, DesktopError>;
    fn login(&mut self, base_url: &str, challenge: &str, code: &str, device: &str) -> Reply;
    fn logout(&mut self);
    fn authenticated(&self) -> bool;
    fn call(&mut self, action: &str, enterprise: Option<Uuid>, payload: Value) -> Reply;
    fn put_object(&mut self, bytes: &[u8], upload: &Value, url: &str) -> std::result::Result<(), DesktopError>;
}

/// Runs the resident loop on real stdin/stdout until EOF or `session.quit`.
pub fn run_stdio() -> Result<()> {
    let closing = Arc::new(AtomicBool::new(false));
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        std::io::BufReader::new(stdin),
        stdout.lock(),
        &mut LiveGateway::default(),
        closing,
    )
}

enum Item {
    Line(Vec<u8>),
    Oversize,
}

fn read_items(mut reader: impl BufRead, sender: mpsc::Sender<Item>, closing: Arc<AtomicBool>) {
    loop {
        let mut buffer = Vec::new();
        let read = match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buffer)
        {
            Ok(read) => read,
            Err(_) => 0,
        };
        if read == 0 {
            // EOF: queued requests still finish; a vanished peer is noticed by the next write.
            return;
        }
        if buffer.len() > MAX_LINE_BYTES && buffer.last() != Some(&b'\n') {
            // Discard the remainder of the oversized line without keeping it.
            loop {
                let mut sink = Vec::new();
                match reader.by_ref().take(64 * 1024).read_until(b'\n', &mut sink) {
                    Ok(0) | Err(_) => {
                        let _ = sender.send(Item::Oversize);
                        return;
                    }
                    Ok(_) if sink.last() == Some(&b'\n') => break,
                    Ok(_) => {}
                }
            }
            if sender.send(Item::Oversize).is_err() {
                return;
            }
            continue;
        }
        if is_quit(&buffer) {
            closing.store(true, Ordering::SeqCst);
        }
        if sender.send(Item::Line(buffer)).is_err() {
            return;
        }
    }
}

fn is_quit(line: &[u8]) -> bool {
    serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|value| value.get("cmd").and_then(Value::as_str).map(|cmd| cmd == "session.quit"))
        .unwrap_or(false)
}

/// Serve requests sequentially; one reader thread splits lines so long operations stay cancellable.
pub fn serve<R, W>(
    reader: R,
    mut writer: W,
    gateway: &mut dyn Gateway,
    closing: Arc<AtomicBool>,
) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    write_line(
        &mut writer,
        &json!({"event": "ready", "protocol": PROTOCOL_VERSION, "version": env!("CARGO_PKG_VERSION")}),
    )?;
    let (sender, receiver) = mpsc::channel();
    let reader_closing = closing.clone();
    let handle = std::thread::spawn(move || read_items(reader, sender, reader_closing));
    let mut desktop = Desktop { gateway, closing, quit: false };
    while let Ok(item) = receiver.recv() {
        let response = match item {
            Item::Oversize => failure(Value::Null, DesktopError::bad("request line is too long")),
            Item::Line(bytes) => desktop.handle_line(&bytes),
        };
        write_line(&mut writer, &response)?;
        if desktop.quit {
            break;
        }
    }
    desktop.gateway.logout();
    drop(receiver);
    // The reader thread may be blocked on stdin; process exit reaps it.
    if handle.is_finished() {
        let _ = handle.join();
    }
    Ok(())
}

fn write_line(writer: &mut impl Write, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

fn success(id: Value, data: Value) -> Value {
    json!({"id": id, "ok": true, "data": data})
}

fn failure(id: Value, error: DesktopError) -> Value {
    json!({"id": id, "ok": false, "error": error.to_value()})
}

struct Desktop<'a> {
    gateway: &'a mut dyn Gateway,
    closing: Arc<AtomicBool>,
    quit: bool,
}

impl<'a> Desktop<'a> {
    fn handle_line(&mut self, bytes: &[u8]) -> Value {
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text.trim(),
            Err(_) => return failure(Value::Null, DesktopError::bad("request is not valid UTF-8")),
        };
        if text.is_empty() {
            return failure(Value::Null, DesktopError::bad("empty request line"));
        }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return failure(Value::Null, DesktopError::bad("request is not valid JSON")),
        };
        let id = match parsed.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.chars().count() <= MAX_ID_CHARS => id.to_owned(),
            _ => return failure(Value::Null, DesktopError::bad("request id is required")),
        };
        let id_value = json!(id);
        let Some(command) = parsed.get("cmd").and_then(Value::as_str) else {
            return failure(id_value, DesktopError::bad("cmd is required"));
        };
        let args = match parsed.get("args") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return failure(id_value, DesktopError::bad("args must be an object")),
        };
        match self.dispatch(command, &args) {
            Ok(data) => success(id_value, data),
            Err(error) => failure(id_value, error),
        }
    }

    fn dispatch(&mut self, command: &str, args: &Map<String, Value>) -> Reply {
        match command {
            "session.ping" => Ok(json!({"protocol": PROTOCOL_VERSION, "authenticated": self.gateway.authenticated()})),
            "session.quit" => {
                self.quit = true;
                Ok(json!({"closing": true}))
            }
            "auth.challenge" => {
                let challenge = self
                    .gateway
                    .request_code(&string(args, "base_url")?, &string(args, "phone")?)?;
                Ok(json!({"challenge": challenge}))
            }
            "auth.login" => {
                let base_url = string(args, "base_url")?;
                let code = string(args, "code")?;
                let device = optional_string(args, "device")?.unwrap_or_else(|| "cgeos2-workbench".to_owned());
                let challenge = match optional_string(args, "challenge")? {
                    Some(value) => value,
                    None => self.gateway.request_code(&base_url, &string(args, "phone")?)?,
                };
                self.gateway.login(&base_url, &challenge, &code, &device)
            }
            "auth.logout" => {
                self.gateway.logout();
                Ok(json!({"authenticated": false}))
            }
            "auth.status" => Ok(json!({"authenticated": self.gateway.authenticated()})),
            _ => {
                if !self.gateway.authenticated() {
                    return Err(DesktopError::new("NOT_AUTHENTICATED", "login is required"));
                }
                self.authorized(command, args)
            }
        }
    }

    fn authorized(&mut self, command: &str, args: &Map<String, Value>) -> Reply {
        match command {
            "enterprise.list" => {
                let mut payload = Map::new();
                if let Some(query) = optional_string(args, "query")? {
                    payload.insert("query".into(), json!(query));
                }
                payload.insert("offset".into(), json!(args.get("offset").and_then(Value::as_u64).unwrap_or(0)));
                payload.insert("limit".into(), json!(args.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 100)));
                self.gateway.call("Platform.Enterprise.Index.List", None, Value::Object(payload))
            }
            "site.list" => {
                let enterprise = uuid(args, "enterprise")?;
                self.gateway.call("Client.Tenant.Sites.List", Some(enterprise), json!({}))
            }
            "category.list" => {
                let (enterprise, site) = (uuid(args, "enterprise")?, uuid(args, "site")?);
                self.gateway.call("Client.NeoCMS.Category.Query", Some(enterprise), json!({"site_id": site}))
            }
            "product.list" => {
                let (enterprise, site) = (uuid(args, "enterprise")?, uuid(args, "site")?);
                self.gateway.call("Client.NeoCMS.Product.Query", Some(enterprise), json!({"site_id": site}))
            }
            "product.get" => {
                let (enterprise, site, product) =
                    (uuid(args, "enterprise")?, uuid(args, "site")?, uuid(args, "product")?);
                self.gateway.call(
                    "Client.NeoCMS.Product.Get",
                    Some(enterprise),
                    json!({"site_id": site, "product_id": product}),
                )
            }
            "product.save" => self.product_save(args),
            "product.publish" => self.product_publish(args),
            "product.publish_status" => {
                let (enterprise, site, product, task) = (
                    uuid(args, "enterprise")?,
                    uuid(args, "site")?,
                    uuid(args, "product")?,
                    uuid(args, "task")?,
                );
                self.gateway.call(
                    "Client.NeoCMS.Product.PublishStatus",
                    Some(enterprise),
                    json!({"site_id": site, "product_id": product, "task_id": task}),
                )
            }
            "media.list" => {
                let (enterprise, site) = (uuid(args, "enterprise")?, uuid(args, "site")?);
                self.gateway.call("Client.NeoCMS.Media.Query", Some(enterprise), json!({"site_id": site}))
            }
            "media.upload" => self.media_upload(args),
            other => Err(DesktopError::new("UNKNOWN_COMMAND", format!("unknown command {other}"))),
        }
    }

    fn product_save(&mut self, args: &Map<String, Value>) -> Reply {
        let (enterprise, site, product, operation) = (
            uuid(args, "enterprise")?,
            uuid(args, "site")?,
            uuid(args, "product")?,
            uuid(args, "operation_id")?,
        );
        let base = args
            .get("base_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| DesktopError::bad("base_revision must be a non-negative integer"))?;
        let body = args
            .get("body")
            .filter(|value| value.is_object())
            .ok_or_else(|| DesktopError::bad("body must be an object"))?;
        let reply = self.gateway.call(
            "Client.NeoCMS.Product.Save",
            Some(enterprise),
            json!({"site_id": site, "product_id": product, "operation_id": operation,
                "base_revision": base, "body": body}),
        )?;
        if reply.get("version") != Some(&json!(1)) || reply.get("status") != Some(&json!("SAVED")) {
            return Err(DesktopError::unknown(
                "BAD_RESPONSE",
                "save response was not a confirmed SAVED result; verify with product.get",
                None,
            ));
        }
        Ok(reply)
    }

    fn product_publish(&mut self, args: &Map<String, Value>) -> Reply {
        let (enterprise, site, product, operation) = (
            uuid(args, "enterprise")?,
            uuid(args, "site")?,
            uuid(args, "product")?,
            uuid(args, "operation_id")?,
        );
        let revision = args
            .get("expected_revision")
            .and_then(Value::as_u64)
            .filter(|value| *value >= 1)
            .ok_or_else(|| DesktopError::bad("expected_revision must be a positive integer"))?;
        let wait = args
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_PUBLISH_WAIT_SECONDS)
            .clamp(1, 3600);
        let mut value = self.gateway.call(
            "Client.NeoCMS.Product.Publish",
            Some(enterprise),
            json!({"site_id": site, "product_id": product, "expected_revision": revision,
                "operation_id": operation}),
        )?;
        let task = value
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|text| Uuid::parse_str(text).ok())
            .ok_or_else(|| DesktopError::unknown("BAD_RESPONSE", "publish receipt has no task_id", None))?;
        let detail = json!({"task_id": task, "operation_id": operation});
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            match value.get("status").and_then(Value::as_str) {
                Some("COMMITTED") => return Ok(value),
                Some("FAILED") => {
                    return Err(DesktopError {
                        code: "PUBLISH_FAILED".into(),
                        message: "publication failed".into(),
                        outcome_unknown: false,
                        detail: Some(detail),
                    })
                }
                Some("DELIVERING" | "ACTIVATING" | "PENDING_ACK") => {
                    if self.closing.load(Ordering::SeqCst) {
                        return Err(DesktopError::unknown("CANCELLED", "bridge is closing", Some(detail)));
                    }
                    if Instant::now() >= deadline {
                        return Err(DesktopError::unknown(
                            "PUBLISH_PENDING",
                            "publication did not finish in time; query product.publish_status",
                            Some(detail),
                        ));
                    }
                    std::thread::sleep(poll_interval());
                    value = self.gateway.call(
                        "Client.NeoCMS.Product.PublishStatus",
                        Some(enterprise),
                        json!({"site_id": site, "product_id": product, "task_id": task}),
                    )?;
                }
                _ => {
                    return Err(DesktopError::unknown(
                        "BAD_RESPONSE",
                        "unrecognised publication status",
                        Some(detail),
                    ))
                }
            }
        }
    }

    fn media_upload(&mut self, args: &Map<String, Value>) -> Reply {
        let (enterprise, site) = (uuid(args, "enterprise")?, uuid(args, "site")?);
        let path = std::path::PathBuf::from(string(args, "file")?);
        let bytes = read_media_snapshot(&path)
            .map_err(|error| DesktopError::new("FILE_UNREADABLE", error.to_string()))?;
        let content_type = sniff_image(&bytes)
            .ok_or_else(|| DesktopError::new("UNSUPPORTED_IMAGE", "only JPEG, PNG and WebP images can be uploaded"))?;
        if let Some(declared) = optional_string(args, "content_type")? {
            if declared != content_type {
                return Err(DesktopError::new(
                    "UNSUPPORTED_IMAGE",
                    "declared content type does not match the image bytes",
                ));
            }
        }
        let filename = optional_string(args, "filename")?.unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "upload".to_owned())
        });
        let presigned = self.gateway.call(
            "Client.NeoCMS.Media.Presign",
            Some(enterprise),
            json!({"site_id": site, "filename": filename, "content_type": content_type,
                "size_bytes": bytes.len()}),
        )?;
        let upload_info = presigned
            .get("upload")
            .filter(|value| value.is_object())
            .ok_or_else(|| DesktopError::new("BAD_RESPONSE", "presign response has no upload"))?
            .clone();
        let resource = pick(&upload_info, "resourceId", "resource_id")
            .ok_or_else(|| DesktopError::new("BAD_RESPONSE", "presign response has no resource id"))?;
        let upload_url = pick(&upload_info, "uploadUrl", "upload_url")
            .ok_or_else(|| DesktopError::new("BAD_RESPONSE", "presign response has no upload url"))?;
        self.gateway.put_object(&bytes, &upload_info, &upload_url)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let detail = json!({"resource_id": resource, "sha256": digest});
        let confirmed = self
            .gateway
            .call(
                "Client.NeoCMS.Media.Confirm",
                Some(enterprise),
                json!({"site_id": site, "resource_id": resource, "sha256_digest": digest,
                    "actual_size_bytes": bytes.len()}),
            )
            .map_err(|mut error| {
                // The object is uploaded; whether the platform recorded it is only known by querying.
                error.outcome_unknown = true;
                error.detail = Some(detail.clone());
                error
            })?;
        let media = confirmed.get("media").cloned().unwrap_or(Value::Null);
        let matches = media.get("id").and_then(Value::as_str) == Some(resource.as_str())
            && media.get("sha256").and_then(Value::as_str) == Some(digest.as_str());
        if !matches {
            return Err(DesktopError::unknown(
                "BAD_RESPONSE",
                "media confirmation did not match the uploaded bytes",
                Some(detail),
            ));
        }
        Ok(json!({"media": media, "size_bytes": bytes.len(), "content_type": content_type}))
    }
}

fn poll_interval() -> Duration {
    if cfg!(test) {
        Duration::from_millis(5)
    } else {
        Duration::from_millis(500)
    }
}

fn pick(value: &Value, camel: &str, snake: &str) -> Option<String> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Identify the three accepted image formats from their magic bytes.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn string(args: &Map<String, Value>, key: &str) -> std::result::Result<String, DesktopError> {
    optional_string(args, key)?.ok_or_else(|| DesktopError::bad(format!("{key} is required")))
}

fn optional_string(args: &Map<String, Value>, key: &str) -> std::result::Result<Option<String>, DesktopError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(DesktopError::bad(format!("{key} must be a non-empty string"))),
    }
}

fn uuid(args: &Map<String, Value>, key: &str) -> std::result::Result<Uuid, DesktopError> {
    Uuid::parse_str(&string(args, key)?).map_err(|_| DesktopError::bad(format!("{key} must be a UUID")))
}

/// Production gateway: one login, one Terminal connection, credentials only in memory.
#[derive(Default)]
struct LiveGateway {
    runtime: Option<Runtime>,
}

fn code_name(code: Code) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "UNAVAILABLE".to_owned())
}

impl Gateway for LiveGateway {
    fn request_code(&mut self, base_url: &str, phone: &str) -> std::result::Result<String, DesktopError> {
        let client = login_client(base_url)?;
        client
            .request_code(phone)
            .map_err(|code| DesktopError::new("LOGIN_FAILED", format!("request code failed: {}", code_name_login(&code))))
    }

    fn login(&mut self, base_url: &str, challenge: &str, code: &str, device: &str) -> Reply {
        self.logout();
        let endpoint = AuthEndpoint::new(base_url)
            .map_err(|_| DesktopError::new("LOGIN_FAILED", "invalid auth endpoint"))?;
        let client = LoginClient::new(endpoint.clone())
            .map_err(|_| DesktopError::new("LOGIN_FAILED", "cannot create login client"))?;
        let store = Arc::new(MemoryStore::default());
        let credentials = client
            .login(challenge, code, device, ClientKind::Web, store.as_ref())
            .map_err(|code| DesktopError::new("LOGIN_FAILED", format!("login failed: {}", code_name_login(&code))))?;
        let addresses = resolve_terminal_address(base_url)
            .map_err(|_| DesktopError::new("UNAVAILABLE", "cannot resolve API host"))?;
        let terminal = endpoint
            .terminal_endpoint(addresses)
            .map_err(|_| DesktopError::new("LOGIN_FAILED", "invalid terminal endpoint"))?;
        let info = json!({"account": credentials.account, "session": credentials.session,
            "device_id": credentials.device_id, "authenticated": true});
        let runtime = Runtime::start(terminal, credentials, store)
            .map_err(|_| DesktopError::new("UNAVAILABLE", "cannot start terminal runtime"))?;
        wait_until_ready(&runtime).map_err(|error| DesktopError::new("UNAVAILABLE", error.to_string()))?;
        self.runtime = Some(runtime);
        Ok(info)
    }

    fn logout(&mut self) {
        if let Some(mut runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }

    fn authenticated(&self) -> bool {
        self.runtime.is_some()
    }

    fn call(&mut self, action: &str, enterprise: Option<Uuid>, payload: Value) -> Reply {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| DesktopError::new("NOT_AUTHENTICATED", "login is required"))?;
        let action = Action::try_from(action.to_owned())
            .map_err(|_| DesktopError::new("UNKNOWN_ACTION", "action is not registered"))?;
        let pending = runtime
            .handle()
            .begin(action, enterprise, payload)
            .map_err(|code| DesktopError::new(&code_name(code), "cannot submit request"))?;
        match pending.wait() {
            Outcome::Ok { data } => Ok(data),
            Outcome::Error { code } => {
                let unknown = matches!(code, Code::OutcomeUnknown | Code::Unavailable);
                let mut error = DesktopError::new(&code_name(code), format!("request failed: {}", code_name(code)));
                error.outcome_unknown = unknown;
                Err(error)
            }
        }
    }

    fn put_object(&mut self, bytes: &[u8], upload: &Value, url: &str) -> std::result::Result<(), DesktopError> {
        upload::put_bytes(bytes, upload, url, upload::DEFAULT_TOTAL_SECONDS)
            .map_err(|error| DesktopError::new("UPLOAD_FAILED", error.to_string()))
    }
}

fn code_name_login(code: &impl std::fmt::Debug) -> String {
    format!("{code:?}")
}

fn login_client(base_url: &str) -> std::result::Result<LoginClient, DesktopError> {
    let endpoint = AuthEndpoint::new(base_url)
        .map_err(|_| DesktopError::new("LOGIN_FAILED", "invalid auth endpoint"))?;
    LoginClient::new(endpoint).map_err(|_| DesktopError::new("LOGIN_FAILED", "cannot create login client"))
}

#[cfg(test)]
mod tests;
