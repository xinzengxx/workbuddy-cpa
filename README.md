# workbuddy-cpa

把**腾讯 CodeBuddy**（copilot.tencent.com）封装成 [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)(CPA) 的 **Rust 原生插件**：任何支持 OpenAI / Anthropic 协议的客户端（Claude Code、Cursor、Cline、SDK……）都能直接调用 CodeBuddy 背后的模型。

Rust 重写版前身是 [lovingfish/workbuddy-cliproxy](https://github.com/lovingfish/workbuddy-cliproxy)（Go 版）；原始 workbuddy 插件设计归 Sliverkiss 所有（[cpa-plugin](https://github.com/Sliverkiss/cpa-plugin)）。

## 为什么是 Rust

CPA 的 Go 插件要求宿主与插件共享包**逐字节同源**——CPA 每次升级都会让旧插件加载失败。Rust 版只通过 C ABI（`pluginabi.ABIVersion=1`）和 JSON RPC 契约（`SchemaVersion=3`）与宿主交互，**不依赖宿主的 Go 工具链和 SDK 版本**，预编译产物下载即用。

## 功能

- **扫码登录 / token 刷新**：完全走 CodeBuddy 插件协议，凭据存为 `workbuddy.json`
- **chat 执行**：OpenAI chat-completions 输入输出；非流式请求自动转上游流式再聚合；跨格式入口（Anthropic 等）自动补 SSE 帧
- **系统提示词改写**：绕开 CodeBuddy 对 Claude Code 模板短语的逐字屏蔽（`official CLI` → `official CLI tool`、`Main branch` → `Default branch`）
- **hy3 系列强制 `reasoning_effort: high`**
- **额度面板**：CPA 管理面板内嵌 WorkBuddy 页——总积分卡片 + 各套餐包明细（剩余天数 >7 天绿 / ≤7 天红）
- **配置驱动模型列表**：`cliproxyapi.conf` 中改模型列表热生效，无需重编

## 模型（内置默认表）

`glm-5.2` · `glm-5.3-flash` · `glm-5.1` · `glm-5v-turbo` · `kimi-k2.7` · `minimax-m3-pay` · `hy3` · `hy3-preview` · `hy3-preview-agent` · `deepseek-v4-pro` · `deepseek-v4-flash`

具体可用性以 CodeBuddy 账号权限为准。

## 安装

**前置**：运行中的 CLIProxyAPI v7.2.x（带 CGO / 插件支持）。

### 方式一：Release 下载（推荐）

从 [Releases](../../releases) 下载对应平台 zip（如 `workbuddy_0.2.0_darwin_arm64.zip`），解压出 `workbuddy.dylib`（Linux 为 `.so`，Windows 为 `.dll`），放入 CPA 插件目录。

### 方式二：CPA 插件商店

zip 命名与 CPA `internal/pluginstore.ArchiveName` 规范一致，CPA 面板内置插件商店可直接安装（若已收录本仓库）。

### 方式三：源码编译

```bash
git clone <this-repo>
cd workbuddy-cpa
cargo build --release
cp target/release/libworkbuddy.dylib ~/.cli-proxy-api/plugins/   # linux: *.so / windows: *.dll
```

### 启用

`cliproxyapi.conf`：

```yaml
plugins:
  enabled: true
  dir: "~/.cli-proxy-api/plugins"     # brew launchd 服务必须用绝对路径
  configs:
    workbuddy: { enabled: true, priority: 100 }
```

重启 CPA，日志出现 `plugin registered plugin_id=workbuddy version=0.2.0` 即成功。然后到 CPA 面板添加 workbuddy 凭据，扫码登录 CodeBuddy。

## 自定义模型列表

```yaml
plugins:
  configs:
    workbuddy:
      enabled: true
      models:
        - id: glm-5.3-flash
          name: "GLM-5.3 Flash"
          context: 1000000
        - id: kimi-k3            # 新模型上线后加一行即可
```

保存后 CPA 热加载（`plugin.reconfigure`），无需重启或重编。

## 使用

CPA 默认端口 `8317`，API key 见 `config.yaml` 的 `api-keys`。

| 协议 | Base URL |
|------|----------|
| OpenAI | `http://<host>:8317/v1` |
| Anthropic | `http://<host>:8317`（不带 `/v1`，走 `x-api-key`） |

## 额度查询

面板：CPA 管理界面 → WorkBuddy 菜单。
API：`GET /v0/management/plugins/workbuddy/accounts`（Bearer 管理密钥）。

## 兼容性

| 宿主 CPA | 本插件 |
|---|---|
| ≥ v7.2.x（C ABI v1 / RPC schema v3） | v0.2.0+ |

宿主升级通常无需任何操作；仅当 CPA 变更 C ABI 或 RPC 契约时才需要跟进（会发布对应新版本）。

## License

MIT
