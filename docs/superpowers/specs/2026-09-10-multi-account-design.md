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

- `GET /accounts` 响应每账号增加：`enabled`（bool）、`auth_index`、`file_name`。
- 新增 `POST /plugins/workbuddy/toggle`：body `{"auth_index": "...", "enabled": bool}`。
  停用名单持久化在 `~/.cli-proxy-api/workbuddy-state.json`（插件自管理：
  `toggle` 写入、插件启动与 `pick` 时读取），`toggle` 即时更新内存名单并持久化，
  无需重启。不采用改写宿主 conf 的方案（插件不应改写宿主配置）。
- panel.html 重构为已确认的两区布局：
  - 上区：账号卡片栅格（昵称/文件名/套餐数、总剩余率大字+进度条、
    启用/停用徽章（可点击 → toggle）、最早到期提示、全部刷新按钮）。
  - 下区：账号下拉选择 → 该账号总积分卡 + 套餐明细表（剩余天数徽章
    >7 天绿 / ≤7 天红），纯前端切换（accounts 一次性返回全部数据）；
    套餐行 >6 行默认折叠（显示前 6 行 + 展开按钮）。
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
- 风险：
  - `scheduler.pick` 仅在宿主启用内置调度器路径时被调用——需在双凭据真实
    场景验证钩子确实触发（验收首项，失败则回退方案：宿主 round-robin 兜底，
    插件退化为仅在面板展示各账号额度）。
  - 加权随机在双账号下与"轮转"体感差异不大，验收以分布统计（各账号
    承接请求数比例≈额度比例）为准。
  - 停用名单文件与凭据目录并发写：单写者（management.handle），加 Mutex。

## 六、验收

1. 双凭据场景（复制现有凭据文件 + 修改 uid 后 6 位伪造第二账号）：
   `scheduler.pick` 触发、权重偏向高额度账号、耗尽账号被跳过、
   停用账号不被选中。
2. 重复扫码同账号 → 覆盖同文件；新账号 → 新文件。
3. 面板：两区布局、切换账号数据同步、toggle 后总览卡徽章变化且 pick 立即生效。
4. e2e 脚本扩展多账号断言；CI 六平台全绿；tag `v0.3.0` 发布并更新主服务。
