use anyhow::{anyhow, Context, Result};
use cgeos_access_service::{
    initialization_plan, wasm_compute_canonical_digest, wasm_validate_inquiry_form,
};
use cgeos_sdk_shared::host::SessionStore;
use cgeos_sdk_shared::login::{AuthEndpoint, LoginClient};
use cgeos_sdk_shared::native::{Runtime, Status};
use cgeos_sdk_shared::protocol::{Action, ClientKind, Credentials, Outcome, Request};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "cgeos2",
    version,
    about = "CGEOS2 Linux CLI for AI-assisted service testing"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Access(AccessArgs),
    Client(ClientArgs),
}

#[derive(Args)]
struct AccessArgs {
    #[command(subcommand)]
    command: AccessCommand,
}

#[derive(Subcommand)]
enum AccessCommand {
    Version,
    Init {
        #[arg(long)]
        encryption: bool,
        #[arg(long)]
        device_consent: bool,
        #[arg(long)]
        advanced_consent: bool,
    },
    ValidateInquiry {
        #[arg(long)]
        name: String,
        #[arg(long)]
        contact: String,
        #[arg(long)]
        message: String,
    },
    Digest {
        #[arg(long)]
        site_id: String,
        #[arg(long)]
        payload: String,
        #[arg(long)]
        form_id: Option<String>,
    },
}

#[derive(Args)]
struct ClientArgs {
    #[command(subcommand)]
    command: ClientCommand,
}

#[derive(Subcommand)]
enum ClientCommand {
    /// Request a code and complete native login against one API origin.
    Login {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
    },
    /// Login and execute one Terminal request against an authorized API origin.
    Call {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        enterprise: Option<Uuid>,
        #[arg(long, default_value = "{}")]
        payload: String,
    },
    /// HyperAdmin enterprise provisioning with durable status polling.
    Enterprise {
        #[command(subcommand)]
        command: EnterpriseCommand,
    },
    /// Tenant site provisioning with durable status polling.
    Site {
        #[command(subcommand)]
        command: SiteCommand,
    },
    /// HyperAdmin site-to-Outpost assignments.
    Outpost {
        #[command(subcommand)]
        command: OutpostCommand,
    },
    Validate {
        request: String,
    },
    Build {
        #[arg(long)]
        action: String,
        #[arg(long, default_value = "{}")]
        payload: String,
        #[arg(long)]
        enterprise: Option<Uuid>,
    },
}

#[derive(Subcommand)]
enum EnterpriseCommand {
    Provision {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        owner: Uuid,
        #[arg(long)]
        license_template: Uuid,
        #[arg(long)]
        expires_at_seconds: i64,
        #[arg(long)]
        operation_id: Option<Uuid>,
        #[arg(long, default_value_t = 120)]
        wait_seconds: u64,
    },
}

#[derive(Subcommand)]
enum SiteCommand {
    Create {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        enterprise: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long, value_parser = ["site", "e-catalog"])]
        site_type: String,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        operation_id: Option<Uuid>,
        #[arg(long, default_value_t = 120)]
        wait_seconds: u64,
    },
    Publish {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        enterprise: Uuid,
        #[arg(long)]
        site: Uuid,
        #[arg(long, default_value = "index")]
        page: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long, default_value_t = 120)]
        wait_seconds: u64,
    },
}

#[derive(Subcommand)]
enum OutpostCommand {
    List {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        enterprise: Option<Uuid>,
        #[arg(long)]
        site: Option<Uuid>,
    },
    Assign {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        enterprise: Uuid,
        #[arg(long)]
        site: Uuid,
        #[arg(long)]
        node: Uuid,
        #[arg(long)]
        operation_id: Uuid,
    },
    Unassign {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        phone: String,
        #[arg(long)]
        code: String,
        #[arg(long, default_value = "cgeos2-cli")]
        device: String,
        #[arg(long)]
        enterprise: Uuid,
        #[arg(long)]
        site: Uuid,
        #[arg(long)]
        node: Uuid,
        #[arg(long)]
        operation_id: Uuid,
    },
}

#[derive(Default)]
struct MemoryStore(Mutex<Option<Credentials>>);

impl SessionStore for MemoryStore {
    fn load(&self) -> std::result::Result<Option<Credentials>, cgeos_sdk_shared::protocol::Code> {
        self.0
            .lock()
            .map(|store| store.clone())
            .map_err(|_| cgeos_sdk_shared::protocol::Code::Unavailable)
    }
    fn save(
        &self,
        credentials: &Credentials,
    ) -> std::result::Result<(), cgeos_sdk_shared::protocol::Code> {
        self.0
            .lock()
            .map(|mut store| *store = Some(credentials.clone()))
            .map_err(|_| cgeos_sdk_shared::protocol::Code::Unavailable)
    }
    fn clear(&self) -> std::result::Result<(), cgeos_sdk_shared::protocol::Code> {
        self.0
            .lock()
            .map(|mut store| *store = None)
            .map_err(|_| cgeos_sdk_shared::protocol::Code::Unavailable)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let value = match cli.command {
        Command::Access(args) => run_access(args.command)?,
        Command::Client(args) => run_client(args.command)?,
    };
    if cli.json || !std::io::stdout().is_terminal() {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok": true, "data": value}))?
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn run_access(command: AccessCommand) -> Result<Value> {
    match command {
        AccessCommand::Version => Ok(json!({
            "access_service_version": cgeos_access_service::ACCESS_SERVICE_VERSION,
            "target_protocol_revision": cgeos_access_service::TARGET_PROTOCOL_REVISION,
        })),
        AccessCommand::Init {
            encryption,
            device_consent,
            advanced_consent,
        } => Ok(json!({"plan": initialization_plan(encryption, device_consent, advanced_consent)})),
        AccessCommand::ValidateInquiry {
            name,
            contact,
            message,
        } => {
            let output = wasm_validate_inquiry_form(&name, &contact, &message);
            serde_json::from_str(&output).context("access-service returned invalid JSON")
        }
        AccessCommand::Digest {
            site_id,
            payload,
            form_id,
        } => {
            let output = wasm_compute_canonical_digest(&site_id, form_id, &payload);
            Ok(json!({"digest": output}))
        }
    }
}

fn run_client(command: ClientCommand) -> Result<Value> {
    match command {
        ClientCommand::Login {
            base_url,
            phone,
            code,
            device,
        } => {
            let endpoint = AuthEndpoint::new(&base_url)
                .map_err(|code| anyhow!("invalid auth endpoint: {code:?}"))?;
            let client = LoginClient::new(endpoint)
                .map_err(|code| anyhow!("cannot create login client: {code:?}"))?;
            let challenge = client
                .request_code(&phone)
                .map_err(|code| anyhow!("request code failed: {code:?}"))?;
            let store = MemoryStore::default();
            let credentials = client
                .login(&challenge, &code, &device, ClientKind::Web, &store)
                .map_err(|code| anyhow!("login failed: {code:?}"))?;
            Ok(json!({
                "challenge": challenge,
                "account": credentials.account,
                "session": credentials.session,
                "device_id": credentials.device_id,
                "kind": credentials.kind,
                "credentials_held_in_memory": true,
                "token": "<redacted>",
            }))
        }
        ClientCommand::Call {
            base_url,
            phone,
            code,
            device,
            action,
            enterprise,
            payload,
        } => {
            let mut runtime = authenticated_runtime(&base_url, &phone, &code, &device)?;
            let payload: Value = serde_json::from_str(&payload).context("payload must be JSON")?;
            let result = terminal_request(&runtime, &action, enterprise, payload);
            runtime.shutdown();
            Ok(json!({
                "action": action,
                "enterprise": enterprise,
                "result": result?,
            }))
        }
        ClientCommand::Enterprise { command } => match command {
            EnterpriseCommand::Provision {
                base_url,
                phone,
                code,
                device,
                name,
                owner,
                license_template,
                expires_at_seconds,
                operation_id,
                wait_seconds,
            } => {
                let operation = operation_id.unwrap_or_else(Uuid::new_v4);
                let mut runtime = authenticated_runtime(&base_url, &phone, &code, &device)?;
                let start = terminal_request(
                    &runtime,
                    "Platform.Enterprise.Provision.Start",
                    None,
                    json!({"operation_id":operation,"display_name":name,"owner_account_id":owner,
                        "license_template_id":license_template,"license_expires_at_seconds":expires_at_seconds}),
                );
                let result = match start {
                    Ok(value) => poll_task(
                        &runtime,
                        "Platform.Enterprise.Provision.Get",
                        None,
                        operation,
                        value,
                        wait_seconds,
                    ),
                    Err(error) => Err(error),
                };
                runtime.shutdown();
                result
            }
        },
        ClientCommand::Site { command } => match command {
            SiteCommand::Create {
                base_url,
                phone,
                code,
                device,
                enterprise,
                name,
                site_type,
                domain,
                operation_id,
                wait_seconds,
            } => {
                let operation = operation_id.unwrap_or_else(Uuid::new_v4);
                let mut runtime = authenticated_runtime(&base_url, &phone, &code, &device)?;
                let start = terminal_request(
                    &runtime,
                    "Client.Tenant.Sites.Create",
                    Some(enterprise),
                    json!({"operation_id":operation,"name":name,"site_type":site_type,
                        "primary_domain":domain}),
                );
                let result = match start {
                    Ok(value) => poll_task(
                        &runtime,
                        "Client.Tenant.Sites.CreateStatus",
                        Some(enterprise),
                        operation,
                        value,
                        wait_seconds,
                    ),
                    Err(error) => Err(error),
                };
                runtime.shutdown();
                result
            }
            SiteCommand::Publish {
                base_url,
                phone,
                code,
                device,
                enterprise,
                site,
                page,
                expected_revision,
                operation_id,
                wait_seconds,
            } => {
                if expected_revision < 1 {
                    return Err(anyhow!("expected-revision must be positive"));
                }
                let mut runtime = authenticated_runtime(&base_url, &phone, &code, &device)?;
                let start = terminal_request(
                    &runtime,
                    "Client.NeoCMS.Page.Publish",
                    Some(enterprise),
                    json!({"operation_id":operation_id,"site_id":site,"page_key":page,
                        "expected_revision":expected_revision}),
                );
                let result = match start {
                    Ok(value) => poll_publication(
                        &runtime,
                        enterprise,
                        site,
                        &page,
                        operation_id,
                        value,
                        wait_seconds,
                    ),
                    Err(error) => Err(error),
                };
                runtime.shutdown();
                result
            }
        },
        ClientCommand::Outpost { command } => {
            let (base_url, phone, code, device, action, payload) = match command {
                OutpostCommand::List {
                    base_url,
                    phone,
                    code,
                    device,
                    enterprise,
                    site,
                } => (
                    base_url,
                    phone,
                    code,
                    device,
                    "Platform.Outpost.Site.List",
                    json!({"enterprise_id":enterprise,"site_id":site}),
                ),
                OutpostCommand::Assign {
                    base_url,
                    phone,
                    code,
                    device,
                    enterprise,
                    site,
                    node,
                    operation_id,
                } => (
                    base_url,
                    phone,
                    code,
                    device,
                    "Platform.Outpost.Site.Assign",
                    json!({"operation_id":operation_id,"enterprise_id":enterprise,"site_id":site,"node_id":node}),
                ),
                OutpostCommand::Unassign {
                    base_url,
                    phone,
                    code,
                    device,
                    enterprise,
                    site,
                    node,
                    operation_id,
                } => (
                    base_url,
                    phone,
                    code,
                    device,
                    "Platform.Outpost.Site.Unassign",
                    json!({"operation_id":operation_id,"enterprise_id":enterprise,"site_id":site,"node_id":node}),
                ),
            };
            let mut runtime = authenticated_runtime(&base_url, &phone, &code, &device)?;
            let result = terminal_request(&runtime, action, None, payload);
            runtime.shutdown();
            result
        }
        ClientCommand::Validate { request } => {
            let parsed =
                Request::decode(&request).map_err(|code| anyhow!("invalid request: {code:?}"))?;
            Ok(serde_json::to_value(parsed).context("serialize request")?)
        }
        ClientCommand::Build {
            action,
            payload,
            enterprise,
        } => {
            let action =
                Action::try_from(action).map_err(|code| anyhow!("invalid action: {code:?}"))?;
            let payload: Value = serde_json::from_str(&payload).context("payload must be JSON")?;
            let request = Request {
                id: next_request_id(),
                action,
                enterprise,
                payload,
            };
            let encoded = serde_json::to_string(&request)?;
            Request::decode(&encoded)
                .map_err(|code| anyhow!("constructed invalid request: {code:?}"))?;
            Ok(json!({"request": request, "encoded": encoded}))
        }
    }
}

fn poll_publication(
    runtime: &Runtime,
    enterprise: Uuid,
    site: Uuid,
    page: &str,
    task: Uuid,
    mut value: Value,
    wait_seconds: u64,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(wait_seconds.clamp(1, 3600));
    loop {
        match value.get("status").and_then(Value::as_str) {
            Some("COMMITTED") => return Ok(value),
            Some("FAILED") => return Err(anyhow!("publication failed: {}", value)),
            Some("DELIVERING" | "ACTIVATING" | "PENDING_ACK") if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(500));
                value = terminal_request(
                    runtime,
                    "Client.NeoCMS.Page.PublishStatus",
                    Some(enterprise),
                    json!({"task_id":task,"site_id":site,"page_key":page}),
                )?;
            }
            Some("DELIVERING" | "ACTIVATING" | "PENDING_ACK") => {
                return Err(anyhow!("publication timed out; operation_id={task}"))
            }
            _ => return Err(anyhow!("invalid publication response")),
        }
    }
}

fn authenticated_runtime(base_url: &str, phone: &str, code: &str, device: &str) -> Result<Runtime> {
    let endpoint =
        AuthEndpoint::new(base_url).map_err(|value| anyhow!("invalid auth endpoint: {value:?}"))?;
    let client = LoginClient::new(endpoint.clone())
        .map_err(|value| anyhow!("cannot create login client: {value:?}"))?;
    let challenge = client
        .request_code(phone)
        .map_err(|value| anyhow!("request code failed: {value:?}"))?;
    let store = Arc::new(MemoryStore::default());
    let credentials = client
        .login(&challenge, code, device, ClientKind::Web, store.as_ref())
        .map_err(|value| anyhow!("login failed: {value:?}"))?;
    let terminal = endpoint
        .terminal_endpoint(resolve_terminal_address(base_url)?)
        .map_err(|value| anyhow!("invalid terminal endpoint: {value:?}"))?;
    let runtime = Runtime::start(terminal, credentials, store)
        .map_err(|value| anyhow!("cannot start terminal runtime: {value:?}"))?;
    wait_until_ready(&runtime)?;
    Ok(runtime)
}

fn terminal_request(
    runtime: &Runtime,
    action: &str,
    enterprise: Option<Uuid>,
    payload: Value,
) -> Result<Value> {
    let action =
        Action::try_from(action.to_string()).map_err(|code| anyhow!("invalid action: {code:?}"))?;
    match runtime
        .handle()
        .begin(action, enterprise, payload)
        .map_err(|code| anyhow!("cannot submit request: {code:?}"))?
        .wait()
    {
        Outcome::Ok { data } => Ok(data),
        Outcome::Error { code } => Err(anyhow!("request failed: {code:?}")),
    }
}

fn poll_task(
    runtime: &Runtime,
    status_action: &str,
    enterprise: Option<Uuid>,
    operation: Uuid,
    mut value: Value,
    wait_seconds: u64,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(wait_seconds.clamp(1, 3600));
    loop {
        match value.get("state").and_then(Value::as_str) {
            Some("active") => return Ok(value),
            Some("failed") => {
                return Err(anyhow!(
                    "provisioning failed at {}: {}",
                    value
                        .get("step")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    value
                        .get("error_code")
                        .and_then(Value::as_str)
                        .unwrap_or("UNAVAILABLE")
                ))
            }
            Some("pending") if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(500));
                value = terminal_request(
                    runtime,
                    status_action,
                    enterprise,
                    json!({"operation_id":operation}),
                )?;
            }
            Some("pending") => {
                return Err(anyhow!(
                    "provisioning status timed out; operation_id={operation}"
                ))
            }
            _ => return Err(anyhow!("invalid provisioning response")),
        }
    }
}

fn resolve_terminal_address(base_url: &str) -> Result<Vec<std::net::SocketAddr>> {
    let parsed = url::Url::parse(base_url).context("base URL is invalid")?;
    let host = parsed.host_str().context("base URL has no host")?;
    let port = parsed
        .port_or_known_default()
        .context("base URL has no port")?;
    let mut addresses: Vec<_> = (host, port)
        .to_socket_addrs()
        .context("cannot resolve API host")?
        .collect();
    addresses.sort_by_key(|address| if address.is_ipv4() { 0 } else { 1 });
    addresses
        .into_iter()
        .next()
        .map(|address| vec![address])
        .context("API host resolved to no addresses")
}

fn wait_until_ready(runtime: &Runtime) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match runtime.handle().status() {
            Status::Ready => return Ok(()),
            Status::CredentialsCleared => {
                return Err(anyhow!("terminal credentials were rejected"))
            }
            Status::UpdateRequired => return Err(anyhow!("terminal requires a client update")),
            Status::StorageFailure => return Err(anyhow!("terminal session storage failed")),
            Status::Connecting | Status::Offline if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Status::Connecting | Status::Offline => {
                return Err(anyhow!("terminal connection timed out"))
            }
        }
    }
}

fn next_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |duration| duration.as_nanos() as u64);
    ((nanos % 999_999_999_999_999_999).max(1)).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_wraps_access_service_hash_in_json() {
        let value = run_access(AccessCommand::Digest {
            site_id: "demo".to_owned(),
            payload: r#"{"message":"inspect"}"#.to_owned(),
            form_id: None,
        })
        .unwrap();
        let digest = value["digest"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
