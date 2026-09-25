# CGEOS2 CLI

`cgeos2` 是 CGEOS2 下方的开源 Linux CLI，面向开发人员和 AI Agent 做可重复的 Access Service 与 Client Service 边界测试。

## 设计

- `access` 直接调用当前 `access-service` Rust crate 的确定性核心函数；不伪造网络成功，也不把尚未实现的浏览器宿主包装成已实现能力。
- `client` 直接调用当前 `client-service` 的 `terminal-protocol`，验证请求边界或生成带新关联 ID 的请求。
- 非 TTY stdout 默认输出 `{ "ok": true, "data": ... }` JSON，便于 Agent 解析；`--json` 强制相同格式。
- `client-service` shared crate 同时保留 `rlib` 与 `cdylib`，其他项目仍可链接 Rust 静态接口或 Linux `.so`。

## 构建

```bash
cargo build --release --locked
# target/release/cgeos2
```

CLI 通过相邻仓库路径依赖 `../access-service` 与 `../client-service`；不要复制服务源码。

## 示例

以下联网示例仅使用占位地址和环境变量。请从获授权的密钥管理渠道取得测试账号，
并通过标准输入传入一次性验证码；不要把真实账号、验证码或内部端点写入命令历史和文档。

```bash
cgeos2 access version
cgeos2 access init --encryption --device-consent
cgeos2 access validate-inquiry --name Agent --contact agent@example.test --message inspect
cgeos2 access digest --site-id demo --payload '{"message":"inspect"}'

cgeos2 client login --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin
cgeos2 client challenge --base-url https://api.example.test --phone "$AUTHORIZED_PHONE"
cgeos2 client build --action Client.Auth.Login.Request --payload '{"username":"tester"}'
cgeos2 client call --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --action Client.Tenant.Sites.List --enterprise 00000000-0000-0000-0000-000000000001

# HyperAdmin：同一登录会话内发起并轮询企业开通任务
cgeos2 client enterprise provision --base-url https://api.example.test \
  --phone "$AUTHORIZED_PHONE" --code-stdin --name TEST --owner <ACCOUNT_UUID> \
  --license-template <TEMPLATE_UUID> --expires-at-seconds 2082758400

# 企业站点：operation-id 可显式传入，超时后用原值重试，不要生成新值
cgeos2 client site create --base-url https://api.example.test \
  --phone "$AUTHORIZED_PHONE" --code-stdin --enterprise <ENTERPRISE_UUID> \
  --name 'TEST Site' --site-type site --domain site.example.test \
  --template-id fleks-site --template-version 0.1.0 --preset-id cubegarden-official

# Console 已发送 OTP 时复用 challenge；验证码只从 stdin 读取
printf '%s\n' "$OTP" | cgeos2 client site create --base-url https://api.example.test \
  --challenge <CHALLENGE_UUID> --code-stdin --enterprise <ENTERPRISE_UUID> \
  --name 'Example Site' --site-type site --domain site.example.test \
  --template-id fleks-site --template-version 0.1.0 --preset-id cubegarden-official

cgeos2 client outpost list --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin
cgeos2 client outpost assign --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --enterprise <ENTERPRISE_UUID> --site <SITE_UUID> --node <OUTPOST_UUID> --operation-id <UUID>
cgeos2 client site publish --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --enterprise <ENTERPRISE_UUID> --site <SITE_UUID> --page index --expected-revision 1 \
  --operation-id <UUID> --wait-seconds 120

# Agent 工具提案按站点隔离；确认时 Java 会重新校验 AI_USE 与 CONTENT_WRITE。
cgeos2 client agent run --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --enterprise <ENTERPRISE_UUID> --site <SITE_UUID> --message '更新商品草稿'
cgeos2 client agent session --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --enterprise <ENTERPRISE_UUID> --site <SITE_UUID>
cgeos2 client agent confirm --base-url https://api.example.test --phone "$AUTHORIZED_PHONE" --code-stdin \
  --enterprise <ENTERPRISE_UUID> --site <SITE_UUID> --approval <APPROVAL_UUID> --approve
```

`client login` 复用 `client-service` 原生登录流程。登录成功后凭据仅保存在当前进程内存，
token 不打印。账号及验证码由获授权的调试环境提供，仓库不记录内部测试凭据。
`client challenge` 可先生成预签登录挑战；后续联网命令用 `--challenge` 避免重复发送验证码，
并可用 `--code-stdin` 读取一行 OTP。`--code` 与 `--code-stdin` 冲突，验证码和 token 均不写入输出。
`client call` 同样只在当前进程保存凭据，并通过该 API origin 的 `/terminal` WSS
执行一次真实请求；仅可用于获授权的调试环境，服务端业务错误会以非零状态退出。
企业与站点专用命令复用一次登录连接、持有一个幂等 operation UUID，并轮询持久任务至
`active`、`failed` 或超时；输出不包含 token，失败与超时均为非零退出。

## 许可

CLI 源码使用 MIT；链接的 CGEOS2 服务 crate 与第三方组件继续使用各自许可证。
