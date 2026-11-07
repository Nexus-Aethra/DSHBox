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
| node 被移除 | 若仍有容器引用 → 拒绝；强制移除需先 `container copy` 到别处 | 不静默改绑到 host |
| node `failed` | 同 offline，且拒绝新选址 | 不自动重装 node |

不自动迁移的理由：迁移会改变网络边界与文件系统语义（WSL2 netns 共享、drvfs 慢），
用户以为自己在一块盘上、其实换了一块，是最难查的一类 bug。迁移必须是显式动作。

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
| daemon 重启 | `sessionEpoch++`；所有 node 降为 `registering/offline`（A 已实现）；已认领任务置 `failed`；产物行保留 |
| node 断线重连 | 用 enroll 重新注册（新 epoch），`claimedBy` 旧值失配 → 该任务不再属于它 |
| 产物平台不符 | 视为陈旧，重派构建；不复用、不删除旧行 |
| 归档磁盘不足 | helper 侧导出前预检（D: 仅 ~8.8 GB），不足则不写 `.part` 并回报明确错误 |
| 认领后 helper 崩溃 | 任务停 `running`；epoch 未变时不重排，等 `node_claim_task` 的超时回收（见 §9 Q3） |

## 8. 明确不存

- DSH 源码树、插件、profile（这些是 node 本地的，产物只按 base 缓存）
- node 本地绝对路径的语义（`archive` 是不透明句柄）
- DSH 宿主的端口号（每次启动随机，只存「意向」，真实端口由 node 侧在启动后回报）
- 宿主的 `build_platform()` 结果（这是 B 里最容易写错的一行）

## 9. 未决问题（需要裁决）

**Q1 容器能否在两个 node 上并存副本？** 我倾向「不能」，`container copy` 到另一 node 是新建一个
容器、旧的不动；两个同名副本会让 `container rm` 语义变模糊。

**Q2 产物是否随 node 删除级联清理？** 我倾向：删 node 时删索引行，**不删** node 磁盘上的归档
（控制层本来就碰不到），并在返回里说明「归档留在原机，可手工清理」。

**Q3 认领后超时**：`running` 且 heartbeat 已断的 node 上的任务，多久回收？我倾向
「不按时间自动失败」，而是由 node 重新注册时显式上报 `abandonedTaskIds`，避免时钟/网络抖动误杀。

**Q4 `node_build_artifact` 是否等待完成？** 我倾向：RPC 立即返回 `taskId`，调用方（CLI/UI）自行订阅；
容器创建路径顺带派活时也不等待。

## 10. 阶段落点

| 阶段 | 用它做什么 |
|---|---|
| B1 | 建 `node-artifacts` scope、`node_build_artifact/list/report`、`node_claim_task`、`tasks.nodeId/claimedBy/claimEpoch` |
| B2 | helper 认领并执行，写 `nodes.mirror` / `nodes.toolchain` |
| C | `container.json` 增 `nodeId/placement`，选址与启动下沉 node |
| P | `nodes.relay` 填值，页面通过隧道回传 |
