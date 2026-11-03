# 2026-11-03 — 优化 agent 体验（实战报告 5 项的修复设计）

来源：一个跑在容器里的 agent 提交的工具面报告（2 项阻断 + 3 项能力缺失），5 条已全部在 `main`
源码中逐条核实。本文是实施前的设计：改动边界、落点、验收方式。行号为核实时读到的位置，
实施时若已漂移，以语义为准。

## 已核实的 5 个问题

| # | 症状（agent 视角） | 根因 |
|---|---|---|
| 1 | 只有 `container-<ts>` 全 ID 可用，名字和裸 UUID 一律报 `container not found: {id}`，agent 靠试错猜出格式 | daemon 每个动词各自做 `instances/<id>` 精确匹配：`resources.rs:32`、`workspaces.rs:315`、`dispatch.rs:530/1346/1373`、`extensions.rs` 6 处；唯一会按名字解析的 `resolve_container`（`apply.rs:312`，错误会列出已有容器）只有 apply 在用 |
| 2 | 容器每次重启换端口（实测 4 次重启 4 个端口），旧页面停在死端口且无路可去 | `lifecycle.rs:60 allocate_loopback_port()` bind :0 → 新端口写进 `host.json`；页面工具里没有 navigate，`box_set_browser` 只接受浏览器可执行文件 `path`（`tools.ts:811`），agent 传的 `url` 被丢弃；只有重建会话（lifecycle restart）才重读 `host.json` |
| 3 | `box_workspace add` 的 `restart` 不生效，运行中容器照样被拒 | 插件端已声明并转发（`manage.ts:526/574`），daemon 端 `add_container_workspace_rpc`（`workspaces.rs:357`）从不读 `request["restart"]`；对照 `resources.rs:175/841`：资源注入的同款编排存在，workspace 这个动词没做 |
| 4 | 悬停才显示的行操作按钮在无障碍树里不存在，也没有工具能触发悬停 | box-browser 无 hover 概念（grep 零命中）；`box_click_element`/`box_click_at` 只发按下+抬起 |
| 5 | 无 JS 执行能力，`box_console` 只回日志 | box-browser 内部已在用 `Runtime.evaluate`（`session.rs:173`，`examples/probe.rs` 有完整用法），未暴露成工具 |

## 设计原则

1. **解析集中在边界，不在调用方各写一份。** #1 的根源不是缺一个解析器，而是 12 个入口各自
   决定怎么找容器。加一个共享解析函数，所有容器标识入口走它，`apply.rs` 的私有实现改为复用。
2. **能力落在 box-browser，工具落在插件。** 导航/悬停/求值属于 CDP 会话层；插件只做参数、
   报错文案与 `box_*` 注册。插件不得复制 daemon 语义（AGENTS.md）。
3. **给 agent 自我纠错的路径。** #2 除 navigate 外，每个页面工具的输出带当前 `url` — agent
   看到输出的 url 与 host.json 不一致就知道该导航，而不是反复截图。
4. **参数要么生效，要么不存在。** #3 实现 restart，而不是从 schema 删掉它：描述已承诺行为，
   `resources` 动词也已经是这个语义。

## A. 容器标识解析（#1）

新增 `src-tauri/crates/dshboxd/src/resolve.rs`：

```rust
pub(crate) struct ResolvedContainer { pub id: String, pub directory: PathBuf, pub name: Option<String> }

/// 把调用方给的容器标识解析成规范 id + 目录。接受三种写法：全 id（container-1730…）、
/// 容器名（my-dev）、裸 uuid（去掉 container- 前缀的 id）。名字歧义是错误，不是取第一个。
pub(crate) fn resolve_container_id(state: &DaemonState, raw: &str) -> Result<ResolvedContainer, String>;
```

规则（与 `apply.rs:312` 现有行为一致，扩展到全部入口）：
1. `instances/<raw>` 目录存在 → 直接命中（今天的语义，零行为变化）。
2. 否则扫 `box_containers::scan_containers(root)`：`name == raw` 命中；
   `format!("container-{raw}")` 命中某个 id 也算命中（裸 uuid）。
3. 都不中 → 错误改成 apply 风格：`no container named "{raw}"; the containers are: a, b, c`；
   一个容器都没有时明说还没有容器。

接线点（替换现有精确匹配）：
- `dshboxd/src/resources.rs:32 container_dir`
- `dshboxd/src/workspaces.rs:315 container_dir`
- `dshboxd/src/dispatch.rs`：`describe_container`(1373)、`plugin_dependency_graph` 的 container 分支(530)
- `dshboxd/src/extensions.rs`：6 处 `container not found`
- `dshboxd/src/apply.rs`：`resolve_container` 改为调用共享实现
- CLI `src/cli/container.rs:67`（`logs`）：解析后再取目录
- desktop `app/containers.rs`、`app/extensions.rs`：**未接线**——desktop 层发的是真实 id，保持精确匹配；
  评审（P2-5）更正此处的过记，避免文档多报。

约束：解析只在**读请求边界**做一次，向下传规范 id。字符集检查只守**真正拼接路径**的那条捷径
（`fast_lookup`）；名字匹配不设字符集门——名字在创建时只查长度，合法名字可以带空格、点号或任意
Unicode（评审 P1-1）。**`delete_container` 不接解析**，见风险 R1。

## B. 导航与端口漂移（#2）

**B1 box-browser（`session.rs`）**
- `navigate(&mut self, url)` → CDP `Page.navigate`，等 `Page.loadEventFired`，带超时；超时不算
  失败，回报 pending（一个卡在加载的页面不是错误）。
- `current_url()` → `Runtime.evaluate("document.location.href")`，复用 `session.rs:173` 已有通道。
- `PageSnapshot` 加 `url` 字段，插件透传。

**B2 插件：`box_navigate`**
```ts
parameters: { containerId, url?, wait?: "load" | "none" }
```
- 不给 `url` 时取 `describe_container` 的 `url`（即当前 `host.json` 的 `hostUrl`）—
  "把页面带回活端口"就是这个日常动作。
- 返回 `{ url, title }`；`box_page_text`/`box_screenshot`/`box_click_element` 输出附带 `url`。

**B3 稳定端口：本次不做。** 把 `allocate_loopback_port()` 改成按容器 id 派生，需要 DSH host
接受外传端口并定义碰撞回退，是另一条线；`box_navigate` 已能让 agent 自己纠正。记为后续可选项。

## C. workspace restart 编排（#3）

`add_container_workspace_rpc` 读 `request["restart"]`（默认 false）。为 true 且 host 在跑：
`stop_dsh_container()` → 写 registry → `start_dsh_container_inner()`，返回 `{ restarted: true, url }`。
复用 `resources.rs:841` 那套停止/启动调用，不新写进程逻辑。缺省时的错误文案指向真实动作：
`this container is running and its host rewrites its own workspace registry; pass restart: true, or run dshbox container stop <id> first`。

## D. `box_hover`（#4）

`session.rs`：`hover(x, y)` → `Input.dispatchMouseEvent { type: "mouseMoved", x, y, buttons: 0 }`。
插件 `box_hover`：`{ containerId, x?, y?, role?, name? }`；给 role/name 时先取 a11y 树坐标
（与 `box_click_element` 同一套定位），命中多个报歧义而不是选第一个。

## E. `box_evaluate`（#5）

`session.rs`：`evaluate(expression) -> Value`，`Runtime.evaluate { returnByValue: true, awaitPromise: true }`；
`exceptionDetails` 转成错误文本，不静默返回 undefined。插件 `box_evaluate`：`{ containerId, expression }`
→ `{ result }`，输出沿用现有截断策略。

## F. 调用者身份（追加决策）

用户在实现中途提出：同一份插件既装在容器里也装在容器外（桌面 DSH、Web 会话），
agent 无法区分自己坐在哪里——旧 `box-dsh-context` 移除后，"我是谁"没有跟着迁移过来。

- 守护进程在容器内拉起 DSH host 时注入 `DSHBOX_CONTAINER_ID`（`lifecycle.rs`，
  走 `dsh_host_policy` 的 `task_override`，环境保持白名单语义）。
- 插件 `selfIdentity()`：env 戳为权威；安装路径解析（`<instance>/profile/...` 路径段）
  仅作旧守护进程的兜底。身份只有两种：`container`（在某个 Box 容器里）与
  `external`（容器外观察，桌面/Web 一律算 external——不预设载体）。
- `box_overview` 输出恒带 `self: { role, containerId }`（不受 include 过滤影响）；
  `runsInside` 自杀守卫改读同一身份，语义不变。

## 版本与发布

- 日常**只做 patch 升级**（0.1.9 → 0.1.10）；`x.y.0` 只留给真正的功能里程碑，不随手 bump。
- 插件 `@nexus-aethra/dsh-box-sandbox`：`0.2.0 → 0.2.1-alpha.1`（同样只动 patch 位）。
- `npm publish --tag alpha`：dist-tag `latest` 保持 0.2.0，普通安装不受影响。
- 注册表 npmmirror（`npm config get registry` 实测）；发布前 `npm view` 确认 0.2.1-alpha.1 未被占用。
- 改动含 daemon 侧，按 AGENTS.md 的一次构建约定：`prepare-server.mjs --force` + 重编 `dshbox.exe` 一起部署。

## 测试计划

1. 单测：`resolve.rs`（全 id / 名字 / 裸 uuid / 未知 / 同名歧义 / `..` 注入 / 空列表）；
   workspace restart 编排；box-browser 的 navigate + current_url 用本地 fixture 页。
2. 插件：`pnpm check:outputs`（`check-output-schemas.mjs` 钉住输出结构，新字段同一提交更新夹具）；
   `check-tool-outputs.mjs` 加三个新工具的活体用例。
3. 端到端好用性验证（用户要求，用容器里的真实 agent）：
   - 起容器 → 用**名字**跑 `box_workspace add` / `box_page_text` / `box_describe`，全部接受；
   - `box_lifecycle restart` → 端口变 → `box_navigate`（不带 url）→ 页面回来，输出 url 与 host.json 一致；
   - hover-only 行操作：`box_hover` 后截图可见 → `box_click_at` 点得动；
   - `box_evaluate "document.title"` 返回标题；抛异常的表达式返回错误文本；
   - 旧写法（全前缀 id）逐字仍可用 = 向后兼容。
4. 回归：`cargo test --workspace`、`pnpm build`、`command_surface` 绿（无新 Tauri 命令，只加 daemon RPC 与工具）。

## 分支与提交

- 分支 `optimize/agent-experience`（用户指定：优化 agent 体验）。
- 提交切分：`feat(dshboxd): container identifier resolution` → `feat(box-browser): navigate/hover/evaluate`
  → `feat(plugin): box_navigate/box_hover/box_evaluate + workspace restart` → `docs`。
- PR 按 `pr-review-merge` 规范：功能分支、评审轮次、squash 带 `(#N)`、`--body-file` 写最新正文。

## 风险

- **R1** 解析器若覆盖 `delete_container`，等于让别名能删容器 — 破坏性动词接受别名是新攻击面。
  决定：delete 仍只认全 id，解析只服务读与非破坏性写。
- **R2** 页面工具输出新增 `url` 是输出结构变更，`check-output-schemas.mjs` 会红 → 同一提交更新夹具。
- **R3** 容器里的 agent 用的是启动时物化的插件副本：发布 alpha 后必须重启容器才生效。
- **R4** `box_evaluate` 给 agent 任意 JS 执行权。容器内本就能跑任意工具代码，边界没有变宽；
  SKILL.md 写明它只作用于页面世界，不是宿主。
