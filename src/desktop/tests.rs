use super::*;
use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Cursor};

const ENTERPRISE: &str = "00000000-0000-0000-0000-0000000000e1";
const SITE: &str = "00000000-0000-0000-0000-0000000000a1";
const PRODUCT: &str = "00000000-0000-0000-0000-0000000000b1";
const OPERATION: &str = "00000000-0000-0000-0000-0000000000c1";
const TASK: &str = "00000000-0000-0000-0000-0000000000d1";

#[derive(Default)]
struct Script {
    authenticated: bool,
    logins: usize,
    calls: Vec<(String, Option<Uuid>, Value)>,
    replies: HashMap<String, VecDeque<Reply>>,
    puts: Vec<usize>,
    put_failure: Option<DesktopError>,
}

impl Script {
    fn reply(&mut self, action: &str, reply: Reply) -> &mut Self {
        self.replies.entry(action.to_owned()).or_default().push_back(reply);
        self
    }
    fn count(&self, action: &str) -> usize {
        self.calls.iter().filter(|call| call.0 == action).count()
    }
}

impl Gateway for Script {
    fn request_code(&mut self, _: &str, _: &str) -> std::result::Result<String, DesktopError> {
        Ok("00000000-0000-0000-0000-00000000cafe".to_owned())
    }
    fn login(&mut self, _: &str, _: &str, _: &str, _: &str) -> Reply {
        self.authenticated = true;
        self.logins += 1;
        Ok(json!({"account": "acct", "authenticated": true}))
    }
    fn logout(&mut self) {
        self.authenticated = false;
    }
    fn authenticated(&self) -> bool {
        self.authenticated
    }
    fn call(&mut self, action: &str, enterprise: Option<Uuid>, payload: Value) -> Reply {
        self.calls.push((action.to_owned(), enterprise, payload));
        self.replies
            .get_mut(action)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Ok(json!({"version": 1, "items": []})))
    }
    fn put_object(&mut self, bytes: &[u8], _: &Value, _: &str) -> std::result::Result<(), DesktopError> {
        self.puts.push(bytes.len());
        match self.put_failure.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Delivers input a few bytes at a time to prove lines are reassembled across reads.
struct Trickle {
    inner: Cursor<Vec<u8>>,
    step: usize,
}

impl Read for Trickle {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let limit = buffer.len().min(self.step);
        self.inner.read(&mut buffer[..limit])
    }
}

fn request(id: &str, cmd: &str, args: Value) -> String {
    format!("{}\n", json!({"id": id, "cmd": cmd, "args": args}))
}

fn run(script: &mut Script, input: Vec<u8>, step: usize) -> Vec<Value> {
    let reader = BufReader::with_capacity(7, Trickle { inner: Cursor::new(input), step });
    let mut output = Vec::new();
    serve(reader, &mut output, script, Arc::new(AtomicBool::new(false))).unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn login_line() -> String {
    request("login", "auth.login", json!({"base_url": "https://api.example.test", "phone": "+15555550100", "code": "123456"}))
}

fn png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"fake-body-bytes");
    bytes
}

fn upload_args(file: &std::path::Path) -> Value {
    json!({"enterprise": ENTERPRISE, "site": SITE, "file": file.to_string_lossy()})
}

fn presign_reply() -> Reply {
    Ok(json!({"version": 1, "upload": {"resourceId": "00000000-0000-0000-0000-0000000000f1",
        "uploadUrl": "https://objects.example.test/put?sig=secret", "httpMethod": "PUT",
        "requiredHeaders": {"content-type": "image/png"}}}))
}

#[test]
fn announces_ready_and_one_login_serves_many_calls_on_one_connection() {
    let mut script = Script::default();
    let input = [
        login_line(),
        request("1", "enterprise.list", json!({})),
        request("2", "site.list", json!({"enterprise": ENTERPRISE})),
        request("3", "product.list", json!({"enterprise": ENTERPRISE, "site": SITE})),
        request("4", "category.list", json!({"enterprise": ENTERPRISE, "site": SITE})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[0]["event"], "ready");
    assert_eq!(out.len(), 6);
    assert!(out[1..].iter().all(|line| line["ok"] == true));
    assert_eq!(script.logins, 1);
    assert_eq!(script.calls.len(), 4);
    assert!(!out.iter().any(|line| line.to_string().contains("123456")));
}

#[test]
fn json_lines_are_reassembled_from_tiny_reads_with_chinese_text() {
    let mut script = Script::default();
    let input = [login_line(), request("中文-1", "site.list", json!({"enterprise": ENTERPRISE}))].concat();
    let out = run(&mut script, input.into_bytes(), 3);
    assert_eq!(out[2]["id"], "中文-1");
    assert_eq!(out[2]["ok"], true);
}

#[test]
fn commands_before_login_are_refused_and_bad_lines_do_not_stop_the_loop() {
    let mut script = Script::default();
    let mut input = b"not json\n\n".to_vec();
    input.extend_from_slice(&[0xff, 0xfe, b'\n']);
    input.extend_from_slice(b"{\"cmd\":\"site.list\"}\n");
    input.extend_from_slice(request("a", "site.list", json!({"enterprise": ENTERPRISE})).as_bytes());
    input.extend_from_slice(request("b", "nope", json!({})).as_bytes());
    input.extend_from_slice(request("c", "session.ping", json!({})).as_bytes());
    let out = run(&mut script, input, 5);
    assert_eq!(out[1]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[2]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[3]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[4]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[5]["error"]["code"], "NOT_AUTHENTICATED");
    assert_eq!(out[6]["error"]["code"], "NOT_AUTHENTICATED");
    assert_eq!(out[7]["ok"], true);
    assert!(script.calls.is_empty());
}

#[test]
fn oversized_line_is_rejected_and_the_next_request_still_works() {
    let mut script = Script::default();
    let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
    input.push(b'\n');
    input.extend_from_slice(request("after", "session.ping", json!({})).as_bytes());
    let out = run(&mut script, input, 1 << 20);
    assert_eq!(out[1]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[2]["id"], "after");
    assert_eq!(out[2]["ok"], true);
}

#[test]
fn permission_denial_and_revision_conflict_are_reported_with_stable_codes() {
    let mut script = Script::default();
    script
        .reply("Client.NeoCMS.Product.Query", Err(DesktopError::new("FORBIDDEN", "denied")))
        .reply("Client.NeoCMS.Product.Save", Err(DesktopError::new("CONFLICT", "revision")));
    let input = [
        login_line(),
        request("list", "product.list", json!({"enterprise": ENTERPRISE, "site": SITE})),
        request("save", "product.save", json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
            "operation_id": OPERATION, "base_revision": 3, "body": {"title": "T"}})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["error"]["code"], "FORBIDDEN");
    assert_eq!(out[2]["error"]["outcome_unknown"], false);
    assert_eq!(out[3]["error"]["code"], "CONFLICT");
    assert_eq!(out[3]["error"]["outcome_unknown"], false);
    let payload = &script.calls[1].2;
    assert_eq!(payload["base_revision"], 3);
    assert_eq!(payload["operation_id"], OPERATION);
}

#[test]
fn save_requires_a_confirmed_saved_result() {
    let mut script = Script::default();
    script.reply("Client.NeoCMS.Product.Save", Ok(json!({"version": 1, "status": "PENDING"})));
    let input = [
        login_line(),
        request("save", "product.save", json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
            "operation_id": OPERATION, "base_revision": 0, "body": {}})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["ok"], false);
    assert_eq!(out[2]["error"]["outcome_unknown"], true);
}

#[test]
fn upload_handles_chinese_and_spaced_paths_and_confirms_the_same_bytes() {
    let directory = std::env::temp_dir().join(format!("cgeos 上传 {}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("正面 图 1.png");
    std::fs::write(&file, png()).unwrap();
    let digest = format!("{:x}", Sha256::digest(png()));
    let mut script = Script::default();
    script
        .reply("Client.NeoCMS.Media.Presign", presign_reply())
        .reply(
            "Client.NeoCMS.Media.Confirm",
            Ok(json!({"version": 1, "media": {"id": "00000000-0000-0000-0000-0000000000f1",
                "publicUrl": "https://cdn.example.test/a.png", "contentType": "image/png", "sha256": digest}})),
        );
    let input = [login_line(), request("up", "media.upload", upload_args(&file))].concat();
    let out = run(&mut script, input.into_bytes(), 9);
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(out[2]["ok"], true, "{}", out[2]);
    assert_eq!(script.puts, vec![png().len()]);
    let presign = &script.calls[0].2;
    assert_eq!(presign["filename"], "正面 图 1.png");
    assert_eq!(presign["content_type"], "image/png");
    assert_eq!(script.calls[1].2["sha256_digest"], digest.as_str());
    assert!(!out[2].to_string().contains("secret"));
}

#[test]
fn failed_put_never_confirms_and_unknown_confirm_is_flagged() {
    let directory = std::env::temp_dir().join(format!("cgeos-up-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("a.png");
    std::fs::write(&file, png()).unwrap();

    let mut failing = Script::default();
    failing.reply("Client.NeoCMS.Media.Presign", presign_reply());
    failing.put_failure = Some(DesktopError::new("UPLOAD_FAILED", "media upload rejected with HTTP status 403"));
    let input = [login_line(), request("up", "media.upload", upload_args(&file))].concat();
    let out = run(&mut failing, input.clone().into_bytes(), 4096);
    assert_eq!(out[2]["error"]["code"], "UPLOAD_FAILED");
    assert_eq!(failing.count("Client.NeoCMS.Media.Confirm"), 0);

    let mut unknown = Script::default();
    unknown.reply("Client.NeoCMS.Media.Presign", presign_reply());
    unknown.reply("Client.NeoCMS.Media.Confirm", Err(DesktopError::new("UNAVAILABLE", "lost")));
    let out = run(&mut unknown, input.into_bytes(), 4096);
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(out[2]["error"]["outcome_unknown"], true);
    assert_eq!(out[2]["error"]["detail"]["resource_id"], "00000000-0000-0000-0000-0000000000f1");
}

#[test]
fn non_images_and_mismatched_types_are_refused_before_any_request() {
    let directory = std::env::temp_dir().join(format!("cgeos-bad-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let text = directory.join("a.png");
    std::fs::write(&text, b"not an image").unwrap();
    let image = directory.join("b.png");
    std::fs::write(&image, png()).unwrap();
    let mut script = Script::default();
    let mut mismatched = upload_args(&image);
    mismatched["content_type"] = json!("image/jpeg");
    let input = [
        login_line(),
        request("a", "media.upload", upload_args(&text)),
        request("b", "media.upload", mismatched),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(out[2]["error"]["code"], "UNSUPPORTED_IMAGE");
    assert_eq!(out[3]["error"]["code"], "UNSUPPORTED_IMAGE");
    assert!(script.calls.is_empty());
}

fn publish_line(wait: u64) -> String {
    request("pub", "product.publish", json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
        "operation_id": OPERATION, "expected_revision": 4, "wait_seconds": wait}))
}

fn receipt(status: &str) -> Reply {
    Ok(json!({"status": status, "task_id": TASK}))
}

#[test]
fn publish_polls_until_committed_and_reports_the_real_result() {
    let mut script = Script::default();
    script
        .reply("Client.NeoCMS.Product.Publish", receipt("PENDING_ACK"))
        .reply("Client.NeoCMS.Product.PublishStatus", receipt("DELIVERING"))
        .reply("Client.NeoCMS.Product.PublishStatus", receipt("COMMITTED"));
    let input = [login_line(), publish_line(30)].concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["ok"], true);
    assert_eq!(out[2]["data"]["status"], "COMMITTED");
    assert_eq!(script.count("Client.NeoCMS.Product.PublishStatus"), 2);
}

#[test]
fn failed_publication_is_an_error_and_a_timeout_is_unknown_not_success() {
    let mut failed = Script::default();
    failed
        .reply("Client.NeoCMS.Product.Publish", receipt("PENDING_ACK"))
        .reply("Client.NeoCMS.Product.PublishStatus", receipt("FAILED"));
    let input = [login_line(), publish_line(30)].concat();
    let out = run(&mut failed, input.into_bytes(), 4096);
    assert_eq!(out[2]["error"]["code"], "PUBLISH_FAILED");
    assert_eq!(out[2]["error"]["outcome_unknown"], false);

    let mut slow = Script::default();
    slow.reply("Client.NeoCMS.Product.Publish", receipt("PENDING_ACK"));
    for _ in 0..1000 {
        slow.reply("Client.NeoCMS.Product.PublishStatus", receipt("ACTIVATING"));
    }
    let input = [login_line(), publish_line(1)].concat();
    let out = run(&mut slow, input.into_bytes(), 4096);
    assert_eq!(out[2]["error"]["code"], "PUBLISH_PENDING");
    assert_eq!(out[2]["error"]["outcome_unknown"], true);
    assert_eq!(out[2]["error"]["detail"]["task_id"], TASK);
}

#[test]
fn quit_logs_out_and_stops_without_reading_later_lines() {
    let mut script = Script::default();
    let input = [login_line(), request("q", "session.quit", json!({})), request("late", "session.ping", json!({}))].concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out.last().unwrap()["id"], "q");
    assert!(!script.authenticated);
}

#[test]
fn image_sniffing_accepts_only_jpeg_png_webp() {
    assert_eq!(sniff_image(&png()), Some("image/png"));
    assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
    assert_eq!(sniff_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
    assert_eq!(sniff_image(b"GIF89a"), None);
}
