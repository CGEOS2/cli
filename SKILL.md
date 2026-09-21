# CGEOS2 CLI Skill

Use this skill when an AI Agent needs to inspect or exercise CGEOS2 service boundaries from Linux.

## Safety and output contract

- Prefer `--json`; stdout is machine-readable for non-TTY execution.
- Treat a non-zero exit as failure; never infer login success from a challenge response.
- `client login` performs real HTTP requests to `/auth/challenge` and `/auth/login`; it is not
  a local simulation. Use only an authorized debug endpoint.
- For Portal `--debug-pass`, pass the fixed phone exactly as `REDACTED_DEBUG_PHONE`; do not add `+`.
- The CLI-only `debug-pass` feature does not weaken default `client-service` builds.

## Access Service checks

```bash
cgeos2 --json access version
cgeos2 --json access init --encryption --device-consent
cgeos2 --json access validate-inquiry \
  --name 'Agent' --contact 'agent@example.test' --message 'inspect'
cgeos2 --json access digest --site-id demo \
  --payload '{"message":"inspect"}'
```

Use these for deterministic checks of version/protocol metadata, initialization decisions,
inquiry validation, and canonical payload digest behavior. They call the current Rust
`access-service` implementation; they do not issue HTTP requests.

## Client Service checks

Build a fresh correlated terminal request:

```bash
cgeos2 --json client build \
  --action Client.Auth.Login.Request \
  --payload '{"username":"fixture"}'

cgeos2 --json client validate "$REQUEST_JSON"
```

`client validate` enforces the current `client-service` protocol size, action, correlation-ID,
enterprise, and reserved-auth-field rules. A rejected request is expected negative evidence;
record the exact error and exit status.

For one authorized live Portal request, use `client call`. It logs in and opens the Terminal WSS
within one process, never prints the token, and exits non-zero for a server-side error:

```bash
cgeos2 --json client call \
  --base-url https://api.example.test --phone REDACTED_DEBUG_PHONE --code REDACTED_DEBUG_CODE \
  --action Client.Tenant.Sites.List \
  --enterprise 00000000-0000-0000-0000-000000000001
```

## Agent workflow

1. Run `access version` and record the protocol revision.
2. Run one positive and one intentionally invalid inquiry case.
3. Build a client request, then validate its `encoded` field.
4. For boundary testing, mutate one field at a time (invalid action, empty ID, reserved auth key,
   oversized input) and expect rejection.
5. Report command, exit status, JSON result, and whether the check was local-only or involved a
   real Portal. Do not substitute a local check for an end-to-end check.

## Source boundaries

The CLI consumes `../access-service` and `../client-service` as path dependencies. It must not
copy service code or add alternate protocol definitions. The client shared crate exports both
`rlib` and `cdylib`; changes must preserve both forms for downstream Rust and `.so` consumers.
