# workbuddy-cpa

把**腾讯 CodeBuddy**（copilot.tencent.com）封装成 [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)(CPA) 的 **Rust 原生插件**：任何支持 OpenAI / Anthropic 协议的客户端（Claude Code、Cursor、Cline、SDK……）都能直接调用 CodeBuddy 背后的模型。

Rust 重写版前身是 [lovingfish/workbuddy-cliproxy](https://github.com/lovingfish/workbuddy-cliproxy)（Go 版）；原始 workbuddy 插件设计归 Sliverkiss 所有（[cpa-plugin](https://github.com/Sliverkiss/cpa-plugin)）。

## 为什么是 Rust

CPA 的 Go 插件要求宿主与插件共享包**逐字节同源**——CPA 每次升级都会让旧插件加载失败。Rust 版只通过 C ABI（`pluginabi.ABIVersion=1`）和 JSON RPC 契约（`SchemaVersion=3`）与宿主交互，**不依赖宿主的 Go 工具链和 SDK 版本**，预编译产物下载即用。

## 功能

- **扫码登录 / token 刷新**：完全走 CodeBuddy 插件协议；默认凭据 `workbuddy.json`，**多账号**各自落在独立文件 `workbuddy-{uid后6位}.json`
- **多账号管理**：面板内「＋ 添加账号」（扫码/授权链接 + 轮询落盘）、卡片徽章一键启停、悬停 ✕ 删除；启停与删除走宿主权威接口（见下）
- **chat 执行**：OpenAI chat-completions 输入输出；非流式请求自动转上游流式再聚合；跨格式入口（Anthropic 等）自动补 SSE 帧
- **系统提示词改写**：绕开 CodeBuddy 对 Claude Code 模板短语的逐字屏蔽（`official CLI` → `official CLI tool`、`Main branch` → `Default branch`）
- **hy3 系列强制 `reasoning_effort: high`**
- **额度面板（两区）**：上区账号配额总览卡片（剩余率进度条 + 最早到期 + 启停徽章），下区选中账号的套餐级明细表（剩余天数 >7 天绿 / ≤7 天红）
- **配置驱动模型列表**：`cliproxyapi.conf` 中改模型列表热生效，无需重编

## 模型（内置默认表）

`glm-5.2` · `glm-5.3-flash` · `glm-5.1` · `glm-5v-turbo` · `kimi-k2.7` · `minimax-m3-pay` · `hy3` · `hy3-preview` · `hy3-preview-agent` · `deepseek-v4-pro` · `deepseek-v4-flash` · `deepseek-v4.1-flash`

具体可用性以 CodeBuddy 账号权限为准。

## 安装

**前置**：运行中的 CLIProxyAPI v7.2.x（带 CGO / 插件支持）。

### 方式一：Release 下载（推荐）

从 [Releases](../../releases) 下载对应平台 zip（macOS：`workbuddy_0.3.0_darwin_arm64.zip` / `_darwin_amd64.zip`；Linux：`_linux_amd64.zip` / `_linux_arm64.zip`；Windows：`_windows_amd64.zip` / `_windows_arm64.zip`），解压出 `workbuddy.dylib`（Linux 为 `.so`，Windows 为 `.dll`），放入 CPA 插件目录。

> **升级时先删旧文件再放新文件**（`rm workbuddy.dylib && cp ...`，或 `cp` 到临时名再 `mv` 覆盖）。
> 直接 `cp` 原地覆盖一个**正被运行中的 CPA 映射**的 `.dylib`，会让 macOS 判定该文件代码签名失效，
> 之后任何 `dlopen` 它的进程都会被 SIGKILL（崩溃报告 `CODESIGNING / Invalid Page`）。
> 换新 inode（先删后写、或改名覆盖）即可避免；万一中招，`codesign --force --sign - workbuddy.dylib` 也能救回来。

### 方式二：CPA 插件商店

zip 命名与 CPA `internal/pluginstore.ArchiveName` 规范一致，CPA 面板内置插件商店可直接安装（若已收录本仓库）。

### 方式三：源码编译

```bash
git clone <this-repo>
cd workbuddy-cpa
cargo build --release
# 先删后放：避免原地覆盖正在被 CPA 映射的旧产物
rm -f ~/.cli-proxy-api/plugins/workbuddy.dylib
cp target/release/libworkbuddy.dylib ~/.cli-proxy-api/plugins/   # macOS: *.dylib
cp target/release/libworkbuddy.so    ~/.cli-proxy-api/plugins/   # Linux: *.so
cp target/release/workbuddy.dll      <CPA插件目录>/              # Windows: *.dll
```

> Windows 注意：CPA 插件目录取配置里 `plugins.dir`（可用相对路径，如 CPA 可执行文件同目录下的 `plugins/`）。Windows 上若 CPA 由任务计划/服务方式启动，建议像 macOS 一样把 `dir` 写成绝对路径（如 `C:\path\to\plugins`），避免工作目录不确定导致找不到插件。

### 启用

`cliproxyapi.conf`：

```yaml
plugins:
  enabled: true
  dir: "~/.cli-proxy-api/plugins"     # brew launchd 服务必须用绝对路径
  configs:
    workbuddy: { enabled: true, priority: 100 }
```

重启 CPA，日志出现 `plugin registered plugin_id=workbuddy version=0.3.0` 即成功。然后到 CPA 面板添加 workbuddy 凭据，扫码登录 CodeBuddy。

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

## 额度与多账号管理

面板：CPA 管理界面 → WorkBuddy 菜单。

| 操作 | 位置 | 实现 |
|---|---|---|
| 查看各账号额度/套餐明细 | 面板两区（总览卡片 + 明细表） | `GET /v0/management/plugins/workbuddy/accounts` |
| 添加账号 | 面板「＋ 添加账号」 | `GET .../login/start` → 展示授权链接 → `POST .../login/poll` 成功后 `host.auth.save` 落独立凭据文件 |
| 启用 / 停用账号 | 卡片徽章点击 | 面板直接调宿主 `PATCH /v0/management/auth-files/status` |
| 删除账号 | 卡片右上 ✕ | 面板直接调宿主 `DELETE /v0/management/auth-files` |

**为什么启停/删除不走插件自己的接口**：实测（CPA v7.2.130）`host.auth.save` 不会持久化 `disabled` 字段的改写，
且删掉凭据文件后宿主内存里仍保留该凭据——两者都必须由宿主自己的 auth-files 接口完成。
插件因此只负责模型/执行/额度，凭据生命周期完全交还宿主。

> 已知限制：CPA v7.2.130 **不会调用插件声明的 `scheduler.pick`** 钩子，多账号由宿主内置 round-robin 轮转。
> 插件已实现完整 `pick` 协议并保留 `scheduler: true` capability，宿主未来启用该钩子即自动生效（额度感知加权随机）。

## 兼容性

| 宿主 CPA | 本插件 |
|---|---|
| ≥ v7.2.x（C ABI v1 / RPC schema v3） | v0.2.0+ |
| ≥ v7.2.130（`/v0/management/auth-files` 启停与删除） | v0.3.0+（需多账号管理时） |

宿主升级通常无需任何操作；仅当 CPA 变更 C ABI 或 RPC 契约时才需要跟进（会发布对应新版本）。

## License

MIT
