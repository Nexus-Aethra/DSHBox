# 2026-11-07 — Node 持久化与跨 node 调度：数据结构设计（v3）

> 状态：**设计，未实现**。前序：`docs/design/2026-11-04-node-architecture-roadmap.md`（A 已合并）、
> `docs/design/2026-11-04-node-artifact.md`（B 的目标）、`docs/design/2026-11-04-shared-harness-mirror.md`（常量仓库）。
> v3 按独立评审（PR #32）的意见修正：任务状态机补全、daemon 重启与节点失联彻底分开、
> purge 与 tombstone 的可达性矛盾、异步边界的方向、以及 v1/v2 遗留的 8 条实现级缺口。
> 本文只定数据与调度，不写代码；B1 的施工以本文为准。

## 0. 一句话模型

每个 node 自带常量仓库与工具链，在本地完成 clone/install/build/run；控制层只持有**注册表、
产物索引、任务队列、relay 意向**四类记录，从不持有 node 本地路径的语义，也不替 node 做文件操作。

## 1. 存储分工：什么进 documents，什么留在文件

`DocumentStore` 是 `documents(scope, id, value)` 一张表，**没有二级索引**，只能整 scope 读回再过滤
(`box-foundation/src/collection.rs:29-38`, `box-store/src/document.rs:16-23`)。判据只有一条：
**权威数据归谁写**。

| 数据 | 归属 | 落点 | 理由 |
|---|---|---|---|
| node 注册表（含 tombstone） | 控制层 | documents `scope=nodes` | 控制层是唯一写者 |
| 产物索引 | 控制层（校验格式后写） | documents `scope=node-artifacts` | 摘要与大小由 node 计算、控制层校验形状 |
| 任务队列（含清理意向与结果） | 控制层 | documents `scope=tasks` | 调度器唯一写者；D4 之后所有 node 动作都在这里 |
| 容器身份 + 落点 | 执行侧（文件即事实） | `instances/<id>/container.json` | 容器目录本身就是它的生命 |
| 常量仓库 / 产物归档 / profile | node | node 本地磁盘 | 控制层只存**不透明句柄**与摘要 |

两条硬规则：

1. **控制层不许把 `archive` 当路径去 stat**（I8）。它是句柄，只有产生它的 node 能解释。
2. **清理意向与结果也是记录**：谁要删、删哪些、删成功几项，都在 `tasks` 里留痕（§3.6）。

## 2. 表结构

### 2.1 `nodes`（A 已有，B 扩展）

```jsonc
{
  "id": "host" | "node-3f9a",          // 主键；host 内建，不可删
  "kind": "host|wsl2|libkrun|vz|docker",
  "platform": "windows-x86_64|linux-x86_64",  // 见 §2.7 的拼法约定
  "status": "registering|online|offline|draining|failed|removed",
  "capabilities": ["exec","build","image-export","copy","dsh-host"],
  "controlUrl": "http://127.0.0.1:5321" | null,
  "sessionEpoch": 17,                  // 每次 daemon 启动 +1
  "helperSession": "9f2c-…",           // helper 进程启动时自己生成的实例号（daemon 只比较，不发号）
  "enrolledAt": 1793, "lastSeen": 1794, "helperVersion": "0.1.10",
  "backendRef": "Ubuntu-24.04" | null,
  "hostNode": true,
  "mirror": { "path": "/var/dshbox/harness-mirror.git", "commit": "0d1f500…", "fetchedAt": 1793, "sizeBytes": 412000000 },
  "toolchain": { "node": "22.13.0", "pnpm": "10.4.1", "platform": "linux-x86_64" },
  "workload": {                        // node 心跳上报的摘要，控制层不去远端扫盘
    "containers": 3, "runningHosts": 2, "diskFreeBytes": 8100000000
  },
  "relay": null,                       // P 阶段：{ "listenerPortHint": 0, "lastTunnelAt": 1794 }
  "removedAt": null,                   // tombstone 时间；非 null 即 status=removed
  "lastArtifacts": [                   // tombstone 清单：删索引行之后仍能枚举清理目标
    { "baseId": "base-9c1f…", "archive": "/var/dshbox/artifacts/base-9c1f-<taskId>.tar", "sizeBytes": 1327000000 }
  ]
}
```

**派生就绪（不落盘）**：`readyFor(base) = status==online && mirror.commit == base.sourceCommit && toolchain.platform == platform`。

**状态机**：`registering →(register)→ online →(3 次心跳丢失)→ offline →(register)→ online`；
`online →(drain)→ draining →(最后任务结束)→ offline`；任何状态 `→(不可恢复错误)→ failed`；
`任一 →(node rm)→ removed`（tombstone，保留记录与 `lastArtifacts`）。

### 2.2 `node-artifacts`（B 新增）

主键必须是单字符串（`documents.id` 是 `TEXT`），用 `nodeId|baseId`：

```jsonc
{
  "id": "node-3f9a|base-9c1f…",       // nodeId + '|' + baseId
  "nodeId": "node-3f9a",
  "baseId": "base-9c1f…",             // prepared base 的 id，形如 base-<digest>（versions.rs:353）
  "sourceCommit": "0d1f500…",
  "platform": "linux-x86_64",          // 必须等于 node.platform
  "archive": "/var/dshbox/artifacts/base-9c1f-<taskId>.tar",   // 不透明句柄；按任务隔离，见 §3.4
  "sha256": "9f2c…（64 位小写十六进制）",
  "sizeBytes": 1327000000,
  "builtAt": 1794
}
```

校验（写入前，拒绝式）：`nodeId` 存在于 `nodes` 且非 `removed`；`platform == node.platform`；
`sha256` 为 64 位小写十六进制；`sizeBytes > 0`；`baseId` 形如 `base-<16 位小写 hex>`
（B1-b1 实现按 `versions.rs:353` 的 `{:016x}` 与 `prepared_base_dir` 的字母表收紧为精确 16 位，
比本文最初的「以 `base-` 开头且不含 `|`」更严：控制层不应持有 node 侧目录解析会拒绝的行）；
文本字段非空且 ≤ 4 KiB。**只有 `node_artifact_report` 走完校验才写行**；任务失败只留 `tasks` 记录。

不再存 `verifiedAt`：控制层不 stat node 路径（I8），所以它无法“核对归档摘要”。
摘要由 node 在构建时计算并在使用时（C 阶段，真正解包前）复算；控制层只校验**形状**并记录。
这是对 `node-artifact.md:62-65`（“控制层校验后再入库…核对磁盘上的实际摘要”）的修订，理由见 §11。

### 2.3 `tasks`（已有，B 扩展）

```jsonc
{ "id": "task-…", "kind": "node_build_artifact|node_purge|node_sync_mirror|node_container_start|…",
  "nodeId": "node-3f9a",          // 派给谁；"host" 表示控制层内执行（§2.6）
  "claimedBy": "node-3f9a",        // 谁认领的；null = 未认领
  "claimSession": "9f2c-…",        // 认领时 node 的 helperSession；与当前 helperSession 不等即失联
  "idempotencyKey": "opt-64字符内", // 调用方给，用于去重（§3.5）
  "resourceKeys": ["node:node-3f9a", "base:base-9c1f…"],
  "params": { "baseId": "…", "sourceCommit": "…", "platform": "linux-x86_64" },  // ≤ 4 KiB
  "status": "queued", … }
```

`nodeId` 单独成字段而不是塞进 `resourceKeys`：`resourceKeys` 的语义是**锁范围**（同一 key 的任务互斥），
调度归属是另一件事；混用会让「同一 base 只编一次」和「只有这个 node 能领」变成同一种表达。

**授权面**：新增的 node 侧方法必须同时进 `NODE_METHODS`（`nodes.rs:72-77`）—— `node_claim_task`、
`node_artifact_report`，以及 B2 的清理进度回报。漏一个，helper 第一次调用就是 403；这是 B1 实机验证的第一件事。

### 2.4 容器落点：`container.json` 增量

```jsonc
{ "id": "cnt-…", "name": "…", "profile": "web",
  "nodeId": "host",                 // 新增；缺省 = host（旧容器无需改动）
  "runtime": "vm:wsl2" | null,       // 保留只读，见 §5
  "placement": { "boundAt": 1794, "pinned": true, "reason": "pinned|auto|host-default" } }
```

为什么放文件：容器目录就是它的存在证明（删目录=删容器），写进 documents 会形成「记录在、目录没了」
的二相性，而这正是 `resolve.rs` 已在处理的孤儿场景。

**一个容器只在一个 node 上运行**：`container id` 全局唯一且创建时固定，落点是一对一关系。
跨 node 复制 = **在目标 node 新建一个容器（新 id）**，旧的原样留着（与今天同机复制 `free_record_id` 给 `-2` 后缀一致）。
推论：**任何“迁移”都是显式建新+删旧**，不存在原地搬家 —— 也因此 node 掉线时不自动迁移（§3.4）。

### 2.5 任务状态机（完整版，含 `miss`）

现有 `TaskState`（`box-scheduler/src/lib.rs:59-78`）有 queued/running/succeeded/failed/cancelled/
interrupted/rollingback/rolledback。新增一个终态：

```rust
Miss,   // 认领它的 node 不再持有它：换会话、被移除、或控制层发现它已不可达
```

**`helperSession` 必须由 helper 生成，不能由 daemon 发号**：re-enroll 一个正在构建的节点是被测试固定过的
合法路径（`nodes.rs:729-747`），控制层主动 re-enroll 会让“发号 +1”把一个活着的构建判成 `miss`，
而 `miss` 是终态、完成后的 report 会被 I12 丢弃 —— 结果是产物没记账、磁盘上留一个没人管的归档。
helper 在进程启动时生成一个实例号，注册时上报；daemon 只存与比较。进程换了实例 = 换了 `helperSession` = 原 claim 失联，
这才是 `miss` 的正确判据。

**三种“没做完”必须互斥**，否则 UI 无法给出正确建议：

| 状态 | 含义 | 谁造成 | UI 建议 |
|---|---|---|---|
| `interrupted` | 控制层自己重启，任务被打断 | daemon | 重试 |
| `miss` | node 侧不再持有该任务 | node | 检查节点 |
| `cancelled` | 用户要求停 | 用户 | 无需处理 |

完整转移表：

| 从 | 到 | 触发条件 |
|---|---|---|
| `queued` | `running` | node 认领 |
| `queued` | `miss` | 目标 node 被移除（`status=removed`）——这活没人能领了 |
| `queued` | `cancelled` | 用户取消 |
| `queued` | `interrupted` | daemon 重启 |
| `running` | `succeeded`/`failed` | node 回报（且 `claimSession` 与当前一致） |
| `running` | `miss` | 同一 node 带有**不同的 `helperSession`** 重新注册（进程换了实例），且该任务不在它上报的 `abandonedTaskIds` 里 |
| `running` | `miss` | 同一 `helperSession` 上报 `abandonedTaskIds` **包含**该任务（它自己承认丢了） |
| `running` | `miss` | 目标 node 被移除 |
| `running` | `cancelled` | 用户取消（经 §3.3 的取消通道送达 node） |
| `running` | `interrupted` | daemon 重启（`restore()` 统一处理） |
| `miss` | — | 终态，**不自动重排**（构建可能已写了半个归档，重排会让两个 worker 抢同一产物） |

两条**状态机之外**的实现纪律（评审 B4 指出）：

1. `finish()` 曾经会 `transition_to().ok()` 吞掉转移错误再无条件写 `stage="Completed"`，于是
   `miss` 之后迟到的回报会把记录写成 **miss + Completed + 100%**。**已实现（#35）**：只有
   `from.can_transition_to(target)` 成立才写状态，`Miss` 没有出边，终态任务收到迟到的 report
   一律忽略并记日志（`tracing::warn!`）。
2. `restore()` 在 daemon 启动时把 `running` 钉成 `interrupted`（终态），所以**“失联判 miss”的扫描
   永远不会看到它们** —— 这是有意的：重启是控制层自己的原因，归 `interrupted`。`miss` 只在
   daemon 存活期间由节点事件触发。两条路径不许互相认领。

### 2.6 `host` node 的任务在控制层内执行

`host` 是内建节点，**没有 helper、没有 session、无法认领任务**（A 的实现如此）。因此：

- `tasks.nodeId == "host"` 由控制层内的本地执行器跑（沿用 `box_scheduler::run_queued` 的既有路径）；
- `node_claim_task` 只对非 host 节点有效，host 节点调用返回明确错误而不是空列表；
- 选址排序里 host 优先的前提是“本地执行器能跑这类任务”，`node_purge` 这类只对远端有意义的
  动作在 host 上直接拒绝（本机文件用户自己就能删）。

### 2.7 platform 拼法：统一，并且校验

今天有**三种拼法**并存，是 B 里最容易出错的一处：

| 出处 | 形式 | 例 |
|---|---|---|
| `nodes.rs:182`（host node） | `{OS}-{ARCH}`（`std::env::consts`） | `windows-x86_64` |
| `bundled.rs`/打包目录 | 包目标名 | `win-x64` / `linux-x64` |
| 早期文档 | 手写 | `windows-x64` |

**约定**：`nodes.platform` 与 `node-artifacts.platform` **一律用 `{OS}-{ARCH}`**（即 `std::env::consts` 的拼法，
取 helper 自己编译时的值，不取宿主），enroll 时用白名单校验（`^(linux|windows|macos)-(x86_64|aarch64)$`），
不合法直接拒绝注册。打包目录名（`win-x64`）是**包目标**，与 node platform 是两个概念，不得混用。

**平台值必须由 helper 自报**：今天 `platform` 是 enroll 调用方（供给适配器）填的（`nodes_rpc.rs:13` →
grant → `nodes.rs:295-312`），helper 从来没被问过。B1 要求 `node_register` 报文里带 `platform`，
daemon 用它覆盖 grant 里的值并与白名单比对；否则白名单校验的只是适配器打的字，不是在跑的那台机器。

## 3. 调度

### 3.1 新建容器落在哪个 node（选址）

```
若显式 nodeId：
   node 不存在 → 报错 | status 不在 {online,draining} → 报错（列出当前状态）
   缺 capability / 平台不匹配 / base 产物缺失且无法构建 → 报错并列出缺哪一项
   → 绑定，pinned=true
否则：
   候选 = status==online 且 platform 兼容 且 capability 满足 的节点
   排序：host 优先（本地执行器可跑）→ readyFor(base) 已真（省一次构建）
        → 已运行容器数最少（nodes.workload）→ lastSeen 最新
   绑定第一个；无候选 → 报错并逐候选给出拒绝原因
```

**控制层不主动去远端数容器**：`nodes.workload` 由心跳携带（§2.1），所以排序只用控制层已有数字，
不会因为某个 node 卡住而让 `list_containers` 变慢（桌面 3s 轮询不允许）。

### 3.2 产物在哪个 node 上构建（缓存）

键 `(nodeId, baseId)`，一个 node 一个 base 只编一次：已有记录且 `platform` 相符 → 复用；
已有记录但平台不符 → 视为陈旧重新派活（同 id 覆盖）；无记录 → 派 `node_build_artifact`，
`resourceKeys` 含 `base:<baseId>`，保证同一 base 不并发编两次。跨 node 复制容器**不复制产物**，只搬 `profile/`。

### 3.3 认领、取消、以及“方向”

**认领是 node → 控制层的同步调用**（node 问“有我的活吗”），不吃 §3.5 的异步规则：
`node_claim_task` 在一个调度器锁内把 `queued → running` 并写 `claimedBy/claimSession/startedAt`，
然后返回任务；返回后 helper 才开干。控制层的任务持久化必须由调度器的单一写者完成，
不能在内存改完再整表 `persist()` 覆盖（评审 B7：认领写入会被覆盖；这个整表写方法现在**已经删除**，见下）。

落地方式（已实现）：单条改动只写自己那一行——`TaskManager::persist_row`，在持锁期间完成；
一次关闭多条的扫描（`interrupt_running`）写它改过的那一批 `persist_rows`；**整表写的方法已经
删掉**（不是留着没人用，而是根本没得调），`restore()` 把整表读进来、只写它关掉的那几行。
为什么不能“在锁里调整表写”：整表写内部会 `list()`（再取同一把锁），会死锁——这正是要单独有
`persist_row` 的原因。测试：13 个单条写点由一张表逐点钉住（经由同一个 store 上的第二个
`TaskManager` 推进某个任务，再改动另一个任务，磁盘上那一行必须还是别人写的），批量那条单独
一条测试**从磁盘读回**（读 `manager.list()` 是自证的，测不到落盘）。`restore()` 也只写它自己关掉的那几行、并且只扫**本进程拥有**的任务（`Sweep`：
daemon 扫全部，桌面端只扫自己入队的 kind）——桌面与 daemon 共用一个 store，整表扫会把 daemon
正在跑的任务在库里写成 `interrupted`。事件通知器（daemon 的 `DaemonNotifier`、桌面端的
`emit_task_update`）同样不写库：
它们每个 stage tick 都会跑，且在事件里整表快照就等于把最高频的那条竞态路径重新装回去。
唯一既有 caveat：行写失败时内存已经改过、错误照常上报，所以一条记录可能在内存里是新的、
磁盘上还是旧的，直到下一次 mutation 重写它——这不是本设计引入的，但值得记住。

断线重连、daemon 重启、事件丢帧都自愈：helper 重连后继续 claim。SSE 只用于“有新活”的提示，不承担可靠性。

**取消必须有到 node 的通道**（评审 B5）：认领响应同时带 `{ tasks: [...], cancels: [taskId…] }`，
helper 每次轮询（心跳周期）都取一次待取消列表并对本地进程树发终止。没有这条通道，
`cancel_requested` 只会写进记录，实际构建照跑。

### 3.4 node 掉线、移除：先 tombstone，再谈清理

| 情况 | 处理 | 明确不做 |
|---|---|---|
| node `offline` | 其容器在视图里标 `unreachable`（**只用控制层记录推导**，不现问 node）；其 `running` 任务要等 node 回来或 `node rm` 才离开 `running`（取消通道要 helper 轮询，helper 死了收不到） | 不自动迁移容器 |
| node 有容器引用时移除 | 默认拒绝，提示先 `container copy` 建新容器再删旧 | 不静默改绑到 host |
| node 被移除 | 转 `status=removed` 的 **tombstone**：保留 `backendRef/lastSeen/lastArtifacts`，清空 `controlUrl`；**删产物索引行**（枚举从此走 `lastArtifacts`）；其 `queued/running` 任务置 `miss` | 不删 node 磁盘文件却报告已清理 |
| node `failed` | 同 offline，且拒绝新选址；状态机里 **failed 没有出边**，排给它的 `queued` 只能靠用户取消或 `node rm` 释放 | 不自动重装 node |

**产物路径按任务隔离**：归档落 `artifacts/base-<digest>-<taskId>.tar.part`，成功后 rename 成同名去 `.part`。
这样「手动重试」与「自动重排」的危险就被结构消掉了 —— 两个任务各有自己的路径，不会有两个 worker 写同一 tar、
摘要对着交错文件算。再加一道保险：helper 在 rename 前自检自己仍持有该 `claimSession`，不持有则丢弃产出并回报。

**为什么要 tombstone**：把「索引行」和「那台机器上的字节」分开记账。删索引行解决的是控制层视图，
`lastArtifacts` 保留了清理所需的枚举依据，两者都不影响 `node rm` 恢复成干净状态。

### 3.5 异步边界：控制层→node 才异步，node→控制层是同步

规则只约束**控制层发起、会等某个 node 的动作**：

```
控制层 → node（构建产物 / 清理磁盘 / 同步镜像 / 起停容器 / 开隧道）
   → RPC 立即返回 { taskId }，绝不等待
   → 任务进 box-scheduler，nodeId 决定谁领
   → 订阅 task_* 事件取结果
node → 控制层（register / heartbeat / claim / report / artifact_report）
   → 同步，天然短小；它们本身就是控制层状态的输入
纯控制层读写（node_list/get、node_artifact_list）
   → 同步
```

**还有第三类：大流量的 node→control 字节流**——`/mirror.bundle`（几十 MB 起）与该平台工具链（数百 MB）。
它既不是同步 RPC（会占住 helper 的一次请求），也不是 task（结果不是一次状态转移，而是一个文件）：
走 HTTP 流式下载、可断点续传、按 sha256 校验，失败从断点继续。sha256 由控制面在生成 bundle/打包工具链时
计算并写进响应头（`X-Bundle-Sha256`），node 校验通过才解包 —— 与 `latest.json` 同一条规矩：没有摘要的字节不落地。

两条附加约束，都是为了不让“异步”变成“卡死”：

1. **派生状态不得实时问 node**：`unreachable`、远端容器数、磁盘余量一律来自控制层已存字段
   （心跳上报的 `workload`）；否则桌面 3s 轮询的 `list_containers` 会被一个卡住的 node 拖死。
2. **幂等键由调用方给**（`idempotencyKey`，≤64 字符），去重元组是 **(kind, nodeId, idempotencyKey)** —— 只用 key 会让同一个 key 发给不同节点或不同动作时拿回不相关的 taskId。不做 `params` 深比较（`params` 是自由 JSON，
   深比较 + 整表 upsert 会破坏 §6 的毫秒级论证）。去重只针对**活跃态** `{queued, running}`；
   `succeeded/failed/cancelled/interrupted/miss` 一律不阻止重派——否则一次 `miss` 会永久堵死同一 key。
   另：`params` ≤ 4 KiB —— **已实现（#37）**：`enqueue_for_node` 与通用 `enqueue_task` RPC 都在边界拦，
   超限直接拒绝（`MAX_PARAMS_BYTES` / `params_size`），拒绝时不留 log 文件、不入队。

### 3.6 `node_purge`：清理 node 磁盘，接受可达性现实

```jsonc
// method: node_purge（主 token；控制层发起，node 执行，异步返回 taskId）
{ "nodeId": "node-3f9a",
  "what": ["artifacts", "mirror", "workspaces", "logs"],   // 显式列举，不接受 "all"
  "baseId": "base-9c1f…" }                                 // 可选：只清某个 base 的产物
→ { "taskId": "task-…" }
```

**可达性是前提，不是细节**：磁盘在 node 上，node 不可达就没人能删。因此：

| node 状态 | `node_purge` 行为 |
|---|---|
| `online` / `draining` | 派任务，逐项执行，失败项留在结果里（不静默跳过） |
| `offline` / `failed` | 拒绝，返回可执行的后端命令（如 `wsl --unregister <distro>`）与剩余清单 |
| `removed`（tombstone） | 同上；`lastArtifacts` 是返回清单的来源 |

顺序：**清理成功之后才允许删 `lastArtifacts`**。产物**索引行**在 `node rm` 时就删（用户视图归零），
枚举依据改由 tombstone 的 `lastArtifacts` 承担 —— 两者不是同一个东西：索引行是“控制层认为这个产物可用”，
`lastArtifacts` 是“这台机器上曾经有什么、要清理时按这个清单来”。删索引行而留着清单，
既让视图干净，又不丢清理依据。

`lastArtifacts` 只列产物，不含 mirror/workspaces/logs。这不是漏洞：node 变成 tombstone 后
**helper 已经不在了**，逐项清理无从执行；此时只有整机级清理——
`node purge --backend`（走供给适配器，如 `wsl --unregister <backendRef>`）一次带走全部——镜像、工作区、日志与产物都在那个 distro 里，所以 artifacts 之外的三类不需要逐目清理。

`node rm` 与 `node purge` 因此是两个动作：前者改变控制层视图（tombstone），后者负责字节。
面向用户的资源控制界面（D 阶段恢复时）驱动同一个 `node_purge`，接口形态不因界面而变。

**tombstone 不能只增不减**：purge 成功（或用户显式 `node forget`）后删掉 tombstone，
否则 §6 的「nodes ≤ 10」要把历史 removed 行算进去。**removed 的 id 不复用**：同一台机器重新加入时
申请新 id，旧 tombstone 留着只为清理与审计。

## 4. 不变量（每条都要有守护）

| # | 不变量 | 守护方式 |
|---|---|---|
| I1 | `artifact.platform == node.platform` | 写入校验 + 单测（回归防线：写成宿主平台就复现原始 bug） |
| I2 | 产物行只能由成功 report 产生 | 单测：任务失败后 `node_artifact_list` 为空 |
| I3 | 一个 node 不能替别的 node 报产物 | `__nodeId` 是服务端保留字段：网关先剥离请求体的值，再用已验证 session 绑定（B1-b1 实现）；主 token 无 session 故根本不能报 + 真实 loopback 测试（删剥离即红）+ 实机探针 |
| I4 | `(nodeId, baseId)` 唯一 | 主键编码 + 覆盖写 |
| I5 | `nodes.host` 不可删、不可 deregister、不参与 claim | A 已有 + §2.6 |
| I6 | 容器引用的 node 必须存在且非 removed | 删除前查 `instances/*/container.json`（本机）+ 远端靠心跳 `workload` + 离线时拒绝删除 |
| I7 | 认领者必须持有当前 `helperSession` 且 `sessionEpoch` 未变 | 认领时写入 + 单测 |
| I8 | 控制层不读 node 本地路径 | 评审约定：`archive` 只透传；不存 `verifiedAt` |
| I9 | 一个容器只属于一个 node（id 全局唯一，无副本） | 容器 id 创建时固定；跨 node 复制走新建 |
| I10 | 控制层不等待 node | 接口评审：会等 node 的一律返回 `taskId`；派生状态不现问 |
| I11 | 失联不伪装成 failed | `TaskState::Miss` + 完整转移表 + 单测 |
| I12 | 终态任务不受迟到回报影响 | `finish()/stage_*` 先判终态；单测（miss 后 report 不改记录） |
| I13 | 清理先于删 `lastArtifacts` | 成功的 purge 才可清 `lastArtifacts`；`node rm` 删索引行但**不**删清单；单测 |

## 5. 迁移、兼容与回滚

- **不动 `PRAGMA user_version`**：只新增 scope（`node-artifacts`）与可选字段；`MIGRATIONS` 仍只有 v1
  （`box-store/src/document.rs:25-29`），新 scope 只是新行。
- 所有新字段 `#[serde(default)]`；旧记录读出来 = 没有 `nodeId`（host）、没有 `mirror`（未就绪）、
  没有 `helperSession`（按空串处理：任何 claim 都对不上 = 未认领）。
- `container.json` 的 `runtime` 保留只读；**C 阶段**（不是 B）新容器同时写 `nodeId` 与 `runtime`，确认没有旧版本后再停写。
- **回滚不是完全无损**：`TaskState::from` 对未知状态回退 `Failed`（`box-scheduler/src/lib.rs:130-141`），
  所以回滚到旧 daemon 后，`miss` 会被读成 `failed` —— 与 I11 冲突但不会崩。B1 的 PR 必须写清这一点，
  并且**不与旧版本混跑**（daemon 与桌面同批重建的既有规定已经覆盖）。

## 6. 查询与规模

`DocumentStore` 只能整 scope 读，成本 = scope 大小。当前量级：nodes ≤ 10、node-artifacts ≤ ~50、
containers ≤ 数百、tasks **会持续增长**。前三个是毫秒级；**tasks 必须有 prune**（按 `finishedAt` + 条数上限），
否则 D4 之后每个 node 动作都留一行，`load_scope("tasks")` 会慢慢变成瓶颈（评审 d 指出“已清”缺依据）。

**何时才需要真索引**：出现「按 node 查上万 task」或「产物行 > 10k」时再加 typed 表（追加一个 migration），
而不是现在预建。

## 7. 失败语义

| 场景 | 行为 |
|---|---|
| daemon 重启 | `sessionEpoch++`；node 降为 `registering/offline`（A 已实现）；`running` 任务 → `interrupted`；产物行与 tombstone 保留 |
| helper 崩溃后重新注册 | `helperSession` 换成新值（epoch 不变——重新注册是合法路径）；原 `claimSession` 失配的任务 → `miss`（`abandonedTaskIds` 只是佐证） |
| node 被移除 | tombstone；其 `queued/running` → `miss`；索引行删除；`lastArtifacts` 留待清理 |
| 产物平台不符 | 视为陈旧重派；不删旧行（同 id 覆盖） |
| 归档磁盘不足 | helper 导出前预检（D: 仅 ~8.8 GB），不足则不写 `.part` 并回报明确错误 |
| `miss` 之后迟到回报 | 忽略并记日志（I12） |

## 8. 明确不存

- DSH 源码树、插件、profile（node 本地；产物只按 base 缓存）
- node 本地绝对路径的语义（`archive` 是句柄）
- DSH 宿主的端口号（每次启动随机；只存 relay 意向，真实端口由 node 回报）
- 宿主的 `build_platform()` 结果（B 段最容易写错的一行）

## 9. 已定决策

| # | 问题 | 决定 | 落点 |
|---|---|---|---|
| D1 | 容器能否在两个 node 上并存副本 | **不能**；id 创建时固定，跨 node 复制 = 新建容器 | §2.4、I9 |
| D2 | 删 node 与磁盘清理 | 删 node = tombstone + 删索引行；磁盘字节由 `node_purge` 异步清理（显式列举、接受可达性现实、先清后删）；将来由资源控制界面驱动 | §3.4、§3.6、I13 |
| D3 | 认领后失联 | 新增 `TaskState::Miss`，与 `failed`/`interrupted`/`cancelled` 互斥；不按时间自动失败 | §2.5、I11、I12 |
| D4 | 多节点管理调用形态 | 控制层→node 一律异步返回 `taskId`；node→控制层与纯控制层读写同步 | §3.5、I10 |
| D5 | node 上的可执行体由谁投放 | 受管后端由**本机**供给适配器投放（操作进程/rootfs，属供给而非连接）；用户设备装节点载荷后 `dshbox node join`；控制面**默认不做网络入站连接**（不做 ssh/scp 推送；反向连自 2026-11-08 起是可选能力，见 `node-link` §6.4）。节点角色用无 Tauri 依赖的 `box-helper`，与 control 同包发布 | §12 |

## 10. 阶段落点

| 阶段 | 内容 |
|---|---|
| B1 | `node-artifacts` scope；**把 A 的 `deregister` 从「删记录」改成「转 tombstone」**（会动既有测试 `nodes.rs:749-758`，需同批更新）；`node_build_artifact` / `node_artifact_list` / `node_artifact_report` / `node_claim_task`（同步）/ `node_purge`；`tasks.nodeId/claimedBy/claimSession/idempotencyKey`；`TaskState::Miss` + 完整转移表 + 终态保护；tombstone 与 `lastArtifacts`；`tasks` prune；异步优先写进 `AGENTS.md` |
| B2 | helper 认领并执行（clone→install→build→tar→sha256→report）、执行 `node_purge`、上报 `mirror/toolchain/workload`、接收取消列表（**2026-11-08 起改为下行帧直推，轮询作废**：`node-link` §6.2/L4）；`GET /mirror.bundle` 与该平台工具链的下载校验（sha256 由控制面生成时计算、写进响应头）；`dshbox node join` 薄壳（`box-helper` 的构建与安装包投放已由 PR #34 完成，不在此阶段） |
| C | `container.json` 增 `nodeId/placement`；选址与启动下沉 node；跨 node copy = 新建容器 |
| P | `nodes.relay` 填值，页面通过隧道回传 |
| D（可后置） | 面板与资源控制界面：node 在线状态、磁盘占用、`node_purge` 入口——接口在 B1 已就位 |

## 11. 与 `node-artifact.md` 的两处差异（需要同批更新那份文档）

1. **信任模型**：那份文档写「控制层校验后再入库…按需核对归档在磁盘上的实际摘要」。控制层到不了
   node 的路径（I8），所以改为：node 算摘要并上报，控制层校验**形状**（长度/字符集/大小/平台）；
   真正解包前的复算发生在 node 侧（C 阶段）。原文的动机（“helper 说编好了不说明字节就是我们想要的”）
   仍然成立，只是执行者从控制层换成了 node 自己 + 平台的构建可复现性。
2. **`baseId` 拼法**：那份文档示例写作 `sealed-…`，实际 prepared base id 是 `base-<digest>`
   （`versions.rs:353`），`sealed-*` 是**模板**（`sealed.rs:1035`）。按旧示例实现会把产物行数放大 N 倍。

## 12. 节点怎么就位：谁来把可执行体放到那台机器上

分层的代价是 node 上必须有东西在跑。三条可能的做法，结论先说：**控制面默认不做网络入站连接**——
它不监听公网；可执行体由「能进去的那一方」投放。

> **2026-11-08 变更**：原文这一句还写着「也不去 dial 一个 node」，已被
> `docs/design/2026-11-08-node-link-session-and-crate-layering.md` §6.4 取代——反向连（control → node）
> 作为**可选**能力存在（默认关闭、需显式绑定地址、走同一套握手认证）。ssh/scp 推送仍然不做。

受管后端由**本机供给适配器**
直接操作进程与 rootfs（那是供给，不是连接；`vm/wsl.rs` 的 `wsl -d … --exec` 与 `--unregister` 就是这一类），
用户设备装好之后由 node 自己出站 join。

### 12.1 三种投放路径

| 路径 | 谁投放 | 怎么做 | 适用 |
|---|---|---|---|
| **受管后端** | 本机供给适配器 | 两条都可行：把 helper 打进 rootfs 再 `wsl --import`（需重打 rootfs），或沿用现状——导入官方 rootfs 后从宿主侧用 `wsl -d … --exec` 注入（`vm/wsl.rs:249,306`）。docker 镜像 `COPY`；libkrun/VZ 的 rootfs 自带 | 控制面能创建/销毁的环境，用户零操作 |
| **用户设备** | 用户自己 | 装**节点载荷**（无头设备不必装带 webkit 的桌面包：直接拷 `box-helper`，或用安装包的 node 组件），再跑 `dshbox node join --control <url> --grant <一次性 token>` | 另一台 PC、服务器；需先开绑定地址（§12.4） |
| 控制面推（不做） | — | ssh/scp 到设备 | 要求长期凭据与 inbound 反向通道；**2026-11-08 起「反向连」另立为可选能力**（`node-link` §6.4，需显式开启、走握手认证），但 ssh/scp 这种「用控制面的凭据进设备」仍然不做 |

### 12.2 一个安装包，两种角色

`dshbox` 今天把控制面（CLI + daemon + 桌面）与操作面熔在一起。分层之后同一个安装里需要两种角色：

```
控制角色  dshbox / dshboxd     注册表、派活、页面回传、桌面
节点角色  box-helper           只注册/心跳/认领/执行/回报；不开窗口、不起 daemon、**默认不监听入站**（§12.4 的可选反向连除外）
```

**节点角色用 `box-helper` 这个独立小二进制**（已存在，236 行，无 Tauri 依赖），而不是给 `dshbox` 加 `--headless`：

- `dshbox` 无条件依赖 Tauri，Linux 上意味着要链接 webkit2gtk：docker 镜像与 WSL rootfs 会因此背几百 MB
  运行时库，而节点角色一个窗口都不开；
- `box-helper` 已是 workspace 成员。**构建与打包已由 PR #34 补上**：`scripts/prepare-server.mjs` 与 sidecar
  同一次 cargo 调用编出、作为 `helper/<target>/` 进入安装包（本节初稿曾写「没有任何构建/打包引用」，那是当时的事实，现已成历史）。
  B2 剩下的活是 `dshbox node join` 的薄壳（写配置 + 起进程，不重复实现注册逻辑）与真正把它跑起来；
- 版本一致性靠既有构建戳纪律（「daemon 与桌面同一批」），node 与 control 同包发布；注册时用 `helperVersion`
  拒绝不兼容（roadmap 风险表已列）。

### 12.3 节点还需要两样东西，不只是二进制

| 需要 | 从哪来 | 时机 |
|---|---|---|
| 常量仓库（`harness-mirror.git`） | control 的 `GET /mirror.bundle`：node 拉、校验 sha256、解出裸库 | join 后第一个异步任务 |
| 该平台工具链（node/pnpm） | 按 `runtime-lock.json` 的校验和从 control 取，或随安装包带 | 同上 |

所以「把二进制传过去」实际是三件：**二进制 + 常量仓库 + 工具链**。后两件由 node 自己取，control 只给地址与摘要；
这也让「节点在哪个网络里」变得无关——只要能出站连到 control，它就能把自己武装起来。

### 12.4 安全与可达性

- **node 出站为主，反向连为可选**（2026-11-08 变更）：注册、心跳、认领、上报仍由 node 发起；
  但 `docs/design/2026-11-08-node-link-session-and-crate-layering.md` 新增了「控制面 dial node」的
  备路径（默认关闭、需显式绑定地址、同样走握手认证）。原文「控制面从不 dial node」的绝对化表述
  以那份文档为准。P 阶段的 relay 隧道仍由 node 发起。
- **控制面的监听面今天是纯回环**：`TcpListener::bind("127.0.0.1:0")`（`main.rs:117`），HTTP 面只有
  `POST /rpc` 与 `GET /events`（`main.rs:229,313`）；`/mirror.bundle` 是本设计新增的路由，同样默认回环。
  因此**路径 2 的跨机器 join 需要显式开启绑定地址**（新增一个 config 项，默认关闭、需要用户确认），
  开启后仍由一次性 grant、session token 与 `NODE_METHODS` 白名单保护。今天它实际只覆盖「同机或共享回环」，
  跨设备是 B2 之后的一次显式动作 —— 不要把「能 join」误读成「默认对外可达」。
- **grant 一次性、短 TTL**（A 已实现 `ENROLL_TTL_SECONDS = 300`），session token 只能调 `NODE_METHODS` 白名单里的方法。
- **control 不信任 node 报的路径**（I8）：`archive` 是不透明句柄，`lastArtifacts` 只用于告诉用户还剩什么。
- **用户设备的信任来自「用户自己装了它」**：grant 在 control 上生成、由用户带过去，不需要 control 有该设备的 SSH 凭据。

### 12.5 什么时候需要第四种路径

当用户希望 control 直接管一台只提供 inbound 的设备（例如内网服务器）时，再加一个 **SSH 供给适配器**；
它只是「怎么供给一台 node」的另一种实现，不改变本文的任何数据结构。

