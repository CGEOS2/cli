use super::*;
use cgeos_sdk_shared::host::SessionStore;
use cgeos_sdk_shared::protocol::Credentials;
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
    /// Saved-session marker: set by a login that carries a session file, kept by disconnect, cleared by logout.
    saved: Option<PathBuf>,
    login_files: Vec<Option<PathBuf>>,
    restores: Vec<(String, String, PathBuf)>,
    restore_failure: Option<DesktopError>,
    disconnects: usize,
    cleared: Vec<Option<PathBuf>>,
    logout_failure: Option<DesktopError>,
}

impl Script {
    fn reply(&mut self, action: &str, reply: Reply) -> &mut Self {
        self.replies
            .entry(action.to_owned())
            .or_default()
            .push_back(reply);
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
    fn login(&mut self, _: &str, _: &str, _: &str, _: &str, session_file: Option<&Path>) -> Reply {
        self.authenticated = true;
        self.logins += 1;
        self.login_files.push(session_file.map(Path::to_path_buf));
        if let Some(file) = session_file {
            self.saved = Some(file.to_path_buf());
        }
        Ok(json!({"account": "acct", "authenticated": true}))
    }
    fn restore(&mut self, base_url: &str, device: &str, session_file: &Path) -> Reply {
        self.restores.push((
            base_url.to_owned(),
            device.to_owned(),
            session_file.to_path_buf(),
        ));
        if let Some(error) = self.restore_failure.clone() {
            return Err(error);
        }
        self.authenticated = true;
        Ok(json!({"account": "acct", "authenticated": true, "restored": true}))
    }
    fn disconnect(&mut self) {
        self.authenticated = false;
        self.disconnects += 1;
    }
    fn logout(&mut self, session_file: Option<&Path>) -> std::result::Result<(), DesktopError> {
        self.authenticated = false;
        if let Some(error) = self.logout_failure.clone() {
            return Err(error);
        }
        self.cleared.push(session_file.map(Path::to_path_buf));
        self.saved = None;
        Ok(())
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
    fn put_object(
        &mut self,
        bytes: &[u8],
        _: &Value,
        _: &str,
    ) -> std::result::Result<(), DesktopError> {
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
    let reader = BufReader::with_capacity(
        7,
        Trickle {
            inner: Cursor::new(input),
            step,
        },
    );
    let mut output = Vec::new();
    serve(
        reader,
        &mut output,
        script,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn login_line() -> String {
    request(
        "login",
        "auth.login",
        json!({"base_url": "https://api.example.test", "phone": "+15555550100", "code": "123456"}),
    )
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
    Ok(
        json!({"version": 1, "upload": {"resourceId": "00000000-0000-0000-0000-0000000000f1",
        "uploadUrl": "https://objects.example.test/put?sig=secret", "httpMethod": "PUT",
        "requiredHeaders": {"content-type": "image/png"}}}),
    )
}

#[test]
fn announces_ready_and_one_login_serves_many_calls_on_one_connection() {
    let mut script = Script::default();
    let input = [
        login_line(),
        request("1", "enterprise.list", json!({})),
        request("2", "site.list", json!({"enterprise": ENTERPRISE})),
        request(
            "3",
            "product.list",
            json!({"enterprise": ENTERPRISE, "site": SITE}),
        ),
        request(
            "4",
            "category.list",
            json!({"enterprise": ENTERPRISE, "site": SITE}),
        ),
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
    let input = [
        login_line(),
        request("中文-1", "site.list", json!({"enterprise": ENTERPRISE})),
    ]
    .concat();
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
    input
        .extend_from_slice(request("a", "site.list", json!({"enterprise": ENTERPRISE})).as_bytes());
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
        .reply(
            "Client.NeoCMS.Product.Query",
            Err(DesktopError::new("FORBIDDEN", "denied")),
        )
        .reply(
            "Client.NeoCMS.Product.Save",
            Err(DesktopError::new("CONFLICT", "revision")),
        );
    let input = [
        login_line(),
        request(
            "list",
            "product.list",
            json!({"enterprise": ENTERPRISE, "site": SITE}),
        ),
        request(
            "save",
            "product.save",
            json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
            "operation_id": OPERATION, "base_revision": 3, "body": {"title": "T"}}),
        ),
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
    script.reply(
        "Client.NeoCMS.Product.Save",
        Ok(json!({"version": 1, "status": "PENDING"})),
    );
    let input = [
        login_line(),
        request(
            "save",
            "product.save",
            json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
            "operation_id": OPERATION, "base_revision": 0, "body": {}}),
        ),
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
    let input = [
        login_line(),
        request("up", "media.upload", upload_args(&file)),
    ]
    .concat();
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
    failing.put_failure = Some(DesktopError::new(
        "UPLOAD_FAILED",
        "media upload rejected with HTTP status 403",
    ));
    let input = [
        login_line(),
        request("up", "media.upload", upload_args(&file)),
    ]
    .concat();
    let out = run(&mut failing, input.clone().into_bytes(), 4096);
    assert_eq!(out[2]["error"]["code"], "UPLOAD_FAILED");
    assert_eq!(failing.count("Client.NeoCMS.Media.Confirm"), 0);

    let mut unknown = Script::default();
    unknown.reply("Client.NeoCMS.Media.Presign", presign_reply());
    unknown.reply(
        "Client.NeoCMS.Media.Confirm",
        Err(DesktopError::new("UNAVAILABLE", "lost")),
    );
    let out = run(&mut unknown, input.into_bytes(), 4096);
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(out[2]["error"]["outcome_unknown"], true);
    assert_eq!(
        out[2]["error"]["detail"]["resource_id"],
        "00000000-0000-0000-0000-0000000000f1"
    );
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
    request(
        "pub",
        "product.publish",
        json!({"enterprise": ENTERPRISE, "site": SITE, "product": PRODUCT,
        "version": 1, "operation_id": OPERATION, "expected_revision": 4, "wait_seconds": wait}),
    )
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
    assert_eq!(out[2]["error"]["detail"]["publication"]["status"], "FAILED");

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
    assert_eq!(
        out[2]["error"]["detail"]["publication"]["status"],
        "ACTIVATING"
    );
}

#[test]
fn quit_disconnects_but_keeps_the_saved_session_and_stops_reading() {
    let mut script = Script::default();
    let login = request(
        "login",
        "auth.login",
        json!({"base_url": "https://api.example.test", "phone": "+15555550100",
        "code": "123456", "session_file": "C:/Users/测试 用户/session.dat"}),
    );
    let input = [
        login,
        request("q", "session.quit", json!({})),
        request("late", "session.ping", json!({})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out.last().unwrap()["id"], "q");
    assert!(!script.authenticated);
    assert_eq!(script.disconnects, 1);
    assert!(
        script.cleared.is_empty(),
        "closing must not forget the login"
    );
    assert_eq!(
        script.saved,
        Some(PathBuf::from("C:/Users/测试 用户/session.dat"))
    );
}

#[test]
fn losing_the_peer_without_quit_also_only_disconnects() {
    let mut script = Script::default();
    let login = request(
        "login",
        "auth.login",
        json!({"base_url": "https://api.example.test", "phone": "+15555550100",
        "code": "123456", "session_file": "/tmp/s.dat"}),
    );
    run(&mut script, login.into_bytes(), 4096); // input ends: stdin closed
    assert_eq!(script.disconnects, 1);
    assert!(script.cleared.is_empty());
    assert!(script.saved.is_some());
}

#[test]
fn login_without_a_session_file_keeps_the_old_call_working() {
    let mut script = Script::default();
    let out = run(&mut script, login_line().into_bytes(), 4096);
    assert_eq!(out[1]["ok"], true);
    assert_eq!(script.login_files, vec![None]);
    assert!(script.saved.is_none());
}

#[test]
fn restore_needs_a_session_file_and_passes_backend_device_and_path() {
    let mut script = Script::default();
    let input = [
        request(
            "bad",
            "auth.restore",
            json!({"base_url": "https://api.example.test"}),
        ),
        request(
            "r",
            "auth.restore",
            json!({"base_url": "https://api.example.test", "device": "dev-1",
            "session_file": "C:/数据 目录/session.dat"}),
        ),
        request("e", "enterprise.list", json!({})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[1]["error"]["code"], "BAD_REQUEST");
    assert_eq!(out[2]["ok"], true);
    assert_eq!(out[2]["data"]["restored"], true);
    assert_eq!(
        out[3]["ok"], true,
        "calls work after a restore without any code or login"
    );
    assert_eq!(script.logins, 0);
    assert_eq!(
        script.restores,
        vec![(
            "https://api.example.test".into(),
            "dev-1".into(),
            PathBuf::from("C:/数据 目录/session.dat")
        )]
    );
}

#[test]
fn restore_failures_keep_their_codes_and_leave_the_bridge_unauthenticated() {
    for code in [
        "NO_SESSION",
        "SESSION_CORRUPT",
        "SESSION_MISMATCH",
        "SESSION_EXPIRED",
        "SESSION_OFFLINE",
    ] {
        let mut script = Script::default();
        script.restore_failure = Some(DesktopError::new(code, "x"));
        let input = [
            request(
                "r",
                "auth.restore",
                json!({"base_url": "https://api.example.test", "session_file": "/tmp/s.dat"}),
            ),
            request("e", "enterprise.list", json!({})),
        ]
        .concat();
        let out = run(&mut script, input.into_bytes(), 4096);
        assert_eq!(out[1]["error"]["code"], code);
        assert_eq!(out[1]["error"]["outcome_unknown"], false);
        assert_eq!(out[2]["error"]["code"], "NOT_AUTHENTICATED");
    }
}

#[test]
fn logout_clears_the_named_session_and_reports_a_failed_removal() {
    let mut script = Script::default();
    let input = [
        login_line(),
        request("o", "auth.logout", json!({"session_file": "/tmp/s.dat"})),
        request("e", "enterprise.list", json!({})),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["ok"], true);
    assert_eq!(out[2]["data"]["authenticated"], false);
    assert_eq!(out[3]["error"]["code"], "NOT_AUTHENTICATED");
    assert_eq!(script.cleared, vec![Some(PathBuf::from("/tmp/s.dat"))]);

    let mut failing = Script::default();
    failing.logout_failure = Some(DesktopError::new("SESSION_CLEAR_FAILED", "locked"));
    let out = run(
        &mut failing,
        request("o", "auth.logout", json!({})).into_bytes(),
        4096,
    );
    assert_eq!(out[1]["error"]["code"], "SESSION_CLEAR_FAILED");
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cgeos2-session-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Reversible stand-in for DPAPI so the file logic is testable on every platform.
struct Xor(u8);

impl session_file::Protector for Xor {
    fn protect(&self, plain: &[u8]) -> std::result::Result<Vec<u8>, SessionFileError> {
        Ok(plain.iter().map(|byte| byte ^ self.0).collect())
    }
    fn unprotect(&self, sealed: &[u8]) -> std::result::Result<Vec<u8>, SessionFileError> {
        self.protect(sealed)
    }
}

struct Broken;

impl session_file::Protector for Broken {
    fn protect(&self, _: &[u8]) -> std::result::Result<Vec<u8>, SessionFileError> {
        Err(SessionFileError::Io("cannot seal".into()))
    }
    fn unprotect(&self, _: &[u8]) -> std::result::Result<Vec<u8>, SessionFileError> {
        Err(SessionFileError::Corrupt)
    }
}

const BASE: &str = "https://api.example.test";
const DEVICE: &str = "cgeos2-workbench";

fn credentials(device: &str) -> Credentials {
    Credentials {
        account: Uuid::from_u128(1),
        session: Uuid::from_u128(2),
        device_id: device.to_owned(),
        token: "ab".repeat(32),
        kind: ClientKind::Web,
    }
}

fn file_in(dir: &Path) -> SessionFile {
    SessionFile::new(
        dir.join("数据 目录").join("session.dat"),
        Arc::new(Xor(0x5a)),
    )
}

#[test]
fn saved_session_round_trips_without_plaintext_and_leaves_no_temp_file() {
    let dir = scratch();
    let file = file_in(&dir);
    file.save(BASE, &credentials(DEVICE)).unwrap();
    let raw = std::fs::read_to_string(dir.join("数据 目录").join("session.dat")).unwrap();
    assert!(
        !raw.contains(&"ab".repeat(32)) && !raw.contains(BASE) && !raw.contains(DEVICE),
        "{raw}"
    );
    let loaded = file.load(&format!("{BASE}/"), DEVICE).unwrap();
    assert!(loaded == credentials(DEVICE));
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("数据 目录")).unwrap().collect();
    assert_eq!(leftovers.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn another_backend_or_device_is_a_mismatch_and_the_file_is_kept() {
    let dir = scratch();
    let file = file_in(&dir);
    file.save(BASE, &credentials(DEVICE)).unwrap();
    assert_eq!(
        file.load("https://other.example.test", DEVICE).err(),
        Some(SessionFileError::Mismatch)
    );
    assert_eq!(
        file.load(BASE, "another-device").err(),
        Some(SessionFileError::Mismatch)
    );
    assert!(
        file.load(BASE, DEVICE).is_ok(),
        "a mismatch must not destroy the saved login"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_and_damaged_files_are_told_apart() {
    let dir = scratch();
    let file = file_in(&dir);
    assert_eq!(
        file.load(BASE, DEVICE).err(),
        Some(SessionFileError::Missing)
    );
    file.save(BASE, &credentials(DEVICE)).unwrap();
    let path = dir.join("数据 目录").join("session.dat");
    let good = std::fs::read_to_string(&path).unwrap();
    let other_key = SessionFile::new(&path, Arc::new(Xor(0x11)));
    for broken in [
        String::new(),
        "not json".to_owned(),
        good[..good.len() / 2].to_owned(),
        good.replace("\"format\":1", "\"format\":9"),
        r#"{"format":1,"data":"!!!"}"#.to_owned(),
    ] {
        std::fs::write(&path, &broken).unwrap();
        assert_eq!(
            file.load(BASE, DEVICE).err(),
            Some(SessionFileError::Corrupt),
            "{broken}"
        );
    }
    std::fs::write(&path, &good).unwrap();
    assert_eq!(
        other_key.load(BASE, DEVICE).err(),
        Some(SessionFileError::Corrupt),
        "wrong key reads as damaged"
    );
    // a payload that decrypts but carries invalid credentials is also damaged
    let mut bad = credentials(DEVICE);
    bad.token = "short".into();
    file.save(BASE, &bad).unwrap();
    assert_eq!(
        file.load(BASE, DEVICE).err(),
        Some(SessionFileError::Corrupt)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clear_removes_the_file_and_is_idempotent() {
    let dir = scratch();
    let file = file_in(&dir);
    file.save(BASE, &credentials(DEVICE)).unwrap();
    file.clear().unwrap();
    file.clear().unwrap();
    assert_eq!(
        file.load(BASE, DEVICE).err(),
        Some(SessionFileError::Missing)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn store_mirrors_login_to_the_file_and_the_sdk_clear_removes_it() {
    let dir = scratch();
    let file = file_in(&dir);
    let store = PersistingStore::with_file(file.clone(), BASE);
    assert!(store.persists());
    store.save(&credentials(DEVICE)).unwrap();
    assert!(store.take_failure().is_none());
    assert!(file.load(BASE, DEVICE).is_ok());
    assert!(store.load().unwrap().is_some());
    store.clear().unwrap(); // what the SDK does when the server rejects the credentials
    assert!(store.load().unwrap().is_none());
    assert_eq!(
        file.load(BASE, DEVICE).err(),
        Some(SessionFileError::Missing)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_write_keeps_the_login_alive_reports_it_and_drops_the_stale_file() {
    let dir = scratch();
    let good = file_in(&dir);
    good.save(BASE, &credentials(DEVICE)).unwrap(); // an older saved login
    let broken = SessionFile::new(dir.join("数据 目录").join("session.dat"), Arc::new(Broken));
    let store = PersistingStore::with_file(broken, BASE);
    store.save(&credentials(DEVICE)).unwrap();
    assert!(
        store.take_failure().is_some(),
        "the caller must be able to tell the login was not persisted"
    );
    assert!(store.take_failure().is_none());
    assert!(
        store.load().unwrap().is_some(),
        "the running session is unaffected"
    );
    assert_eq!(
        good.load(BASE, DEVICE).err(),
        Some(SessionFileError::Missing),
        "an older account must not come back"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn memory_only_store_never_touches_the_disk() {
    let store = PersistingStore::memory_only();
    assert!(!store.persists());
    store.save(&credentials(DEVICE)).unwrap();
    assert!(store.take_failure().is_none());
    store.clear().unwrap();
    assert!(store.load().unwrap().is_none());
}

#[test]
fn restore_distinguishes_rejected_credentials_from_being_offline() {
    let code = |status, timed_out| match restore_outcome(status, timed_out) {
        Some(Ok(())) => "READY".to_owned(),
        Some(Err(error)) => error.code,
        None => "WAIT".to_owned(),
    };
    assert_eq!(code(Status::Ready, false), "READY");
    assert_eq!(code(Status::CredentialsCleared, false), "SESSION_EXPIRED");
    assert_eq!(code(Status::StorageFailure, false), "SESSION_STORAGE");
    assert_eq!(code(Status::UpdateRequired, false), "UPDATE_REQUIRED");
    assert_eq!(code(Status::Connecting, false), "WAIT");
    assert_eq!(code(Status::Offline, false), "WAIT");
    assert_eq!(code(Status::Offline, true), "SESSION_OFFLINE");
    assert_eq!(code(Status::Connecting, true), "SESSION_OFFLINE");
    assert!(
        !restore_outcome(Status::Offline, true)
            .unwrap()
            .unwrap_err()
            .outcome_unknown
    );
}

#[test]
fn session_errors_map_to_stable_protocol_codes() {
    assert_eq!(session_error(SessionFileError::Missing).code, "NO_SESSION");
    assert_eq!(
        session_error(SessionFileError::Corrupt).code,
        "SESSION_CORRUPT"
    );
    assert_eq!(
        session_error(SessionFileError::Mismatch).code,
        "SESSION_MISMATCH"
    );
    assert_eq!(
        session_error(SessionFileError::Io("x".into())).code,
        "SESSION_STORAGE"
    );
}

#[test]
fn image_sniffing_accepts_only_jpeg_png_webp() {
    assert_eq!(sniff_image(&png()), Some("image/png"));
    assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
    assert_eq!(
        sniff_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(sniff_image(b"GIF89a"), None);
}

fn enterprise_receipt(status: &str) -> Reply {
    let node = if status == "COMMITTED" {
        "COMMITTED"
    } else {
        "PENDING"
    };
    Ok(
        json!({"version":2,"enterprise_id":ENTERPRISE,"product_id":PRODUCT,"task_id":OPERATION,"operation_id":OPERATION,"product_revision":4,"status":status,"target_sites":1,"target_nodes":1,"sites":[{"site_id":SITE,"task_id":TASK,"publication_id":PRODUCT,"revision":2,"status":status,"target_nodes":1,"activated_nodes":if status=="COMMITTED" {1} else {0},"nodes":[{"node_id":SITE,"status":node}]}]}),
    )
}

#[test]
fn enterprise_publish_ignores_old_site_parameter_and_validates_parent_receipt() {
    let mut script = Script::default();
    script
        .reply(
            "Client.NeoCMS.Product.Publish",
            enterprise_receipt("DELIVERING"),
        )
        .reply(
            "Client.NeoCMS.Product.PublishStatus",
            enterprise_receipt("COMMITTED"),
        );
    let input=[login_line(),request("v2","product.publish",json!({"enterprise":ENTERPRISE,"site":"retired-site","product":PRODUCT,"operation_id":OPERATION,"expected_revision":4,"wait_seconds":30}))].concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["ok"], true);
    for (_, _, body) in script
        .calls
        .iter()
        .filter(|(action, _, _)| action.starts_with("Client.NeoCMS.Product.Publish"))
    {
        assert_eq!(body["version"], 2);
        assert!(body.get("site_id").is_none());
    }
}

#[test]
fn enterprise_sync_status_validates_receipt_without_product_or_site() {
    let mut script = Script::default();
    let mut receipt = enterprise_receipt("COMMITTED").unwrap();
    receipt["product_id"] = Value::Null;
    receipt["product_revision"] = json!(0);
    script.reply("Client.NeoCMS.Product.PublishStatus", Ok(receipt.clone()));
    receipt["target_nodes"] = json!(99);
    script.reply("Client.NeoCMS.Product.PublishStatus", Ok(receipt));
    let args = json!({"enterprise":ENTERPRISE,"task":OPERATION});
    let input = [
        login_line(),
        request("status", "product.publish_status", args.clone()),
        request("bad", "product.publish_status", args),
    ]
    .concat();
    let out = run(&mut script, input.into_bytes(), 4096);
    assert_eq!(out[2]["ok"], true);
    assert_eq!(out[3]["error"]["code"], "BAD_RESPONSE");
    assert_eq!(script.calls[0].2, json!({"version":2,"task_id":OPERATION}));
}
