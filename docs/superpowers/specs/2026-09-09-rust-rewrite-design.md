# workbuddy 插件 Rust 重写设计

日期：2026-09-09
状态：已批准（聊天中逐节确认）

## 背景与动机

现有 workbuddy 插件（Go, `-buildmode=c-shared`）存在三个结构性限制：

1. **模型列表硬编码**：上游 CodeBuddy 国内站没有可依赖的模型列表接口（原版插件的 "models API" 端点 `/v2/billing/ide/trial/plugins/workbuddy` 实测仅 Global 站有效，国内站 404），新模型需改代码重编。
2. **宿主升级破坏加载**：插件链接了 CPA 的 `sdk/pluginabi`、`sdk/pluginapi` 两个包；CPA 每个版本这两个包几乎必有源码变化，Go 插件机制要求宿主与插件共享包逐字节同源，`brew upgrade cliproxyapi` 后插件必报 "plugin was built with a different version of package"。
3. **无法跨机器分发**：Go 插件不能跨平台、跨 Go 工具链分发，其他用户必须自备同版本工具链编译。

决策（用户确认）：

- 用 **Rust 重写整个插件**（选项 B），产物为纯 C ABI 动态库，彻底消除 2 和 3。
- Rust 版**完全取代** Go 版（选项 1），Go 代码删除，仓库转型 Rust 项目。
- 模型列表改为**配置驱动**（国内站无上游接口，做不到真·动态拉取；`cliproxyapi.conf` 改配置热生效，无需重编）。
- 面板 HTML 复用现有 v2 设计（总积分卡片 + 五列明细表 + 到期天数徽章），原样嵌入。

## 目标

- 功能对齐 Go 版全部行为：扫码登录/轮询/刷新、chat 执行（强制 `stream:true` 上游、SSE 解析、空 delta 清洗、系统提示词改写、hy3 强制 `reasoning_effort=high`、跨格式 SSE 帧适配）、计费额度查询、管理路由 + 嵌入式面板。
- 新增：`cliproxyapi.conf` 中配置模型列表，`plugin.reconfigure` 热加载；未配置时回退内置默认列表。
- 产物不链接任何 CPA Go SDK 包；仅依赖 Go 标准库等价能力（Rust 侧 serde 等 crate）。
- GitHub Actions 六平台发布（macOS arm64/amd64、Linux amd64/arm64、Windows amd64/arm64），产物命名遵循 CPA 插件商店规范，兼容面板一键安装。

## 非目标

- 不做真·上游动态模型拉取（接口不存在）。
- 不复刻原版闭源插件的每日签到、积分调度、多账号生命周期管理。
- 不做 C/Rust 之外的语言变体。

## 架构

```
workbuddy-cliproxy/
├── Cargo.toml              # crate-type = ["cdylib"]
├── panel.html              # v2 面板（include_str! 嵌入）
├── .github/workflows/release.yml
└── src/
    ├── main.rs / lib.rs    # 入口；C ABI 导出
    ├── cabi.rs             # 宿主函数指针表、buffer 读写、hostCall 封装
    ├── rpc.rs              # envelope {ok,result,error}、全部 RPC 请求/响应类型
    ├── dispatch.rs         # 方法分发（match method）
    ├── auth.rs             # 登录状态机、单次轮询、token 刷新
    ├── executor.rs         # 执行、SSE 解析、后台 pump 线程
    ├── models.rs           # 内置模型表 + config 覆盖合并
    ├── billing.rs          # /v2/billing/meter/get-user-resource
    ├── management.rs       # 管理路由分发
    └── upstream.rs         # HTTP 客户端（ureq+rustls）、公共头、重试
```

### 并发模型

- 宿主 RPC 是同步调用：ureq 阻塞式请求，直接在调用线程完成。
- 异步流（`executor.execute_stream` 带 `stream_id`）：返回空 chunks 后 `std::thread::spawn` 后台泵线程，逐块 `host.stream.emit`，结束 `host.stream.close`；emit 失败（客户端断开）即停止读上游。
- 登录状态：`Mutex<HashMap<String, LoginCtx>>`（state → cookie 客户端 + 过期时间），5 分钟 TTL。
- 宿主指针：`OnceLock<HostApi>`，`cliproxy_plugin_init` 时写入。

### 技术选型

| 依赖 | 用途 | 说明 |
|---|---|---|
| ureq | HTTP 客户端 | 同步阻塞；匹配 RPC 形态；无 tokio |
| rustls + rustls-native-certs | TLS | 纯 Rust，不链接系统 OpenSSL（跨机可移植关键）；native-certs 读系统根证书 |
| serde / serde_json | JSON | 契约类型 |
| serde_yaml | 插件配置解析 | config_yaml 覆盖模型表 |
| base64 | `[]byte` 字段 | 宿主 Go json 将 `[]byte` 编码为 base64 |
| cookie_store + ureq 的 cookie 支持 | 登录 cookie 亲和 | auth/state 与轮询共享同一 jar |
| time | RFC3339 时间 | ExpiresAt 等字段 |

禁止引入：tokio/async-std（不需要）、openssl（可移植性）、cgo 桥接库。

## JSON 契约（关键风险，专项核对）

宿主为 Go `encoding/json`。以下字段名已从 CPA v7.2.130 `internal/pluginhost/rpc_schema.go` 与 `sdk/pluginapi/types.go` 提取，实现时逐字段核对：

- **envelope**：`{"ok":bool,"result":raw,"error":{"code","message"}}`（小写）。
- **registration**：`schema_version`(=3)、`metadata`、`capabilities`。capabilities 蛇形键，插件声明与现有 Go 版完全一致的集合：`model_provider`、`auth_provider`、`executor`、`executor_model_scope`（值 `both`）、`executor_input_formats`、`executor_output_formats`、`management_api`（均为 true；`model_registrar` 等其余键不声明）。
- **ModelInfo**：`ID`/`Object`/`OwnedBy`/`DisplayName`/`Name`/`SupportedGenerationMethods`/`ContextLength`/`MaxCompletionTokens`/`UserDefined` —— Go 导出字段名原样（Go json 对无 tag 字段用字段名精确匹配，大小写不敏感但无下划线转换），Rust 侧用 `#[serde(rename_all = "PascalCase")]` 加显式 rename 核对。
- **AuthData**：`Provider`/`ID`/`FileName`/`Label`/`StorageJSON`(base64 string)/`Metadata`。
- **AuthLoginPollResponse**：`Status`（"pending"/"success"/"error"）、`Message`、`Auth`。
- **AuthRefreshResponse**：`Auth`；`AuthData.NextRefreshAfter` 为 RFC3339（可省略）。
- **executor.execute 响应**：`{"Payload": base64, "Headers": {...}}`。
- **executor.execute_stream 响应**：`{"Headers": {...}, "chunks": [{"Payload": base64}]}`；请求带 `stream_id`/`host_callback_id`。
- **管理路由注册**：`{"routes":[{"method","path","description"}],"resources":[{"path","menu","description"}]}`。
- **ManagementResponse**：`{"StatusCode":int,"Headers":{...},"Body":base64}`。
- **host.stream.emit**：`{"stream_id","payload"}`（payload 为原始字节，非 base64 —— 由 Go 侧 `[]byte` 序列化决定，实现时以 host_callbacks 源码为准再核）。

实现期以隔离实例（独立端口 + 测试密钥 + debug 日志）对每个 RPC 实测回归，全部通过才切换。

## 配置驱动模型列表

`cliproxyapi.conf`:

```yaml
plugins:
  configs:
    workbuddy:
      enabled: true
      priority: 100
      models:                      # 可选；缺省用内置表
        - id: glm-5.3-flash
          name: "GLM-5.3 Flash"
          context: 1000000
```

- `plugin.register`/`plugin.reconfigure` 时解析宿主注入的 `config_yaml`，与内置默认表**合并**（配置优先、可增可改）；解析失败回退内置表并经 `host.log` 告警。
- `model.static` / `model.for_auth` 返回合并后的列表。

## 构建与发布

- `cargo build --release`；产物 strip 后约 2-4 MB（对比 Go 版 6 MB）。
- GitHub Actions：tag `v*` 触发；矩阵 `macos-14`(arm64)、`macos-15`(x86_64)、`ubuntu-22.04`(amd64, 兼容旧 glibc)、`ubuntu-24.04-arm`、`windows-2022`(amd64)；交叉编译目标 `aarch64-pc-windows-msvc` 允许失败重试。
- 产物命名 `{id}_{version}_{goos}_{goarch}.zip`（id=workbuddy，根目录 `workbuddy.dylib|so|dll` + `checksums.txt`），与 CPA `internal/pluginstore.ArchiveName` 一致 → 面板插件商店可直装。
- README 重写：安装（Release 下载 / 插件商店 / 源码编译三选一）、Go 版删除说明、CPA 版本兼容性（Rust 版不依赖宿主 Go 版本，仅受 C ABI v1 + schema v3 约束）。

## 迁移与验收

1. Rust 骨架 + C ABI + registration/model 静态响应 → 隔离实例验证 `plugin loaded/registered`。
2. auth 全链路（登录轮询用真实上游冒烟；刷新用现有凭据验证）。
3. executor 非流式 + 流式（`/v1/chat/completions`、`/v1/messages` 两种入口路径的 SSE 帧差异）。
4. billing + 面板（对照 Go 版输出逐字段一致）。
5. 配置覆盖模型热加载验证。
6. 切换主服务安装；CI 发布流水线打通（打一个 `v0.2.0-rust` 测试 tag 验证产物）；删除 Go 代码与 go.mod/go.sum。

## 风险与对策

| 风险 | 对策 |
|---|---|
| JSON 字段名大小写/命名不匹配 → RPC 静默失败 | 契约对照表 + 全 RPC 隔离实测；debug 日志对照 Go 版报文 |
| rustls 无系统根证书 → HTTPS 全挂 | 集成 rustls-native-certs 并单测真实 upstream 请求 |
| host.stream.emit payload 编码误判 | 实现前读 host_callbacks 源码确认字节语义，先于编码假设 |
| Windows arm64 交叉编译工具链问题 | CI 中标记 continue-on-error，单独修复迭代 |
| Go 版行为细节遗漏（cookie 亲和、UA、X-No-* 头、[DONE] 过滤等） | 以现有 main.go 为对照清单逐项移植，验收步骤 2-4 全覆盖 |
