# workbuddy 插件多账号登录与配额管理设计

日期：2026-09-10
状态：已批准（聊天中确认：调度 = 额度感知 + 负载均衡组合；面板上下两区布局）

## 背景与动机

当前插件为单凭据设计：登录成功固定写入 `workbuddy.json`，再次扫码会覆盖原凭据。
Sliverkiss 闭源原版支持 multi-account login 与积分感知调度，本设计将其补齐到
Rust 版，并用宿主官方的 `scheduler.pick` 插件钩子实现（无需 hack）。

技术核查结论（已验证）：

1. 宿主对同 provider 的多份凭据各自注册独立 client（日志证据：
   `Registered client workbuddy.json from provider workbuddy with 11 models`）。
2. 宿主内置路由（`routing.strategy: round-robin`）在 auth 层轮转，但每次选择前
   会调用已注册 Scheduler 插件的 `pick`（`pluginapi.Scheduler::Pick`），候选
   `SchedulerAuthCandidate[]`（含 AuthID/Status/Priority）由宿主传入，插件返回
   `AuthID` 即完成选择；`Handled=false` 则回退宿主内置选择器。
3. 凭据文件名由插件在 `AuthData.FileName` 中给出，宿主按此落盘。

## 目标

- 多账号登录：多次扫码叠加账号，各自独立凭据文件。
- 调度：额度感知 + 负载均衡（加权随机，权重=剩余额度），耗尽/停用账号自动跳过。
- 面板：账号配额总览（上区，剩余率卡片）+ 单账号套餐明细（下区，下拉切换），
  含启用/停用徽章。
- 管理 API：`toggle`（启停账号）。

## 非目标（YAGNI）

- 配额预警通知、跨账号用量报表。
- `select`（固定主账号模式）——`scheduler.pick` 已覆盖需求，暂不做。
- 账号分组/标签。

## 一、登录与凭据落盘（src/auth.rs）

- `AuthData.FileName` 由固定 `workbuddy.json` 改为
  `workbuddy-{uid后6位}.json`（uid 不足 6 位取全部；为空回退 `workbuddy.json`）。
- `poll_login` 成功路径在组装 `storedAuth` 后以新 FileName 返回；同账号重复扫码
  落同文件（宿主按 FileName 覆盖，天然幂等）。
- `auth_data_from_stored` 签名改为接收 `file_name: &str` 参数；
  `parse_auth` 不改（按内容识别 provider，与文件名无关）。

## 二、调度器（新增 src/scheduler.rs）

- 注册 `Capabilities.Scheduler = true`；实现 `scheduler.pick` RPC。
- 内存额度缓存：`Mutex<HashMap<auth_index, CacheEntry>>`，
  `CacheEntry { total_remain, total_size, refreshed_at }`，TTL 60s。
- `pick` 流程：
  1. 过滤候选：`provider == workbuddy`、状态非禁用、面板停用名单外。
  2. 每候选取缓存额度（过期/缺失则用后台线程异步刷新，本次按
     `total_remain = i64::MAX/2` 参与选择——首次未知的账号优先用一次）。
  3. 过滤 `total_remain <= 0` 的耗尽账号；全部耗尽 → `Handled: false` 交还宿主。
  4. **加权随机**：`weight = max(remain, 1)`，用 `rand` crate 选点。额度多的
     账号承接更多流量（负载均衡），趋零账号自动降权（额度感知）。
  5. 返回 `{ AuthID, Handled: true }`。
- 乐观扣减：`executor.execute(_stream)` 成功响应的 `usage.total_tokens` 存在时，
  对本次 AuthID 的缓存 `total_remain -= total_tokens`（不触发落盘/上报）。
- 停用名单：持久化在 `~/.cli-proxy-api/workbuddy-state.json`（见三），
  `pick` 与插件启动时读取；面板 toggle 即时生效。
- 网络刷新失败不阻塞调度：缓存过期仅触发异步刷新，pick 永不发起同步网络请求。

## 三、面板与管理 API（src/management.rs、panel.html）

- `GET /accounts` 响应每账号字段：`auth_index`、`name`/`file_name`（宿主侧磁盘文件名）、`nickname`、
  `enabled`/`disabled`（宿主权威状态）、`status`/`status_message`/`unavailable`/`failed`/`recent_requests`、`credits`。
- 新增 `GET /plugins/workbuddy/login/start` 与 `POST /plugins/workbuddy/login/poll {state}`：
  面板内完成「添加账号」——拿授权链接 → 轮询 → 成功后用 `host.auth.save` 按 `workbuddy-{uid6}.json` 落盘。
- **启停与删除不在插件侧实现（2026-09-10 实测修正）**：
  - `host.auth.save` 无法持久化 `disabled` 字段改写：返回 ok，磁盘与宿主状态均不变（反复 toggle 实测）。
  - 直接删除凭据文件后，宿主内存仍保留该凭据（`/auth-files` 仍列出），调度会继续路由到不存在的凭据。
  - 故面板改为直接调用宿主权威接口：
    `PATCH /v0/management/auth-files/status {name,disabled}` 与
    `DELETE /v0/management/auth-files {names:[...]}`（与宿主自带面板同一套）。
  - 插件侧原 `toggle`/`delete` 路由与 `workbuddy-state.json` 停用名单写入随之移除；
    `state.rs` 只保留 `load_disabled` 供未来 `scheduler.pick` 真正被宿主调用时使用。
- panel.html 两区布局：
  - 上区：账号卡片栅格（昵称/文件名/套餐数、总剩余率大字+进度条、启停徽章、最早到期提示、
    「＋ 添加账号」与「↻ 全部刷新」、悬停显示 ✕ 删除）。
  - 下区：账号下拉选择 → 该账号总积分卡 + 套餐明细表（剩余天数 >7 天绿 / ≤7 天红），
    套餐行 >6 行默认折叠。
- 凭据 `nickname` 为空时显示文件名去掉 `workbuddy-` 前缀与 `.json` 后缀。

## 四、RPC 契约（新增，与宿主逐字段对齐）

- 请求 `scheduler.pick`：`{"plugin":{...},"provider":"workbuddy","providers":[...],
  "model":"...","stream":bool,"options":{...},"candidates":[{"id","provider",
  "priority","status","attributes","metadata"}]}`（字段名以 v7.2.130
  `pluginapi.SchedulerPickRequest` 为准，实现前二次核对源码）。
- 响应：`{"auth_id":"...","delegate_builtin":"","handled":true}`。
- capability：`"scheduler": true`。
- 实现时以 `rpc_schema.go` 与 `pluginapi/types.go` 源码逐字段核对并更新
  契约对照表（同 v0.2.0 重写流程）。

## 五、依赖与风险

- 新增 crate：`rand`（加权随机）。
- 风险（实测更新 2026-09-10）：
  - `scheduler.pick` 在 v7.2.130 双凭据实测**未触发**（RPC trace 证实宿主
    从未发送该方法；双凭据由宿主内置 round-robin 轮转，日志确认
    auth_file 在两个凭据文件间交替）。插件已实现完整 pick 响应协议并保留
    `scheduler: true` capability——宿主未来版本启用该钩子时无需改动即生效。
  - `host.auth.save` 不持久化 `disabled`；删除文件不等于注销凭据。
    故启停/删除改为面板直连宿主 `/v0/management/auth-files`（见三）。
  - **macOS 原地覆盖已映射的 dylib 会导致签名失效**：直接 `cp` 覆盖正在被
    运行中的 CPA 映射的插件产物，后续 `dlopen` 该文件的进程被 SIGKILL
    （崩溃报告 `CODESIGNING / Invalid Page`）。升级/重装必须换新 inode
    （先 `rm` 再 `cp`，或 `cp` 到临时名后 `mv`）。已在 README 安装章节标注。
  - `workbuddy-{uid6}.json` 的 uid 后 6 位理论上可碰撞（实际账号 uid 为 UUID，
    概率可忽略）；碰撞时同文件覆盖，等价于同账号重登。
  - 加权随机在双账号下与"轮转"体感差异不大，验收以分布统计（各账号
    承接请求数比例≈额度比例）为准。
  - 停用名单文件与凭据目录并发写：单写者（management.handle），加 Mutex。

## 六、验收

已完成（2026-09-10，隔离实例端口 8400 + `scripts/e2e-check.sh`，全绿）：

1. 面板两区 + 「＋ 添加账号」+ 徽章启停 + ✕ 删除均可用；`accounts` 字段完整、无调试残留。
2. 新增账号链路：`login/start` 返回授权链接、`login/poll` 正常返回 pending。
3. 启停：`PATCH /auth-files/status` 后磁盘 `disabled` 与面板 `enabled` 一致（启用/停用双向）。
4. 删除：`DELETE /auth-files` 后磁盘文件、宿主列表、面板列表三者同步消失。
5. 回归：12 个模型可见、非流式与流式对话均正常。
6. e2e 脚本已扩展多账号断言（`BASE`/`AK`/`MK_KEY` 可覆盖目标实例）。

待办：CI 六平台全绿、tag `v0.3.0` 发布并更新主服务。
