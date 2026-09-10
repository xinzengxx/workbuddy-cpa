# 多账号登录与配额管理实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** workbuddy-cpa 插件支持多账号登录（每账号独立凭据文件）、scheduler.pick 额度感知加权随机调度、面板 toggle 启停账号与上下两区配额面板。

**Architecture:** 登录时凭据文件名改为 `workbuddy-{uid后6位}.json`；新增 `src/scheduler.rs` 实现宿主 `scheduler.pick` 钩子（额度缓存 TTL 60s + 加权随机 + 乐观扣减）；`management.rs` 增加 toggle API 与 `workbuddy-state.json` 持久化；`panel.html` 重构为上下两区。

**Tech Stack:** Rust stable、serde_json、rand（新增，加权随机）、现有 ureq/rustls 栈。

**Spec:** `docs/superpowers/specs/2026-09-10-multi-account-design.md`

## Global Constraints

- envelope 与字段名规则沿用 v0.2.0 契约：宿主无 tag Go 结构体字段 = 导出名原样（PascalCase，如 `StorageJSON`）；带 tag 的用 tag（`auth_index` 等）；`[]byte` = base64。
- **scheduler.pick 无 rpc 包装**：宿主 `rpcPluginAdapter.Pick` 直接 marshal `pluginapi.SchedulerPickRequest`（`rpc_client.go:369`），JSON 键为 `Provider/Providers/Model/Stream/Options/Candidates`（PascalCase 原样），候选内字段 `ID/Provider/Priority/Status/Attributes/Metadata`；响应键 `AuthID/DelegateBuiltin/Handled`（大小写不敏感读取，写出用 PascalCase）。
- pick 永不发起同步网络请求；缓存过期走异步刷新；全部候选不可用时 `Handled:false` 交还宿主。
- 停用名单持久化：`~/.cli-proxy-api/workbuddy-state.json`（插件自管理，结构 `{"disabled":["<auth_index>",...]}`），toggle 即时写、启动/pick 时读。
- 上游常量、e2e 隔离实例（端口 8399、api key `sk-Cj9KaR4VJoG5nqCzk`、管理密钥 `testkey123`）与 v0.2 计划一致；测试实例用完 pkill。
- 版本号目标 `0.3.0`；CI 六平台 tag `v0.3.0` 发布。

---

### Task 1: 登录凭据文件名独立化

**Files:**
- Modify: `src/rpc.rs`（`auth_data_from_stored` 签名）
- Modify: `src/auth.rs`（poll_login 调用处）

**Interfaces:**
- Produces: `pub fn auth_data_from_stored(sa: &StoredAuth, file_name: &str) -> AuthData`；`pub fn credential_file_name(sa: &StoredAuth) -> String`（uid 后 6 位不足补全/为空回退 `workbuddy.json`）。

- [ ] **Step 1: 改签名与实现**

`src/rpc.rs`：

```rust
pub fn credential_file_name(sa: &StoredAuth) -> String {
    let uid = sa.account.uid.trim();
    if uid.is_empty() { return "workbuddy.json".into(); }
    let chars: Vec<char> = uid.chars().collect();
    let tail: String = if chars.len() >= 6 { chars[chars.len()-6..].iter().collect() } else { uid.to_string() };
    format!("workbuddy-{tail}.json")
}

pub fn auth_data_from_stored(sa: &StoredAuth, file_name: &str) -> AuthData {
    AuthData { provider: "workbuddy".into(), id: "workbuddy".into(), file_name: file_name.into(), label: "WorkBuddy".into(), storage_json: b64_encode(&serde_json::to_vec(sa).unwrap_or_default()), metadata: serde_json::json!({"type": "workbuddy"}) }
}
```

- [ ] **Step 2: 更新调用点与既有测试**

`src/auth.rs`：`poll_login` 成功路径改 `auth_data_from_stored(&sa, &credential_file_name(&sa))`；`parse_auth` 保持 `auth_data_from_stored(&sa, "workbuddy.json")`（parse 时文件名未知，宿主以实际落盘为准）。`rpc.rs` 测试更新：

```rust
#[test] fn credential_file_name_rules() {
    let mut sa = minimal_stored_auth();            // 已有测试 helper 语义：uid 可控
    sa.account.uid = String::new();
    assert_eq!(credential_file_name(&sa), "workbuddy.json");
    sa.account.uid = "3a417abcdef".into();
    assert_eq!(credential_file_name(&sa), "workbuddy-abcdef.json");
    sa.account.uid = "abc".into();
    assert_eq!(credential_file_name(&sa), "workbuddy-abc.json");
}
#[test] fn storage_json_roundtrip() { /* 原测试改为 auth_data_from_stored(&sa, "workbuddy.json") */ }
```

- [ ] **Step 3: 测试通过后提交**

Run: `cargo test --release 2>&1 | tail -3`
Expected: 全 PASS。

```bash
git add -A && git commit -m "feat: per-account credential file names (workbuddy-{uid6}.json)"
```

### Task 2: scheduler.rs 额度缓存与加权随机

**Files:**
- Create: `src/scheduler.rs`
- Modify: `Cargo.toml`（加 `rand = "0.8"`）

**Interfaces:**
- Produces:
  - `pub fn pick(candidates: &serde_json::Value) -> serde_json::Value`（入口，解析 PascalCase 请求 → 响应 `{"AuthID","DelegateBuiltin":"","Handled":bool}`）
  - `pub fn candidates_filter_and_pick(cands: &[Cand], state: &StateView) -> Option<String>`
  - `pub struct Cand { pub id: String, pub provider: String, pub status: String }`（从请求解析）
  - `pub fn is_disabled(auth_index: &str) -> bool`（读 state 文件 + 内存缓存）
  - `pub fn note_usage(auth_index: &str, total_tokens: i64)`（乐观扣减）
  - `pub fn refresh_quota_async(auth_index: &str, sa: StoredAuth)`（后台线程调 billing 拉取并写缓存）

- [ ] **Step 1: Cargo.toml 加依赖**

```toml
rand = "0.8"
```

- [ ] **Step 2: 实现缓存与选择**

核心逻辑（缓存结构 `Mutex<HashMap<String, CacheEntry>>`，`CacheEntry { total_remain: i64, total_size: i64, refreshed_at: u64, pending: bool }`；TTL 60s）：

```rust
pub fn candidates_filter_and_pick(cands: &[Cand], state: &StateView) -> Option<String> {
    let mut weighted: Vec<(String, i64)> = Vec::new();
    for c in cands {
        if c.provider != "workbuddy" { continue; }
        if state.disabled.contains(&c.id) { continue; }
        let entry = cache_get(&c.id);
        let remain = match entry {
            Some(e) => e.total_remain,
            None => { refresh_quota_async_unknown(&c.id); i64::MAX / 2 } // 首次未知：优先用一次
        };
        if remain <= 0 { continue; }
        weighted.push((c.id.clone(), remain.min(1_000_000)));
    }
    if weighted.is_empty() { return None; }
    let total: i64 = weighted.iter().map(|(_, w)| *w).sum();
    let mut pick = rand::thread_rng().gen_range(0..total);
    for (id, w) in &weighted {
        if pick < *w { return Some(id.clone()); }
        pick -= *w;
    }
    weighted.last().map(|(id, _)| id.clone())
}
```

`pick()` 解析请求：`candidates` 数组各元素的 `ID/Provider/Status`（大小写不敏感 get_field 逻辑复制自 dispatch.rs，提取为 `pub fn get_field` 放 rpc.rs 复用）。`Status` 非空且不等于 `active` 的候选跳过（宿主已过滤大部分不可用，双保险）。

- [ ] **Step 3: 单测**

```rust
// 测试用候选构造 helper：fn cand(id: &str, provider: &str, status: &str) -> Cand
#[test] fn disabled_candidate_skipped() { /* state 文件含 id → 不被选中（跑 50 次断言不出现） */ }
#[test] fn exhausted_candidate_skipped() { /* 缓存 total_remain=0 → 不选中 */ }
#[test] fn weighted_distribution_skews_to_rich() { /* 账号A remain=900, B=100 → 200 次选择 A≥120 */ }
#[test] fn all_exhausted_returns_none() { /* → None（外层转 Handled:false） */ }
#[test] fn unknown_candidate_preferred_once() { /* 无缓存账号 remain=MAX/2 → 首次必被选中 */ }
```

- [ ] **Step 4: 测试通过后提交**

```bash
git add -A && git commit -m "feat: quota-aware weighted-random auth scheduler core"
```

### Task 3: scheduler.pick RPC 接入 + 乐观扣减

**Files:**
- Modify: `src/dispatch.rs`（新分支）
- Modify: `src/models.rs`（capabilities 加 `scheduler: true`）
- Modify: `src/rpc.rs`（`Capabilities` 加字段）
- Modify: `src/executor.rs`（usage 回传扣减钩子）

**Interfaces:**
- Consumes: `scheduler::pick`、`scheduler::note_usage`。
- Produces: dispatch `"scheduler.pick"` 分支；executor 成功后调用 `note_usage`。

- [ ] **Step 1: rpc.rs Capabilities 加 `pub scheduler: bool`（models.rs default_registration 置 true）**

- [ ] **Step 2: dispatch 分支**

```rust
"scheduler.pick" => {
    let resp = crate::scheduler::pick(&req);
    ok_envelope(&resp).map_err(|e| e)
}
```

- [ ] **Step 3: executor 乐观扣减**

`execute` 与 `execute_stream` 的同步聚合路径：聚合结果含 `usage.total_tokens` 时调用 `scheduler::note_usage(&auth_index, tokens)`。auth_index 来源：`ExecReq` 增加 `auth_id: String`（dispatch 从请求 `AuthID` 字段取）。

- [ ] **Step 4: 单测 + 提交**

```rust
// 测试环境隔离：state 文件路径经 env override（WORKBUDDY_STATE_FILE），测试进程设置临时路径
#[test] fn scheduler_pick_shape() {
    let body = serde_json::json!({"Provider":"workbuddy","Candidates":[{"ID":"a1","Provider":"workbuddy","Status":"active"},{"ID":"a2","Provider":"codex","Status":"active"}]});
    let resp = crate::scheduler::pick(&body);
    assert_eq!(resp["Handled"], true);
    assert_eq!(resp["AuthID"], "a1"); // 唯一 workbuddy 候选必选
}
#[test] fn scheduler_pick_no_candidates_delegates() {
    let resp = crate::scheduler::pick(&serde_json::json!({"Candidates":[]}));
    assert_eq!(resp["Handled"], false);
}
```

```bash
git add -A && git commit -m "feat: wire scheduler.pick RPC with optimistic usage decrement"
```

### Task 4: state 文件 + toggle API

**Files:**
- Create: `src/state.rs`（`~/.cli-proxy-api/workbuddy-state.json` 读写，Mutex 串行化）
- Modify: `src/management.rs`（accounts 响应加 `enabled`、toggle 路由）、`src/scheduler.rs`（is_disabled 读 state）

**Interfaces:**
- Produces: `state::load_disabled() -> Vec<String>`、`state::set_disabled(auth_index: &str, disabled: bool) -> Result<(), String>`；`state::state_path() -> PathBuf`（默认 `~/.cli-proxy-api/workbuddy-state.json`，env `WORKBUDDY_STATE_FILE` 可覆盖，供测试隔离）；management `POST /plugins/workbuddy/toggle`。

- [ ] **Step 1: state.rs**（路径 `state_path()`：默认 `~/.cli-proxy-api/workbuddy-state.json`，env `WORKBUDDY_STATE_FILE` 可覆盖；结构 `{"disabled":[...]}`；`set_disabled` 原子写：临时文件 + rename；`Mutex` 防并发写）

- [ ] **Step 2: management.rs**：`handle` 增加 `("POST", base+"/toggle")` 分支 → body base64 解码出 `{"auth_index","enabled"}` → `state::set_disabled` → 200 `{"ok":true,"disabled":[...最新名单]}`；`build_accounts_dashboard` 每账号加 `"enabled": !disabled.contains(auth_index)` 与 `"file_name"`（来自 host.auth.get 的文件解析或 candidate name）。

- [ ] **Step 3: scheduler.rs `is_disabled` 改走 state::load_disabled()（内存缓存 5s，避免每次 pick 读盘）**

- [ ] **Step 4: 单测 + 提交**

```rust
#[test] fn toggle_roundtrip() { /* set_disabled("a1",true) → load_disabled 含 a1 → set false 后不含 */ }
#[test] fn toggle_endpoint_200() { /* management::handle POST toggle → 200, ok:true */ }
```

```bash
git add -A && git commit -m "feat: account enable/disable state with toggle API"
```

### Task 5: panel.html 两区重构

**Files:**
- Modify: `panel.html`（完整重写 body 部分，样式沿用）

**Interfaces:**
- Consumes: `GET /accounts`（含 `enabled`/`file_name`）、`POST /toggle`。

- [ ] **Step 1: 上区账号卡片栅格**（每卡：昵称/文件名/套餐数、总剩余率大字+进度条（<30% 红 <50% 黄）、可点击启用徽章（click → toggle → 重渲染）、最早到期提示、右上"↻ 全部刷新"）

- [ ] **Step 2: 下区明细**（账号 `<select>`（options 来自 accounts）、选中账号的总积分卡+套餐表同步渲染（纯前端）、套餐行 >6 默认折叠 + "展开全部"按钮、快照时间+刷新本账号）

- [ ] **Step 3: 单测**（management 测试加：panel HTML 含 `账号配额总览`、`账号额度明细`、`toggle` 调用函数名）

- [ ] **Step 4: 提交**

```bash
git add -A && git commit -m "feat: two-zone multi-account quota panel"
```

### Task 6: 双账号 e2e 验收

**Files:**
- Modify: `scripts/e2e-check.sh`

- [ ] **Step 1: 伪造第二账号**（复制 `workbuddy.json` → `workbuddy-test2.json`，uid 改为不同后 6 位；宿主文件监听会自动注册）

- [ ] **Step 2: 启动隔离实例（同 v0.2 流程）**，验收断言追加：
  - `accounts` 返回 2 个账号，各含 `enabled` 字段；
  - `scheduler.pick` 生效：对 8317 实例连发 6 个 chat 请求（用本插件真实凭据），观察主日志 `Use OAuth provider=workbuddy` 出现两个不同 auth_file（权重随机验证，允许不严格均匀但两账号都出现）；
  - `toggle` 停用 test2 后重复请求，日志只出现主账号；
  - 面板 HTML 含两区标题；
  - **首项验收**：确认 `scheduler.pick` 在宿主侧真实触发（debug 日志或行为证据）；未触发则按 spec 回退方案记录（round-robin 兜底 + 面板展示），spec 与实现同步修订。

- [ ] **Step 3: 清理测试账号文件与实例，提交**

```bash
git add -A && git commit -m "test: multi-account e2e verification"
```

### Task 7: 发布 v0.3.0

- [ ] **Step 1:** `Cargo.toml` version → `0.3.0`，README「功能」加多账号段落，commit + push
- [ ] **Step 2:** `git tag v0.3.0 && git push origin v0.3.0`，CI 全绿（7 jobs），Release 6 zip + checksums.txt
- [ ] **Step 3:** 主服务安装 release 产物，11+ 模型注册、对话实测、面板两区可见

---

## 实施修正（2026-09-10，与上文冲突处以本节为准）

Task 1–5 已按计划落地；Task 4/5 的「启停」实现与 Task 6 的验收在实测后修正：

1. **`host.auth.save` 不持久化 `disabled`**（返回 ok、磁盘与宿主状态均不变），
   且删文件不会让宿主注销内存中的凭据。→ 面板启停/删除改为直连宿主
   `PATCH /v0/management/auth-files/status`、`DELETE /v0/management/auth-files`；
   插件侧 `toggle`/`delete` 路由移除（详见设计文档三、五）。
2. **`login/poll` 的 state 走 POST body**：宿主 `management.handle` 的 query
   并未按预期透传（实测为空），`dispatch.rs` 现同时兼容字符串/对象两种 query 形态，
   poll 优先读 body。
3. **面板新增**「＋ 添加账号」（本计划原未包含，Task 6 用"伪造第二账号"绕过）：
   现在可在面板内完成扫码登录并落独立凭据文件。
4. **安装/升级产物必须换新 inode**（先删后写或改名覆盖）：原地 `cp` 覆盖正在被
   运行中 CPA 映射的 `.dylib` 会让 macOS 判签名失效，后续 dlopen 被 SIGKILL。
5. Task 6 验收改为在隔离实例（8400）上跑扩展后的 `scripts/e2e-check.sh`，
   7 项全绿；主实例（8317）v0.3.0 加载、12 模型、对话正常。
   Task 7（CI/tag/Release）未执行。
