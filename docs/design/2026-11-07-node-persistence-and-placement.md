# 2026-11-07 — Node 持久化与跨 node 调度：数据结构设计

> 状态：**设计，未实现**。前序：`docs/design/2026-11-04-node-architecture-roadmap.md`（A 已合并）、
> `docs/design/2026-11-04-node-artifact.md`（B 的目标）、`docs/design/2026-11-04-shared-harness-mirror.md`（常量仓库）。
> 本文只定数据与调度，不写代码；B1 的施工以本文为准。

## 0. 一句话模型

每个 node 自带常量仓库与工具链，在本地完成 clone/install/build/run；控制层只持有**注册表、
产物索引、任务队列、vhost 端口意向**四类记录，从不持有 node 本地路径的语义，也不替 node 做文件操作。

## 1. 存储分工：什么进 documents，什么留在文件

现行 `DocumentStore` 是 `documents(scope, id, value)` 一张表，**没有二级索引**，只能整 scope 读回再过滤
(`box-foundation/src/collection.rs`, `box-store/src/document.rs`)。所以判断标准只有一条：
**权威数据归谁写**。

| 数据 | 归属 | 落点 | 理由 |
|---|---|---|---|
| node 注册表 | 控制层 | documents `scope=nodes` | A 已如此；控制层是唯一写者 |
| 产物索引 | 控制层（校验后写） | documents `scope=node-artifacts` | 摘要由控制层核对后才算数 |
| 任务队列 | 控制层 | documents `scope=tasks` | 已有；调度器唯一写者 |
| 容器身份 + 落点 | **执行侧**（文件即事实） | `instances/<id>/container.json` | 已有；容器目录本身就是它的生命 |
| profile / 模板树 / 产物归档 | node | node 本地磁盘 | 控制层只存**不透明句柄**与摘要 |

由此得出一条硬规则：**控制层不许把 `archive` 当路径去 stat**。它是不透明句柄，只有产生它的 node 能解释。

## 2. 表结构

### 2.1 `nodes`（A 已有，B 扩展）

```jsonc
{
  "id": "host" | "node-3f9a",        // 主键；host 为内建，不可删
  "kind": "host|wsl2|libkrun|vz|docker",
  "platform": "windows-x64|linux-x64",  // 产物 platform 取它，不取宿主 build_platform()
  "status": "registering|online|offline|draining|failed",
  "capabilities": ["exec","build","image-export","copy","dsh-host"],
  "controlUrl": "http://127.0.0.1:5321" | null,
  "sessionEpoch": 17,                  // 每次 daemon 启动 +1，旧 epoch 的请求一律拒绝
  "enrolledAt": 1793, "lastSeen": 1794, "helperVersion": "0.1.10",
  "backendRef": "Ubuntu-24.04" | null, // 仅供供给适配器读
  "hostNode": true,
  // ── B 新增：node 自足执行所需的两项就绪状态 ──
  "mirror": {                          // 常量仓库在 node 本地的现状
    "path": "/var/dshbox/harness-mirror.git",
    "commit": "0d1f500…",             // 本地镜像的 HEAD
    "fetchedAt": 1793, "sizeBytes": 412000000
  },
  "toolchain": {                       // 该平台可用的 node/pnpm
    "node": "22.13.0", "pnpm": "10.4.1", "platform": "linux-x64"
  },
  "relay": null                        // P 阶段填：{ "listenerPortHint": 0, "lastTunnelAt": 1794 }
}
```

**派生就绪（不落盘）**：`readyFor(base) = status==online && mirror.commit == base.sourceCommit
                       && toolchain.platform == platform`。
派活前一律用它判断，不另存布尔值——存了就会和三个来源不一致。

**状态机**：`registering →(register)→ online →(3 次心跳丢失)→ offline →(register)→ online`；
`online →(drain)→ draining →(最後一task结束)→ offline`；任何状态 `→(helper 报不可恢复错误)→ failed`。
`offline` 保留记录与已有产物（A 已实现），**只有显式移除才删**。

### 2.2 `node-artifacts`（B 新增）

主键必须是单个字符串（`documents.id` 是 `TEXT`），因此用 `nodeId|baseId`：

```jsonc
{
  "id": "node-3f9a|sealed-9c1f…",      // = nodeId + '|' + baseId
  "nodeId": "node-3f9a",
  "baseId": "sealed-9c1f…",            // prepared base 的 digest/id
  "sourceCommit": "0d1f500…",          // 冗余存，供 index 自洽检查
  "platform": "linux-x64",             // 必须等于 node.platform
  "archive": "/var/dshbox/artifacts/base-9c1f.tar",  // 不透明句柄，控制层不 stat
  "sha256": "9f2c…64 hex",
  "sizeBytes": 1327000000,
  "builtAt": 1794, "verifiedAt": 1794
}
```

校验（写入前，全部拒绝式）：`nodeId` 存在于 `nodes`；`platform == node.platform`；
`sha256` 为 64 位小写十六进制；`sizeBytes > 0`；`baseId` 非空且不含 `|`。
**只有 `node_artifact_report` 成功走完校验才写行**；任务失败只留 `tasks` 记录。

为什么不按 node 分 scope（`node-artifacts:<nodeId>`）：`DocumentStore` 没有删 scope，
删 node 会留下孤儿 scope；复合主键一行一产物，级联删除时按 `nodeId` 前缀扫一遍即可。

### 2.3 `tasks`（已有，B 扩展）

```jsonc
{ "id": "task-…", "kind": "node_build_artifact",
  "nodeId": "node-3f9a",            // 新增：派给谁。null = 控制层本地任务
  "claimedBy": "node-3f9a",          // 新增：谁认领的（null = 未认领）
  "claimEpoch": 17,                  // 新增：认领时的 sessionEpoch，防跨重启顶替
  "resourceKeys": ["node:node-3f9a", "base:sealed-9c1f…"],
  "params": { "baseId": "…", "sourceCommit": "…", "platform": "linux-x64" },
  "status": "queued", … }
```

`nodeId` 单独成字段而不是塞进 `resourceKeys`：`resourceKeys` 的语义是**锁范围**
（同一 key 的任务互斥），调度归属是另一件事；两者混用会让「只允许一个 base 构建」和
「只有这个 node 能领」变成同一种表达，日后必然打架。

认领 = `queued --claim--> running`，同时写 `claimedBy/claimEpoch/startedAt`；
任务结束时清空 `claimedBy` 但保留它作为历史（或写进 `params.finishedBy`）。

### 2.5 任务状态机：新增 `miss`

现有 `TaskState`（`box-scheduler/src/lib.rs`）有 queued/running/succeeded/failed/cancelled/
interrupted/rollingback/rolledback。**认领后失联**这件事今天无处可去：写成 `failed` 会
谎报"这项工作失败了"，写成 `interrupted` 会和"daemon 重启打断"混为一谈。新增：

```rust
Miss,   // 认领它的 node 不再持有它：epoch 变了、重注册时没上报、或 node 被移除
```

转移规则（要同步改 `valid_transitions`）：

| 从 | 到 | 触发 |
|---|---|---|
| `running` | `miss` | node 重注册时未在 `abandonedTaskIds` 里提到该任务，或 `claimEpoch < currentEpoch` |
| `queued` | `miss` | 目标 node 被移除（排给它的活没人能领） |
| `miss` | — | 终止态；**不自动重排**（构建可能已写半个归档，重排会让两个 worker 抢同一产物） |

`miss` 与既有状态的边界必须写进文档与 UI 文案：
`interrupted` = 控制层自己重启打断；`miss` = 节点侧不再持有；`cancelled` = 用户要求停。
三者在 UI 上是三种颜色/三种建议动作（重试 / 检查节点 / 无需处理）。

挂点：`TaskState::from`、`valid_transitions`、`is_terminal`、CLI 的状态列、`src/i18n.ts` 的中英文案
（漏一处就会出现任务永远显示"执行中"）。

### 2.4 容器落点：`container.json` 增量

```jsonc
{ "id": "cnt-…", "name": "…", "profile": "web",
  "nodeId": "host",                 // 新增；缺省 = host（旧容器无需改动）
  "runtime": "vm:wsl2" | null,       // 只有本文件存在；见 §5
  "placement": {                     // 新增：绑定即事实，不做隐式迁移
    "boundAt": 1794,
    "pinned": true,                  // 用户显式指定 node，调度器不得改选
    "reason": "pinned|auto|host-default"
  } }
```

为什么放文件而不放 documents：容器目录本身就是它的存在证明（删目录=删容器），
把落点写进 documents 就会出现「documents 有、目录没有」的二相性，而这正是
`resolve.rs` 已经在处理的孤儿场景；两者都在文件里，一致性只需一次 `read_to_string`。

**一个容器只在一个 node 上运行**：`container id` 是全局唯一且创建时固定的，落点因此是
一对一关系，不存在"同一容器在两个 node 上各跑一份"。跨 node 复制 = **新建一个容器（新 id）
在目标 node 上就位**，旧的原样留在原处；这与今天同机复制的语义一致（`free_record_id` 会给出 `-2`
后缀而不是覆盖）。由此得出一条推论：**任何"迁移"都必须是显式的建新+删旧**，不存在原地搬家。

## 3. 调度：三个不同的问题，三套规则

### 3.1 新建容器落在哪个 node（选址）

输入：boxfile/CLI 的 `nodeId?`、`capabilities` 需求、模板的 `baseId`。

```
若显式 nodeId：
   node 不存在 → 报错  |  status != online → 报错（列出当前状态）
   缺 capability 或平台不匹配 → 报错并列出缺哪一项
   → 绑定，pinned=true
否则：
   候选 = 所有 online 且 capability 满足的节点
   排序：host 优先（无跨边界开销）
        → readyFor(base) 已为真（产物已存在，省一次构建）
        → 已运行容器数最少
        → lastSeen 最新
   绑定第一个；无候选 → 报错，逐候选给出拒绝原因（缺产物 / 平台不符 / 离线）
```

产物缺失不是拒绝理由，而是**顺带派一个 `node_build_artifact`**（可等待、可后台）；
容器创建本身不阻塞在构建上。

### 3.2 产物在哪个 node 上构建（缓存）

键 `(nodeId, baseId)`，一个 node 一个 base 只编一次。请求到来时：
已有记录且 `platform == node.platform` → 直接复用；
已有记录但平台不符 → 视为陈旧，重新派活（不删旧行，写入时同 id 覆盖）；
无记录 → 派 `node_build_artifact`，`resourceKeys` 含 `base:<baseId>`，保证同一 base 不并发编两次。

跨 node 复制容器时**不复制产物**：产物是 (node, base) 级别的，新 node 走自己的缓存键，
只搬该容器的 `profile/`（约 62.5 MB）。

### 3.3 长任务由谁执行（认领）

拉取式：helper 用 session token 调 `node_claim_task`，daemon 取该 node 下最老的 `queued` 任务，
在**一个事务语义**内写 `running + claimedBy + claimEpoch` 再返回；返回后 helper 才真正开干。

为什么拉取而不是推送：断线、daemon 重启、事件丢帧都能自愈——helper 重连后继续 claim；
而推送要给每个 node 维护可重放的队列。SSE 只用来「有新活」的提示，不承担可靠性。

认领后的失联：`claimedBy` 非空且 `claimEpoch < currentEpoch`（daemon 重启过）的任务，
启动时统一置 `failed(error="daemon restarted during task")`，**不自动重排**：
构建可能已经写了一半个归档，重排会让两个 worker 同时写同一产物。

### 3.4 node 掉线对既有容器意味着什么

| 情况 | 处理 | 明确不做 |
|---|---|---|
| node `offline` | 其容器在状态里标 `unreachable`（派生，不写盘），页面入口显示原因 | 不自动迁移容器 |
| node 被移除 | 若仍有容器引用 → 拒绝，先 `container copy` 建新容器再删旧；移除时**默认删除产物索引行**；node 磁盘上的归档/缓存由 `node_purge` 异步任务清理（§3.6） | 不静默改绑到 host；不删 node 磁盘文件却报告已清理 |
| node `failed` | 同 offline，且拒绝新选址 | 不自动重装 node |

不自动迁移的理由：迁移会改变网络边界与文件系统语义（WSL2 netns 共享、drvfs 慢），
用户以为自己在一块盘上、其实换了一块，是最难查的一类 bug。迁移必须是显式动作。

### 3.5 跨 node 动作一律异步，控制层不等待

凡是会碰到某个 node 的磁盘、进程或网络的动作，**接口形态统一为「返回 handle，结果靠事件」**：

```
任何 node 侧动作（构建产物 / 清理磁盘 / 起停容器 / 同步镜像 / 开隧道）
   → RPC 立即返回 { taskId }（不阻塞、不等待）
   → 任务进 box-scheduler，nodeId 决定谁领
   → 订阅 task_* 事件（task_stage / task_log / task_finished）取结果
   → 幂等：同 (kind, nodeId, params) 的未完成任务直接返回既有 taskId
```

只有**纯控制层的读写**可以同步：`node_list/get`、`node_artifact_list`、注册/心跳/上报。
判据一句话：**会不会等一个 node**。会等的一律异步。

这条规则的直接好处是控制层永远没有「卡在某个 node 上」的线程——一个 node 的 SSH 挂了、
WSL 卡了、docker daemon 无响应，都只表现为它自己的任务停在 `running`，其它 node 照常。

### 3.6 node 磁盘上的东西，通过控制层发起清理

删除 node 只删索引行（控制层本来就碰不到那些文件），但**清理必须可达**，否则用户没有
手段回收一个已经不存在的 node 占着的几十 GB。因此提供一个异步动作：

```jsonc
// method: node_purge   （主 token；控制层发起，node 执行）
{ "nodeId": "node-3f9a",
  "what": ["artifacts", "mirror", "workspaces", "logs"],   // 显式列举，不接受 "all"
  "baseId": "sealed-9c1f…" }                               // 可选：只清某个 base 的产物
→ { "taskId": "task-…" }
```

- **不接受通配 `all`**：删磁盘上的东西必须显式列举，避免一个笔误清掉整块工作区。
- helper 执行、逐项回报（`node_report`），失败项留在结果里而不是静默跳过。
- node 已不存在时调用必须报 `node not found`，**不允许**留「幽灵清理」。
- 这一层今天先给 CLI 与 RPC；面向用户的资源控制界面在 D（面板）恢复时接上，接口形态不因此改变。

## 4. 不变量（每条都要有守护）

| # | 不变量 | 守护方式 |
|---|---|---|
| I1 | `artifact.platform == node.platform` | 写入校验 + 单测（回归防线，容易写成宿主平台） |
| I2 | 产物行只能由成功 report 产生 | 单测：任务失败后 `node_artifact_list` 为空 |
| I3 | 一个 node 不能替别的 node 报产物 | `__nodeId` 服务端注入（A 已有机制）+ 实机探针 |
| I4 | `(nodeId, baseId)` 唯一 | 主键编码 + 覆盖写语义 |
| I5 | `nodes.host` 不可删、不可 deregister | A 已有 |
| I6 | 容器引用的 node 必须存在 | 删除 node 前扫 `instances/*/container.json` |
| I7 | 认领者必须持有当前 epoch 的 session | `claimEpoch` 比对 + 单测 |
| I8 | 控制层不读 node 本地路径 | 代码评审约定：`archive` 只透传 |
| I9 | 一个容器只属于一个 node（id 全局唯一，无副本） | 容器 id 创建时固定 + 跨 node 复制走「新建」路径 |
| I10 | 跨 node 动作不阻塞控制层 | 接口评审：会等 node 的一律返回 `taskId` |
| I11 | 认领后失联不伪装成 failed | `TaskState::Miss` 专用状态 + 转移表单测 |

## 5. 迁移与兼容

- **不动 `PRAGMA user_version`**：只新增 scope（`node-artifacts`）与可选字段，`documents` 表结构不变。
- 所有新字段 `#[serde(default)]`；旧记录读出来就是「没有 nodeId」= host、"没有 mirror" = 未就绪。
- `container.json` 的 `runtime` 字段保留只读（旧容器/旧模板仍可能写它）；C 阶段写入新容器时
  同时写 `nodeId` 与 `runtime`，等确认没有旧版本再停写 `runtime`。
- 回滚：删掉新 scope 与两个新 RPC 即可，容器文件里多出来的 `nodeId` 被旧代码忽略（未知字段不报错）。

## 6. 查询与规模

`DocumentStore` 只能整 scope 读，因此每条查询的成本 = scope 大小。实际规模：
nodes ≤ 10、node-artifacts ≤ nodes × DSH 版本数（~50）、containers ≤ 数百、tasks 有界（已清）。
在这个量级下整读+过滤都在毫秒级。

**何时才需要真索引**：出现「按 node 查上万个 task」或「产物行 > 10k」时，
再加一张 typed 表（`MIGRATIONS` 追加一个脚本），而不是现在预建。

## 7. 失败语义

| 场景 | 行为 |
|---|---|
| daemon 重启 | `sessionEpoch++`；所有 node 降为 `registering/offline`（A 已实现）；已认领任务置 `interrupted`；产物行保留 |
| node 断线重连 | 用 enroll 重新注册（新 epoch），`claimedBy` 旧值失配 → 该任务不再属于它 |
| 产物平台不符 | 视为陈旧，重派构建；不复用、不删除旧行 |
| 归档磁盘不足 | helper 侧导出前预检（D: 仅 ~8.8 GB），不足则不写 `.part` 并回报明确错误 |
| 认领后 helper 崩溃 | 任务停在 `running`；**不按时间自动失败**。node 用新 epoch 重新注册时若没在 `abandonedTaskIds` 中认领它 → 置 `miss`（§2.5） |

## 8. 明确不存

- DSH 源码树、插件、profile（这些是 node 本地的，产物只按 base 缓存）
- node 本地绝对路径的语义（`archive` 是不透明句柄）
- DSH 宿主的端口号（每次启动随机，只存「意向」，真实端口由 node 侧在启动后回报）
- 宿主的 `build_platform()` 结果（这是 B 里最容易写错的一行）

## 9. 已定决策

| # | 问题 | 决定 | 落点 |
|---|---|---|---|
| D1 | 容器能否在两个 node 上并存副本 | **不能**。container id 创建时固定，一个容器只在一个 node 上运行；跨 node 复制 = 新建容器（新 id），旧的原样留在原处 | §2.4、I9 |
| D2 | 删 node 时产物与磁盘怎么处理 | 默认**删除索引行**；node 磁盘上的归档/镜像/工作区通过 `node_purge` 异步动作清理（显式列举，不接受 `all`）。将来提供面向用户的资源控制界面与操作接口来接这一层 | §3.4、§3.6 |
| D3 | 认领后失联怎么表示 | **用状态机解决**：`TaskState` 增加 `miss`，与 `failed`/`interrupted`/`cancelled` 并列且语义互斥；不按时间自动失败 | §2.5、I11 |
| D4 | 多节点管理的调用形态 | **统一异步**：凡会等某个 node 的动作一律返回 `taskId`，结果靠 `task_*` 事件；只有纯控制层读写同步 | §3.5、I10 |

由 D3/D4 派生两条实现纪律：

1. `miss` 必须同时改 `TaskState::from`、`valid_transitions`、`is_terminal`、CLI 状态列与
   `src/i18n.ts` 中英文案——漏一处就会出现任务永远显示「执行中」。
2. 任何新 RPC 先问一句「会不会等 node」；会等的就不许返回结果，只能返回 handle。

## 10. 阶段落点

| 阶段 | 用它做什么 |
|---|---|
| B1 | 建 `node-artifacts` scope；`node_build_artifact` / `node_artifact_list` / `node_artifact_report` / `node_claim_task` / `node_purge` 的 RPC 与 CLI；`tasks.nodeId/claimedBy/claimEpoch`；`TaskState::Miss` 与转移表；异步接口约定写进 `AGENTS.md` |
| B2 | helper 认领并执行（clone→install→build→tar→sha256→report），实现 `node_purge`，写 `nodes.mirror` / `nodes.toolchain` |
| C | `container.json` 增 `nodeId/placement`，选址与启动下沉 node；跨 node copy = 新建容器 |
| P | `nodes.relay` 填值，页面通过隧道回传 |
| D（可后置） | 面板与资源控制界面：node 在线状态、磁盘占用、`node_purge` 入口——接口在 B1 已经就位，这里只做界面 |
