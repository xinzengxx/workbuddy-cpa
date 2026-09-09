# workbuddy 插件 Rust 重写实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在新仓库 `/Users/xinyuan/Downloads/GitHub/workbuddy-cpa` 用 Rust 重写 workbuddy CLIProxyAPI 插件，产物为纯 C ABI 动态库，功能对齐 Go 版并新增配置驱动模型列表，最终经 GitHub Actions 发布六平台预编译产物。

**Architecture:** cdylib 导出 CPA C ABI 四函数（init/call/free/shutdown）；JSON RPC envelope 与宿主 v7.2.130 逐字段对齐；ureq+rustls 同步 HTTP；流式请求用 std::thread 后台泵 host.stream.emit。

**Tech Stack:** Rust stable（本机 1.97.1 Homebrew）、ureq、rustls、rustls-native-certs、serde、serde_json、serde_yaml、base64、cookie_store、time。

**Spec:** `docs/superpowers/specs/2026-09-09-rust-rewrite-design.md`（本仓库内；原始版本在 `../workbuddy-cliproxy/docs/superpowers/specs/`）。

## Global Constraints

- 不依赖任何 CPA Go SDK 包；crate-type 仅 `["cdylib"]`。
- 禁止引入 tokio/async-std、openssl。
- envelope：`{"ok":bool,"result":<raw json>,"error":{"code","message"}}`。
- registration：`schema_version=3`；capabilities 仅声明 `model_provider`、`auth_provider`、`executor`、`executor_model_scope:"both"`、`executor_input_formats:["chat-completions"]`、`executor_output_formats:["chat-completions"]`、`management_api`。
- `[]byte` 字段（Payload/StorageJSON/Body/payload）= JSON base64 字符串；时间字段 = RFC3339。
- Go 版未导出字段 JSON 键为导出名原样（如 `StorageJSON`、`OwnedBy`）；带 Go json tag 的用 tag（如 `stream_id`、`auth_index`）。
- 上游常量（与 Go 版逐字一致）：providerName=`workbuddy`；upstreamBase=`https://copilot.tencent.com`；clientUA=`CLI/2.63.2 CodeBuddy/2.63.2`；originReferer=`https://www.codebuddy.cn`；endpoints：`/v2/plugin/auth/state?platform=CLI`、`/v2/plugin/login/account?state=`、`/v2/plugin/auth/token?state=`、`/v2/plugin/auth/token/refresh`、`/v2/chat/completions`、`/v2/billing/meter/get-user-resource`。
- 凭据文件名 `workbuddy.json`；存储结构 `{"auth":{"accessToken","refreshToken","expiresAt","domain"},"account":{"uid","enterpriseId","nickname"}}`（注意 Go 版 JSON tag 是 camelCase：`accessToken` 等）。
- 上游拒绝非流式：执行一律强制 `stream:true` 并聚合/转发。
- 测试用隔离实例：`-config /tmp/cpa-test.conf`，端口 8399，`secret-key: "testkey123"`，管理 API 用 `Authorization: Bearer testkey123`；结束后必须 pkill 测试实例。
- 每个 Task 结束 commit；隔离验证实例用完即清。

## 契约对照表（实现/验收的唯一依据，已从宿主 v7.2.130 源码提取）

| RPC | 请求关键字段 | 响应 result 关键字段 |
|---|---|---|
| plugin.register / plugin.reconfigure | reconfigure: `config_yaml`(base64), `schema_version` | registration（见 Global Constraints） |
| model.static / model.for_auth | — / `auth_id`+`storage_json`(base64) | `{"Provider":"workbuddy","Models":[ModelInfo...]}` |
| auth.identifier / executor.identifier | — | `{"Identifier":"workbuddy"}` |
| auth.parse | `raw_json`(base64) | `{"Handled":bool,"Auth":AuthData}` |
| auth.login.start | — | `{"Provider","URL","State","ExpiresAt"}` |
| auth.login.poll | `state` | `{"Status":"pending|success|error","Message","Auth"}` |
| auth.refresh | `storage_json`(base64) | `{"Auth":AuthData}` |
| executor.execute | `model`,`stream`,`payload`(base64),`original_request`,`storage_json`,`metadata` | `{"Payload":base64,"Headers":{...}}` |
| executor.execute_stream | 同上 + `stream_id` | 立即返回 `{"Headers":{...}}`（chunks 空）；后台 emit |
| management.register | `base_path`,`resource_base_path` | `{"routes":[{"method","path","description"}],"resources":[{"path","menu","description"}]}` |
| management.handle | `method`,`path`,`query`,`body`(base64) | `{"StatusCode","Headers","Body":base64}` |
| host.stream.emit（插件→宿主） | `{"stream_id","payload":base64,"error"}` | `{}` |
| host.stream.close（插件→宿主） | `{"stream_id","error"}` | `{}` |
| host.auth.list（插件→宿主） | — | `{"files":[{"id","auth_index","name","type","provider","label"}]}` |
| host.auth.get（插件→宿主） | `{"auth_index"}` | `{"auth_index","name","path","json":base64}` |
| host.log（插件→宿主） | `{"level","message"}` | `{}` |

ModelInfo 字段（PascalCase 原样）：`ID,Object,Created,OwnedBy,Type,DisplayName,Name,Version,Description,InputTokenLimit,OutputTokenLimit,SupportedGenerationMethods,ContextLength,MaxCompletionTokens,UserDefined`。
AuthData 字段：`Provider,ID,FileName,Label,Prefix,ProxyURL,Disabled,StorageJSON(base64),Metadata,Attributes,NextRefreshAfter`。

---

### Task 1: Cargo 骨架 + cdylib 编译通过

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`（空壳）
- Create: `.gitignore`（`/target`）

**Interfaces:**
- Produces: 可编译的 cdylib；后续所有模块挂到 `src/lib.rs` 的 `mod` 声明下。

- [ ] **Step 1: 写 Cargo.toml**

```toml
[package]
name = "workbuddy"
version = "0.2.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]
name = "workbuddy"

[dependencies]
ureq = { version = "2", features = ["json", "cookies"] }
rustls = "0.23"
rustls-native-certs = "0.7"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
base64 = "0.22"
cookie_store = "0.21"
time = { version = "0.3", features = ["formatting", "parsing"] }
```

- [ ] **Step 2: 写最小 src/lib.rs**

```rust
pub const ABI_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 3;
```

- [ ] **Step 3: 编译验证**

Run: `cargo build --release 2>&1 | tail -5`
Expected: `Finished` 无错误；`file target/release/libworkbuddy.dylib` 显示 arm64 Mach-O 共享库。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: cargo skeleton with cdylib target"
```

### Task 2: C ABI 导出 + 宿主指针表

**Files:**
- Create: `src/cabi.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `#[no_mangle] pub extern "C" fn cliproxy_plugin_init / cliproxyPluginCall / cliproxyPluginFree / cliproxyPluginShutdown`；`fn host_call(method: &str, request: &[u8]) -> Result<Vec<u8>, String>`（全模块复用）；`fn host_log(level: &str, msg: &str)`。
- Consumes: dispatch 模块尚不存在时用占位分发（Task 3 替换）。

- [ ] **Step 1: 写 src/cabi.rs**

结构体布局必须与 Go 版 C 块逐字段一致：

```rust
use std::ffi::{c_char, c_void, CStr};
use std::sync::OnceLock;

pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CliproxyBuffer { pub ptr: *mut c_void, pub len: usize }

pub type HostCallFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut CliproxyBuffer) -> i32;
pub type HostFreeFn = unsafe extern "C" fn(*mut c_void, usize);
pub type PluginCallFn = unsafe extern "C" fn(*mut c_char, *mut u8, usize, *mut CliproxyBuffer) -> i32;
pub type PluginFreeFn = unsafe extern "C" fn(*mut c_void, usize);
pub type PluginShutdownFn = unsafe extern "C" fn();

#[repr(C)]
pub struct CliproxyHostApi { pub abi_version: u32, pub host_ctx: *mut c_void, pub call: Option<HostCallFn>, pub free_buffer: Option<HostFreeFn> }
#[repr(C)]
pub struct CliproxyPluginApi { pub abi_version: u32, pub call: Option<PluginCallFn>, pub free_buffer: Option<PluginFreeFn>, pub shutdown: Option<PluginShutdownFn> }

struct HostApi { ctx: *mut c_void, call: HostCallFn, free: HostFreeFn }
static HOST: OnceLock<HostApi> = OnceLock::new();

fn host() -> Option<&'static HostApi> { HOST.get() }

pub fn host_call(method: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    let h = host().ok_or("host API unavailable")?;
    let c_method = std::ffi::CString::new(method).map_err(|e| e.to_string())?;
    let mut resp = CliproxyBuffer { ptr: std::ptr::null_mut(), len: 0 };
    let (req_ptr, req_len) = if request.is_empty() { (std::ptr::null::<u8>(), 0usize) } else { (request.as_ptr(), request.len()) };
    let rc = unsafe { (h.call)(h.ctx, c_method.as_ptr(), req_ptr, req_len, &mut resp) };
    let out = unsafe { std::slice::from_raw_parts(resp.ptr as *const u8, resp.len) }.to_vec();
    unsafe { (h.free)(resp.ptr, resp.len) };
    if rc != 0 { return Err(format!("host call {method} returned {rc}")); }
    Ok(out)
}

pub fn host_log(level: &str, message: &str) {
    let body = serde_json::json!({ "level": level, "message": message });
    let _ = host_call("host.log", body.to_string().as_bytes());
}

#[no_mangle]
pub extern "C" fn cliproxy_plugin_init(host: *const CliproxyHostApi, plugin: *mut CliproxyPluginApi) -> i32 {
    if plugin.is_null() { return 1; }
    unsafe {
        let h = &*host;
        let _ = HOST.set(HostApi { ctx: h.host_ctx, call: h.call.expect("host call fn"), free: h.free_buffer.expect("host free fn") });
        (*plugin).abi_version = ABI_VERSION;
        (*plugin).call = Some(plugin_call_trampoline);
        (*plugin).free_buffer = Some(plugin_free_trampoline);
        (*plugin).shutdown = Some(plugin_shutdown_trampoline);
    }
    0
}

extern "C" fn plugin_call_trampoline(method: *mut c_char, request: *mut u8, request_len: usize, response: *mut CliproxyBuffer) -> i32 {
    if !response.is_null() { unsafe { (*response).ptr = std::ptr::null_mut(); (*response).len = 0; } }
    if method.is_null() {
        write_response(response, crate::rpc::error_envelope("invalid_method", "method is required").as_bytes());
        return 1;
    }
    let m = unsafe { CStr::from_ptr(method as *const c_char) }.to_string_lossy().into_owned();
    let req: &[u8] = if request.is_null() || request_len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(request, request_len) } };
    match crate::dispatch::handle(&m, req) {
        Ok(raw) => { write_response(response, &raw); 0 }
        Err(e) => { write_response(response, crate::rpc::error_envelope("plugin_error", &e).as_bytes()); 1 }
    }
}

extern "C" fn plugin_free_trampoline(ptr: *mut c_void, len: usize) {
    if !ptr.is_null() { unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len, len)); } }
}

extern "C" fn plugin_shutdown_trampoline() {}

fn write_response(response: *mut CliproxyBuffer, bytes: &[u8]) {
    if response.is_null() || bytes.is_empty() { return; }
    let mut v = bytes.to_vec();
    let ptr = v.as_mut_ptr() as *mut c_void;
    let len = v.len();
    std::mem::forget(v);
    unsafe { (*response).ptr = ptr; (*response).len = len; }
}
```

注意 `plugin_free_trampoline` 释放的内存必须来自 `write_response` 的 `Vec`（同分配器同长度），`len` 参数即写入时的 len。

- [ ] **Step 2: lib.rs 挂模块与占位分发**

```rust
pub mod cabi;
pub mod rpc;
pub mod dispatch;
```

`src/rpc.rs` 本任务先放两个最小函数（Task 3 扩全）：

```rust
pub fn error_envelope(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}
```

`src/dispatch.rs` 占位：

```rust
pub fn handle(_method: &str, _request: &[u8]) -> Result<Vec<u8>, String> {
    Ok(crate::rpc::error_envelope("unknown_method", "not implemented").into_bytes())
}
```

- [ ] **Step 3: 编译验证**

Run: `cargo build --release 2>&1 | tail -5 && nm -gU target/release/libworkbuddy.dylib | grep -c cliproxy`
Expected: 编译通过；`nm` 输出包含 4 个导出符号（grep 计数 ≥4）。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: C ABI exports and host pointer table"
```

### Task 3: rpc.rs 全量契约类型 + envelope 编解码（含单测）

**Files:**
- Modify: `src/rpc.rs`（完整替换占位）
- Modify: `src/lib.rs`（追加 `pub mod models;` 占位与测试声明）

**Interfaces:**
- Produces（后续任务依赖的精确签名）:
  - `pub fn ok_envelope<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, String>`
  - `pub fn error_envelope(code: &str, message: &str) -> String`
  - `pub fn parse_envelope(raw: &[u8]) -> Result<serde_json::Value, String>`（解宿主返回的 result）
  - `pub fn b64_decode(s: &str) -> Vec<u8>`、`pub fn b64_encode(b: &[u8]) -> String`
  - 类型：`Registration`, `Capabilities`, `Metadata`, `ModelInfo`, `ModelResponse`, `AuthData`, `AuthParseResponse`, `AuthLoginStartResponse`, `AuthLoginPollResponse`, `AuthRefreshResponse`, `ExecutorExecResponse`, `ExecutorStreamResponse`, `StreamChunk`, `ManagementRegistration`, `MgmtResponse`, `StoredAuth`, `StoredTokens`, `StoredAccount`, `CodeBuddyEnvelope<T>`, `TokenData`, `AccountData`
  - `pub fn auth_data_from_stored(sa: &StoredAuth) -> AuthData`

- [ ] **Step 1: 写类型定义（关键字段 serde rename）**

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Registration { #[serde(rename="schema_version")] pub schema_version: u32, pub metadata: Metadata, pub capabilities: Capabilities }

#[derive(Serialize)]
pub struct Metadata { #[serde(rename="Name")] pub name: String, #[serde(rename="Version")] pub version: String, #[serde(rename="Author")] pub author: String, #[serde(rename="GitHubRepository")] pub repo: String }

#[derive(Serialize)]
pub struct Capabilities { pub model_provider: bool, pub auth_provider: bool, pub executor: bool, #[serde(rename="executor_model_scope")] pub executor_model_scope: &'static str, #[serde(rename="executor_input_formats")] pub input_formats: Vec<&'static str>, #[serde(rename="executor_output_formats")] pub output_formats: Vec<&'static str>, #[serde(rename="management_api")] pub management_api: bool }

#[derive(Serialize, Clone)]
pub struct ModelInfo { #[serde(rename="ID")] pub id: String, #[serde(rename="Object")] pub object: String, #[serde(rename="OwnedBy")] pub owned_by: String, #[serde(rename="DisplayName")] pub display_name: String, #[serde(rename="Name")] pub name: String, #[serde(rename="SupportedGenerationMethods")] pub methods: Vec<String>, #[serde(rename="ContextLength")] pub context_length: i64, #[serde(rename="MaxCompletionTokens")] pub max_completion_tokens: i64, #[serde(rename="UserDefined")] pub user_defined: bool }

#[derive(Serialize)]
pub struct ModelResponse { #[serde(rename="Provider")] pub provider: String, #[serde(rename="Models")] pub models: Vec<ModelInfo> }

#[derive(Serialize, Clone)]
pub struct AuthData { #[serde(rename="Provider")] pub provider: String, #[serde(rename="ID")] pub id: String, #[serde(rename="FileName")] pub file_name: String, #[serde(rename="Label")] pub label: String, #[serde(rename="StorageJSON")] pub storage_json: String, #[serde(rename="Metadata")] pub metadata: serde_json::Value }

#[derive(Serialize)]
pub struct AuthParseResponse { #[serde(rename="Handled")] pub handled: bool, #[serde(rename="Auth")] #[serde(skip_serializing_if="Option::is_none")] pub auth: Option<AuthData> }

#[derive(Serialize)]
pub struct AuthLoginStartResponse { #[serde(rename="Provider")] pub provider: String, #[serde(rename="URL")] pub url: String, #[serde(rename="State")] pub state: String, #[serde(rename="ExpiresAt")] pub expires_at: String }

#[derive(Serialize)]
pub struct AuthLoginPollResponse { #[serde(rename="Status")] pub status: &'static str, #[serde(rename="Message")] pub message: String, #[serde(rename="Auth")] #[serde(skip_serializing_if="Option::is_none")] pub auth: Option<AuthData> }

#[derive(Serialize)]
pub struct AuthRefreshResponse { #[serde(rename="Auth")] pub auth: AuthData }

#[derive(Serialize)]
pub struct ExecutorExecResponse { #[serde(rename="Payload")] pub payload: String, #[serde(rename="Headers")] #[serde(skip_serializing_if="Option::is_none")] pub headers: Option<std::collections::HashMap<String, Vec<String>>> }

#[derive(Serialize)]
pub struct StreamChunk { #[serde(rename="Payload")] pub payload: String }

#[derive(Serialize)]
pub struct ExecutorStreamResponse { #[serde(rename="Headers")] pub headers: std::collections::HashMap<String, Vec<String>>, #[serde(rename="chunks")] #[serde(skip_serializing_if="Vec::is_empty")] pub chunks: Vec<StreamChunk> }

#[derive(Serialize)]
pub struct ManagementRoute { pub method: &'static str, pub path: String, #[serde(skip_serializing_if="String::is_empty")] pub description: String }

#[derive(Serialize)]
pub struct ResourceRoute { pub path: String, #[serde(skip_serializing_if="String::is_empty")] pub menu: String, #[serde(skip_serializing_if="String::is_empty")] pub description: String }

#[derive(Serialize)]
pub struct ManagementRegistration { #[serde(rename="routes")] pub routes: Vec<ManagementRoute>, #[serde(rename="resources")] pub resources: Vec<ResourceRoute> }

#[derive(Serialize)]
pub struct MgmtResponse { #[serde(rename="StatusCode")] pub status_code: u16, #[serde(rename="Headers")] pub headers: std::collections::HashMap<String, Vec<String>>, #[serde(rename="Body")] pub body: String }

// 凭据磁盘结构（camelCase，与 Go 版逐字一致）
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredAuth { pub auth: StoredTokens, pub account: StoredAccount }
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredTokens { #[serde(rename="accessToken")] pub access_token: String, #[serde(rename="refreshToken")] pub refresh_token: String, #[serde(rename="expiresAt")] pub expires_at: i64, pub domain: String }
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredAccount { pub uid: String, #[serde(rename="enterpriseId")] pub enterprise_id: String, pub nickname: String }

#[derive(Deserialize)]
pub struct CodeBuddyEnvelope<T> { pub code: i64, pub msg: String, pub data: Option<T> }

#[derive(Deserialize)]
pub struct TokenData { #[serde(rename="accessToken")] pub access_token: String, #[serde(rename="refreshToken")] pub refresh_token: String, #[serde(rename="expiresIn")] pub expires_in: i64, #[serde(rename="refreshExpiresIn")] pub refresh_expires_in: i64, pub domain: String }

#[derive(Deserialize, Default)]
pub struct AccountData { pub uid: String, #[serde(rename="enterpriseId")] pub enterprise_id: String, pub nickname: String }

pub fn auth_data_from_stored(sa: &StoredAuth) -> AuthData {
    AuthData { provider: "workbuddy".into(), id: "workbuddy".into(), file_name: "workbuddy.json".into(), label: "WorkBuddy".into(), storage_json: b64_encode(&serde_json::to_vec(sa).unwrap_or_default()), metadata: serde_json::json!({"type": "workbuddy"}) }
}

pub fn b64_encode(b: &[u8]) -> String { use base64::Engine as _; base64::engine::general_purpose::STANDARD.encode(b) }
pub fn b64_decode(s: &str) -> Vec<u8> { use base64::Engine as _; base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default() }

pub fn ok_envelope<T: Serialize>(v: &T) -> Result<Vec<u8>, String> {
    let result = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    serde_json::to_vec(&serde_json::json!({"ok": true, "result": serde_json::from_slice::<serde_json::Value>(&result).unwrap()})).map_err(|e| e.to_string())
}

pub fn error_envelope(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}

pub fn parse_envelope(raw: &[u8]) -> Result<serde_json::Value, String> {
    let v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if v["ok"].as_bool() != Some(true) { return Err("host call returned error envelope".into()); }
    Ok(v["result"].clone())
}
```

`Metadata` 注意：Go 无 tag 字段名精确匹配，`GitHubRepository` 必须 rename 原样（不是 PascalCase 转换的 `GithubRepository`）。

- [ ] **Step 2: 写单测（rpc.rs 内 #[cfg(test)]）**

```rust
#[test] fn registration_json_keys() {
    let caps = Capabilities { model_provider: true, auth_provider: true, executor: true, executor_model_scope: "both", input_formats: vec!["chat-completions"], output_formats: vec!["chat-completions"], management_api: true };
    let r = Registration { schema_version: 3, metadata: Metadata { name: "workbuddy".into(), version: "0.2.0".into(), author: "x".into(), repo: "https://example.invalid".into() }, capabilities: caps };
    let env = ok_envelope(&r).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["schema_version"], 3);
    assert_eq!(v["result"]["capabilities"]["executor_model_scope"], "both");
    assert_eq!(v["result"]["capabilities"]["management_api"], true);
    assert!(v["result"]["capabilities"].get("model_registrar").is_none());
}
#[test] fn storage_json_roundtrip() {
    let sa = StoredAuth { auth: StoredTokens { access_token: "a".into(), refresh_token: "r".into(), expires_at: 1, domain: "d".into() }, account: StoredAccount { uid: "u".into(), enterprise_id: "e".into(), nickname: "n".into() } };
    let ad = auth_data_from_stored(&sa);
    assert_eq!(b64_decode(&ad.storage_json), serde_json::to_vec(&sa).unwrap());
    assert_eq!(ad.file_name, "workbuddy.json");
    let back: StoredAuth = serde_json::from_slice(&b64_decode(&ad.storage_json)).unwrap();
    assert_eq!(back.auth.access_token, "a");
}
#[test] fn parse_envelope_error() { assert!(parse_envelope(b"{\"ok\":false}").is_err()); }
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -8`
Expected: 3 个测试全 PASS。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: full RPC contract types with envelope codec"
```

### Task 4: models.rs 内置模型表 + registration + 配置覆盖

**Files:**
- Create: `src/models.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `rpc::{Registration, ModelInfo, ModelResponse, Metadata}`。
- Produces: `pub fn default_registration() -> Registration`；`pub fn builtin_models() -> Vec<ModelInfo>`；`pub fn merge_model_overrides(base: Vec<ModelInfo>, config_yaml: &[u8]) -> Vec<ModelInfo>`（`config_yaml` 内 `models:` 列表：按 `id` 替换/追加，字段 `id,name,context` 可选）。

- [ ] **Step 1: 写内置表与合并逻辑**

内置表（与 Go 版 `wbModels()` 逐项一致，含 glm-5.3-flash；`MaxCompletionTokens` 一律 8192；`Object:"model"`、`OwnedBy:"workbuddy"`、`SupportedGenerationMethods:["chat"]`、`UserDefined:true`）：

```rust
pub fn builtin_models() -> Vec<ModelInfo> {
    let specs: &[(&str, &str, i64)] = &[
        ("glm-5.2", "GLM-5.2", 1_000_000),
        ("glm-5.3-flash", "GLM-5.3 Flash", 1_000_000),
        ("glm-5.1", "GLM-5.1", 131_072),
        ("glm-5v-turbo", "GLM-5V Turbo", 131_072),
        ("kimi-k2.7", "Kimi K2.7", 262_144),
        ("minimax-m3-pay", "MiniMax M3", 204_800),
        ("hy3", "Hy3", 262_144),
        ("hy3-preview", "Hy3 Preview", 262_144),
        ("hy3-preview-agent", "Hy3 Preview Agent", 262_144),
        ("deepseek-v4-pro", "DeepSeek V4 Pro", 1_000_000),
        ("deepseek-v4-flash", "DeepSeek V4 Flash", 1_000_000),
    ];
    specs.iter().map(|(id, name, ctx)| ModelInfo {
        id: id.to_string(), object: "model".into(), owned_by: "workbuddy".into(),
        display_name: name.to_string(), name: id.to_string(),
        methods: vec!["chat".into()], context_length: *ctx, max_completion_tokens: 8192, user_defined: true,
    }).collect()
}
```

配置覆盖用 `#[derive(Deserialize)] struct ModelOverride { id: String, #[serde(default)] name: Option<String>, #[serde(default)] context: Option<i64> }`；从 `serde_yaml::from_slice::<serde_yaml::Value>(config_yaml)` 取 `plugins` 不存在——宿主注入的 config_yaml 是该插件实例自身的 YAML（即 `enabled/priority/models` 平级），解析 `models` 键。

- [ ] **Step 2: 单测**

```rust
#[test] fn merge_override_replaces_context_and_appends_new() {
    let base = builtin_models();
    let yaml = b"enabled: true\nmodels:\n  - id: glm-5.3-flash\n    context: 2000000\n  - id: kimi-k3\n    name: Kimi K3\n";
    let merged = merge_model_overrides(base, yaml);
    let f = merged.iter().find(|m| m.id == "glm-5.3-flash").unwrap();
    assert_eq!(f.context_length, 2_000_000);
    assert!(merged.iter().any(|m| m.id == "kimi-k3"));
    assert_eq!(merged.len(), 12);
}
#[test] fn bad_yaml_falls_back() {
    let base = builtin_models();
    let merged = merge_model_overrides(base.clone(), b"\t-bad:[");
    assert_eq!(merged.len(), base.len());
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -5`
Expected: PASS（累计 5 个测试）。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: builtin model table, registration, config overrides"
```

### Task 5: upstream.rs HTTP 栈 + 公共头

**Files:**
- Create: `src/upstream.rs`

**Interfaces:**
- Produces:
  - `pub fn shared_agent() -> ureq::Agent`（OnceLock；rustls + native certs；cookie store 启用）
  - `pub fn new_login_agent() -> ureq::Agent`（独立 cookie jar）
  - `pub fn common_headers(req: &mut ureq::Request)`：`Content-Type: application/json`、`Accept: application/json, text/plain, */*`、`X-Requested-With: XMLHttpRequest`、`Origin: https://www.codebuddy.cn`、`Referer: https://www.codebuddy.cn/`、`User-Agent: CLI/2.63.2 CodeBuddy/2.63.2`
  - `pub fn backend_headers(req: &mut ureq::Request, sa: &StoredAuth)`：common + `Authorization: Bearer <token>`（空则 `X-No-Authorization: 1`）、`X-User-Id`（空则 `X-No-User-Id: 1`）、`X-Enterprise-Id`（空则 `X-No-Enterprise-Id: 1`）、`X-Refresh-Token`、`X-Domain`（空则 `X-No-Department-Info: 1`）、`X-Product: SaaS`
  - `pub fn post_json(agent, url, headers: Option<impl Fn(&mut ureq::Request)>, body: &str) -> Result<(serde_json::Value, u16), UpstreamError>`：解析 `{code,msg,data}`，`code != 0` → `UpstreamError::Api(code,msg)`，HTTP ≥400 → `UpstreamError::Http(status)`
  - `pub enum UpstreamError { Http(u16), Api(i64, String), Transport(String) }`（Display 实现）

ureq 2.x cookie 支持在 Agent 构建时用 `AgentBuilder::new().cookie_store(cookie_store::CookieStore::new(None, ...))`——注意 cookie_store 需要包 `Arc<RwLock<>>`；构建失败用 `std::sync::OnceLock` 懒加载一次。TLS 根证书：`rustls_native_certs::load_native_certs()` 喂给 `rustls::ClientConfig`，构建 `AgentBuilder::new().tls_connector(...)`。

- [ ] **Step 1: 实现（代码要点如上，模块级单测验证 header 组装）**

单测不发起真实请求，只验证 `backend_headers` 对空/非空字段的 X-No-* 分支：把 header 组装拆成 `apply_common(req: &mut RequestParts)` 级别的纯函数或对 `ureq::Request` 的检查（ureq Request 无只读 header 枚举 → 用内部 `HeaderMap` 结构先行组装再落到 Request；实现方式：定义 `struct HeaderSet(Vec<(String,String)>)`，`backend_header_set(sa) -> HeaderSet`，单测直接测 HeaderSet，发请求时循环 `req.set(k,v)`）。

```rust
fn empty_account_stored_auth() -> StoredAuth {
    StoredAuth { auth: StoredTokens { access_token: "testtoken".into(), refresh_token: "r".into(), expires_at: 1, domain: "".into() }, account: StoredAccount { uid: "".into(), enterprise_id: "".into(), nickname: "".into() } }
}
#[test] fn backend_headers_empty_fields_use_no_headers() {
    let hs = backend_header_set(&empty_account_stored_auth());
    assert!(hs.0.iter().any(|(k,v)| k=="X-No-User-Id" && v=="1"));
    assert!(hs.0.iter().any(|(k,v)| k=="Authorization" && v=="Bearer testtoken"));
    assert!(!hs.0.iter().any(|(k,_)| k=="X-User-Id"));
}
```

- [ ] **Step 2: 真实 TLS 冒烟（手动一步）**

Run: `cargo test --release -- --ignored tls_smoke`（测试内容：`shared_agent().post("https://copilot.tencent.com/v2/plugin/auth/state")...` 发 `{}，期望 HTTP 200 或 4xx，绝不能 TLS 握手错误）
Expected: 非 TLS 错误即通过（说明根证书加载正确）。

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: ureq+rustls HTTP stack with CodeBuddy header sets"
```

### Task 6: auth.rs 登录/轮询/刷新

**Files:**
- Create: `src/auth.rs`

**Interfaces:**
- Consumes: `rpc::{StoredAuth, TokenData, AccountData, AuthData, auth_data_from_stored}`、`upstream::{...}`、`cabi::host_call` 不需要。
- Produces:
  - `pub fn start_login() -> Result<rpc::AuthLoginStartResponse, String>`：POST `/v2/plugin/auth/state?platform=CLI` body `{}`（独立 login agent）；从 data 取 `state`/`authUrl`；存入 `LOGIN_STATES: Mutex<HashMap<String,(ureq::Agent,i64 expires_at_unix)>>`（TTL 300s）。
  - `pub fn poll_login(state: &str) -> Result<rpc::AuthLoginPollResponse, String>`：未知/过期 state → Err("poll: unknown state (restart login)")/Err("poll: login expired")；GET `auth/token?state=`（同 agent）→ 失败或无 accessToken 返回 pending；成功后 GET `login/account?state=`（带 Bearer，失败容忍）组装 StoredAuth → success + Auth。
  - `pub fn refresh(storage_json_b64: &str) -> Result<rpc::AuthRefreshResponse, String>`：解码 StoredAuth → POST `/v2/plugin/auth/token/refresh`（shared agent；headers 用 `backend_header_set` 变体：X-Refresh-Token + X-Enterprise-Id + `X-Auth-Refresh-Source: workbuddy`）→ 更新 token/expiresAt/domain → AuthRefreshResponse。
  - `pub fn parse_auth(raw_json_b64: &str) -> rpc::AuthParseResponse`：解码解析失败或 accessToken 空 → `Handled:false`。

- [ ] **Step 1: 实现**（state 表 TTL 300 秒；`ExpiresAt` 输出格式：`time::OffsetDateTime::now_utc().replace_minute(0)...` 不必精确到分——直接 `(now + 300s).format(&Rfc3339)`）

- [ ] **Step 2: 单测**

```rust
#[test] fn poll_unknown_state_errors() { assert!(poll_login("nope").is_err()); }
#[test] fn parse_auth_rejects_garbage() { assert_eq!(parse_auth(&b64_encode(b"not json")).handled, false); }
#[test] fn parse_auth_accepts_stored() { /* 构造 StoredAuth → b64 → Handled=true 且 Auth.StorageJSON 解回同值 */ }
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: auth login/poll/refresh state machine"
```

### Task 7: executor.rs 执行 + SSE + 后台流泵

**Files:**
- Create: `src/executor.rs`

**Interfaces:**
- Consumes: `rpc::{StoredAuth, ExecutorExecResponse, ExecutorStreamResponse, StreamChunk}`、`upstream::backend_header_set`、`cabi::host_call`。
- Produces:
  - `pub fn sanitize_request(body: &[u8]) -> Vec<u8>`：JSON 解析成功则 ① `stream=true` ② 系统提示词改写（Go 版 `rewriteSystemForUpstream` 两处单字替换 + `reasoning_effort` 对 `hy3*` 前缀强制 `"high"`）③ 重新序列化；失败原样返回。
  - `pub fn sse_framed_for_path(metadata: &serde_json::Value) -> bool`：`metadata["request_path"]` ∈ {`/v1/chat/completions`,`/v1/completions`} → false，否则 true。
  - `pub fn clean_chunk_json(s: &str) -> String`：choices[].delta 剔除空值字段（null/""/[]/{}）；解析失败原样返回。
  - `pub fn execute(req: &ExecReq) -> Result<rpc::ExecutorExecResponse, String>`：强制 stream → POST chat/completions → 聚合成单个 `chat.completion` 对象（Go 版 `aggregateCompletion`：id/model/created/choices[0].message{role,content,reasoning_content,tool_calls}/usage/finish_reason）。
  - `pub fn execute_stream(req: &ExecReq) -> Result<rpc::ExecutorStreamResponse, String>`：无 `stream_id` → 同步收集 chunks（每 chunk：`stripDataPrefix` + `clean_chunk_json` + 可选 `data: ` 前帧）；有 → `std::thread::spawn` 泵线程循环 emit/close（emit 失败 break），立即返回空 chunks + `text/event-stream` 头。
  - `pub struct ExecReq { pub model: String, pub payload_b64: String, pub original_b64: String, pub storage_b64: String, pub metadata: serde_json::Value, pub stream_id: String }`（dispatch 从 RPC 请求构造）。

- [ ] **Step 1: 实现**（照 Go 版 main.go 679-1110 行逐函数移植；JSON 字符串替换的两条 rewrite 常量从 Go 版源码复制原文）

```rust
// 两条改写（Go 版 sanitizeBlockedTemplates 逐字）：保持原文不变，此处在实现时从
// ../workbuddy-cliproxy/main.go 996-1004 行复制精确字符串常量（避免计划转写引入偏差）。
```

- [ ] **Step 2: 单测**

```rust
#[test] fn sanitize_forces_stream_true() {
    let out = sanitize_request(b"{\"model\":\"glm-5.2\",\"stream\":false}");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["stream"], true);
}
#[test] fn sanitize_pins_hy3_reasoning_high() {
    let out = sanitize_request(b"{\"model\":\"hy3-preview\",\"reasoning_effort\":\"low\",\"messages\":[]}");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["reasoning_effort"], "high");
}
#[test] fn sanitize_leaves_non_hy3_effort() {
    let out = sanitize_request(b"{\"model\":\"glm-5.2\",\"reasoning_effort\":\"low\",\"messages\":[]}");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["reasoning_effort"], "low");
}
#[test] fn clean_chunk_strips_empty_delta_fields() {
    let c = clean_chunk_json(r#"{"choices":[{"delta":{"content":"hi","tool_calls":[],"role":null}}]}"#);
    let v: serde_json::Value = serde_json::from_str(&c).unwrap();
    assert!(v["choices"][0]["delta"].get("tool_calls").is_none());
    assert!(v["choices"][0]["delta"].get("role").is_none());
    assert_eq!(v["choices"][0]["delta"]["content"], "hi");
}
#[test] fn sse_frame_detection() {
    let mut m = serde_json::Map::new(); m.insert("request_path".into(), serde_json::json!("/v1/chat/completions"));
    assert!(!sse_framed_for_path(&serde_json::Value::Object(m.clone())));
    m.insert("request_path".into(), serde_json::json!("/v1/messages"));
    assert!(sse_framed_for_path(&serde_json::Value::Object(m)));
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: executor with SSE pump, chunk cleaning, prompt rewrites"
```

### Task 8: billing.rs + management.rs + 面板嵌入

**Files:**
- Create: `src/billing.rs`、`src/management.rs`
- Copy: `panel.html` → 仓库根（已存在）

**Interfaces:**
- Consumes: `upstream`、`rpc::StoredAuth`、`cabi::host_call`（host.auth.list / host.auth.get）。
- Produces:
  - `pub fn fetch_credits(sa: &StoredAuth) -> serde_json::Value`：POST `/v2/billing/meter/get-user-resource` body `{}`（backend headers）；解析 `data.Response.Data.Accounts[]` 的 `PackageName/CapacityType/CapacityRemain/CapacityUsed/CapacitySize/CycleStartTime/CycleEndTime`；输出 `{"total_remain","total_used","total_size","pack_count","packages":[...],"fetched_at","error"?}`（与 Go 版 management.go 相同 JSON 形状；`fetched_at` RFC3339 UTC）。
  - `pub fn registration() -> rpc::ManagementRegistration`：routes = GET `/plugins/workbuddy/accounts`、POST `/plugins/workbuddy/refresh`；resources = `/panel` menu `WorkBuddy`。
  - `pub fn handle(req_method: &str, req_path: &str, req_body_b64: &str) -> rpc::MgmtResponse`：
    - GET `/v0/resource/plugins/workbuddy` 或 +`/`、`/panel`、`/panel.html` → 200 `text/html` `include_str!("../panel.html")`；其他 sub → 404。
    - GET `/v0/management/plugins/workbuddy/accounts` → 200 `{"accounts":[{auth_index,name,nickname,credits}],"fetched_at"}`（经 host.auth.list 过滤 `provider/type == workbuddy`，逐个 host.auth.get 解析凭据调 fetch_credits；错误写入 `credits.error`）。
    - POST `/v0/management/plugins/workbuddy/refresh` → 同 accounts。
    - 其他 → 404 `{"error":"not found: <path>"}`。

- [ ] **Step 1: 实现**（include_str! 嵌面板；MgmtResponse.Headers 固定 `Content-Type: application/json; charset=utf-8` 或 `text/html; charset=utf-8`）

- [ ] **Step 2: 单测**

```rust
#[test] fn registration_routes_shape() {
    let r = registration();
    assert_eq!(r.routes[0].path, "/plugins/workbuddy/accounts");
    assert_eq!(r.routes[0].method, "GET");
    assert_eq!(r.resources[0].menu, "WorkBuddy");
}
#[test] fn panel_served_and_404() {
    let ok = handle("GET", "/v0/resource/plugins/workbuddy/panel", "");
    assert_eq!(ok.status_code, 200);
    let bad = handle("GET", "/v0/resource/plugins/workbuddy/nope", "");
    assert_eq!(bad.status_code, 404);
}
#[test] fn mgmt_unknown_path_404() {
    assert_eq!(handle("GET", "/v0/management/plugins/workbuddy/zzz", "").status_code, 404);
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: billing credits, management routes, embedded panel"
```

### Task 9: dispatch.rs 全量分发 + 热加载 config

**Files:**
- Create: `src/dispatch.rs`（替换占位）
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: Task 4-8 全部Produces。
- Produces: `pub fn handle(method: &str, request: &[u8]) -> Result<Vec<u8>, String>` 覆盖：`plugin.register`/`plugin.reconfigure`（reconfigure 解析 `config_yaml` base64 → `Mutex<Vec<ModelInfo>>` 热替换）、`model.static`/`model.for_auth`（返回热表）、`auth.*`、`executor.*`、`management.register`/`management.handle`；未知方法 → error envelope（返回 Ok(error_envelope bytes)，与 Go 版一致 rc=0）。

- [ ] **Step 1: 实现**（请求字段解码：`auth.parse` 的 `raw_json`、`auth.refresh`/executor 的 `storage_json`/`payload`/`original_request` 均 base64；executor 请求体平铺 `stream_id`/`host_callback_id`；`metadata` 为对象）

- [ ] **Step 2: 单测**

```rust
#[test] fn unknown_method_returns_error_envelope_rc0() {
    let raw = handle("nope.nope", b"{}").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unknown_method");
}
#[test] fn model_static_lists_builtin() {
    let raw = handle("model.static", b"{}").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["result"]["Provider"], "workbuddy");
    assert!(v["result"]["Models"].as_array().unwrap().len() >= 11);
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test --release 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: RPC dispatch with hot model reload"
```

### Task 10: 隔离实例端到端验收（对齐 Go 版行为）

**Files:**
- Create: `scripts/e2e-check.sh`（可重复执行的验证脚本）

**Interfaces:**
- Consumes: Task 1-9 全部完成；本机 `~/.cli-proxy-api/workbuddy.json` 真实凭据（只读）。
- Produces: 全 RPC 实测通过的验收记录（命令输出即证据）。

- [ ] **Step 1: 安装测试**

```bash
cp target/release/libworkbuddy.dylib ~/.cli-proxy-api/plugins/workbuddy.dylib   # 先备份原 Go 版 dylib 为 workbuddy.dylib.go-backup
```

- [ ] **Step 2: 启动隔离实例**

```bash
sed -e 's/^port: 8317/port: 8399/' /opt/homebrew/etc/cliproxyapi.conf > /tmp/cpa-test.conf
python3 -c "import re;p='/tmp/cpa-test.conf';s=open(p).read();open(p,'w').write(re.sub(r'secret-key: \"[^\"]*\"','secret-key: \"testkey123\"',s))"
/opt/homebrew/opt/cliproxyapi/bin/cliproxyapi -config /tmp/cpa-test.conf > /tmp/cpa-test.log 2>&1 &
sleep 4; grep -c "plugin registered plugin_id=workbuddy" /tmp/cpa-test.log   # 期望 1
```

- [ ] **Step 3: 逐 RPC 验收（e2e-check.sh 内容，逐项断言）**

```bash
#!/usr/bin/env bash
set -e
B="http://127.0.0.1:8399"; K="Authorization: Bearer testkey123"
# 1) 模型列表 ≥11 且含 glm-5.3-flash
curl -s $B/v1/models -H "$K" | python3 -c "import json,sys;d=[m['id'] for m in json.load(sys.stdin)['data'] if m.get('owned_by')=='workbuddy'];assert 'glm-5.3-flash' in d and len(d)>=11, d;print('models OK',len(d))"
# 2) 管理额度
curl -s $B/v0/management/plugins/workbuddy/accounts -H "$K" | python3 -c "import json,sys;d=json.load(sys.stdin);c=d['accounts'][0]['credits'];assert c['pack_count']>=4 and 'error' not in c or not c.get('error'),c;print('credits OK',c['total_remain'],'/',c['total_size'])"
# 3) 面板
curl -sf $B/v0/resource/plugins/workbuddy/panel | grep -q "总积分额度" && echo "panel OK"
# 4) 非流式对话
curl -s $B/v1/chat/completions -H "$K" -H 'Content-Type: application/json' -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"只回复两个字：收到"}],"max_tokens":2048,"stream":false}' | python3 -c "import json,sys;r=json.load(sys.stdin);assert r['choices'][0]['message']['content'].strip(),r;print('chat OK')"
# 5) 流式对话（Anthropic 入口路径触发 SSE 帧）
curl -s $B/v1/messages -H "$K" -H 'Content-Type: application/json' -H 'anthropic-version: 2023-06-01' -d '{"model":"glm-5.3-flash","max_tokens":2048,"stream":true,"messages":[{"role":"user","content":"只回复两个字：收到"}]}' | head -c 200 | grep -q "event\|data" && echo "stream OK"
```

- [ ] **Step 4: 运行并清场**

Run: `bash scripts/e2e-check.sh && pkill -f "cliproxyapi -config /tmp/cpa-test.conf" && rm -f /tmp/cpa-test.conf /tmp/cpa-test.log`
Expected: 四个 OK 输出；主服务 `curl 127.0.0.1:8317/v1/models` 仍 200。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test: e2e verification script, Rust plugin passes all RPC checks"
```

### Task 11: CI 六平台发布

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `README.md`（重写：Rust 版安装三选一、无宿主 Go 依赖说明、CPA 版本兼容声明）

**Interfaces:**
- Produces: tag `v*` push → Release 产物 `workbuddy_<ver>_<goos>_<goarch>.zip`（根目录 `workbuddy.dylib|so|dll`）+ `checksums.txt`。

- [ ] **Step 1: 写 workflow**

```yaml
name: release
on:
  push:
    tags: ["v*"]
permissions: { contents: write }
jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: macos-14, target: aarch64-apple-darwin, goos: darwin, goarch: arm64, ext: dylib }
          - { os: macos-15, target: x86_64-apple-darwin, goos: darwin, goarch: amd64, ext: dylib }
          - { os: ubuntu-22.04, target: x86_64-unknown-linux-gnu, goos: linux, goarch: amd64, ext: so }
          - { os: ubuntu-24.04-arm, target: aarch64-unknown-linux-gnu, goos: linux, goarch: arm64, ext: so }
          - { os: windows-2022, target: x86_64-pc-windows-msvc, goos: windows, goarch: amd64, ext: dll }
          - { os: windows-2022, target: aarch64-pc-windows-msvc, goos: windows, goarch: arm64, ext: dll }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: "${{ matrix.target }}" }
      - run: cargo build --release --target ${{ matrix.target }}
      - run: |
          VER="${GITHUB_REF_NAME#v}"
          ZIP="workbuddy_${VER}_${{ matrix.goos }}_${{ matrix.goarch }}.zip"
          mkdir -p pkg
          cp "target/${{ matrix.target }}/release/libworkbuddy.${{ matrix.ext }}" "pkg/workbuddy.${{ matrix.ext }}" 2>/dev/null || cp "target/${{ matrix.target }}/release/workbuddy.${{ matrix.ext }}" "pkg/workbuddy.${{ matrix.ext }}"
          (cd pkg && zip "../$ZIP" *)
          echo "ZIP=$ZIP" >> $GITHUB_ENV
        shell: bash
      - uses: softprops/action-gh-release@v2
        with: { files: "${{ env.ZIP }}" }
  checksums:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4  # build job 需先 upload-artifact 各 zip
      - run: sha256sum workbuddy_*.zip > checksums.txt
      - uses: softprops/action-gh-release@v2
        with: { files: "checksums.txt" }
```

build job 需在 release 步骤前加 `- uses: actions/upload-artifact@v4 with: { name: ${{ env.ZIP }}, path: ${{ env.ZIP }} }`；checksums job 下载全部产物生成单一 checksums.txt 上传到同一 Release。（末端产物：Release 含 6 个 zip + checksums.txt。）

- [ ] **Step 2: 本地验证 zip 命名逻辑**（模拟一次打包，确认文件名/根目录结构符合 `internal/pluginstore.ArchiveName`）

- [ ] **Step 3: 打测试 tag 验证 CI**

```bash
git tag v0.2.0-rust && git push origin v0.2.0-rust
# 仓库需先 push 到 GitHub（git remote add origin git@github.com:<user>/workbuddy-cpa.git）
```
Expected: Actions 全绿（windows/arm64 允许单独重试），Release 出现 6 个 zip + checksums.txt。

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "ci: six-platform release pipeline and README"
```

### Task 12: 切换与收尾

**Files:**
- Modify: 本机安装（非仓库文件）
- Modify: 旧仓库 `../workbuddy-cliproxy/README.md`（顶部加"已由 workbuddy-cpa 取代"横幅 + 链接）

- [ ] **Step 1: 主服务切 Rust 版**

```bash
cp ~/.cli-proxy-api/plugins/workbuddy.dylib.go-backup /tmp/ 2>/dev/null || true
cp target/release/libworkbuddy.dylib ~/.cli-proxy-api/plugins/workbuddy.dylib
brew services restart cliproxyapi && sleep 4
curl -s http://127.0.0.1:8317/v1/models -H "Authorization: Bearer sk-Cj9KaR4VJoG5nqCzk" | grep -c "workbuddy"   # ≥11
```

- [ ] **Step 2: 热加载模型验证**

在 `/opt/homebrew/etc/cliproxyapi.conf` 的 `workbuddy:` 下加 `models: [{id: kimi-k3, name: "Kimi K3"}]` → `brew services restart cliproxyapi` → `/v1/models` 出现 `kimi-k3`（owned_by workbuddy）→ 撤掉该配置行再重启（恢复 11 个）。验证完毕配置还原。

- [ ] **Step 3: 旧仓库退役横幅 + Commit**

```bash
cd ../workbuddy-cliproxy
# README 顶部插入："> 本仓库已由 Rust 重写版取代：https://github.com/<user>/workbuddy-cpa （更小产物、无宿主 Go 版本依赖、预编译六平台发布）"
git add README.md && git commit -m "docs: point to Rust rewrite at workbuddy-cpa" --no-verify
```



