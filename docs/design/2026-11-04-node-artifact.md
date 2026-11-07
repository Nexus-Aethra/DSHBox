# 2026-11-04 — Node 产物（stage B）：异步「构筑虚拟机版」

> 状态：**设计（B1-b1 已落地一部分）**。前一支 `feat/node-core`（A）已合并，本文件是 B 的施工依据。
> 父文档：`docs/design/2026-11-04-node-architecture-roadmap.md`。
> **权威设计以 [`2026-11-07-node-persistence-and-placement.md`](2026-11-07-node-persistence-and-placement.md) 为准**：
> 本文的两处旧结论已被推翻，见文末「与 v3 设计的差异」。B1-b1 已实现产物索引、
> `node_artifact_list` / `node_artifact_report` 两个 RPC 与平台校验；其余（任务派发、认领、清理）仍在 B1-b2 / B2。

## 要解决的问题

A 让节点能注册。B 让节点**干活并交出产物**：模板里「构筑虚拟机版」这件事，
从同步阻塞控制层，变成「往某个 node 派一个任务，helper 在 guest 内做完，回报 digest」。

之所以非做不可，实测数字（见路线图）：

- `pnpm run build` 433–471 s，一次 base 准备总共 9–12.5 min；
- 若挂在初次导入的同步路径上，每次导入多等 10–15 min；
- `client_artifacts_present` 比对的是**宿主**的 `build_platform()`，
  所以 Windows 准备的 base 永远满足不了 Linux guest —— **Linux 产物只能在 Linux 内构建**。

第三条是硬约束，也是 B 存在的真正理由：不是「为了快」，是「在宿主上根本做不出来」。

## 数据模型增量

沿用路线图的 `NodeArtifact`，本轮确定字段：

```rust
pub struct NodeArtifact {
    pub id: String,             // "<nodeId>|<baseId>"，documents 单列主键
    pub node_id: String,        // 哪个 node 编出来的
    pub base_id: String,        // base-<16 位小写 hex>，是 prepared base 不是 sealed 模板
    pub source_commit: String,  // 冗余存，便于 index 自洽检查
    pub platform: String,       // 必须等于 node.platform，{OS}-{ARCH} 拼法
    pub archive: String,        // 不透明句柄；控制层不 stat、不拼路径（I8）
    pub sha256: String,         // 64 位小写 hex；node 构建时算，C 阶段解包前复算
    pub size_bytes: u64,
    pub built_at: u64,          // 控制层收到 report 的时刻，不是 node 构建完成时刻
}
```

键：`(node_id, base_id)`，编码为 `<nodeId>|<baseId>`。落在现有 `DocumentStore` 的
`node-artifacts` scope，不引入 `PRAGMA user_version` 迁移（与 A 的 `NodeRecord` 同一策略）。
**不存 `verifiedAt`**：控制层到不了 node 的路径，无从「核对磁盘上的实际摘要」。

**不存什么**：不存 DSH 源码树、不存插件、不存 profile。一个 DSH 版本一个 base；
一份 (node, base) 一份镜像；N 个 sealed 模板共享该 base。

## RPC 增量

| method | 凭证 | 参数 | 返回 |
|---|---|---|---|
| `node_artifact_report` | session | `{ baseId, sourceCommit, platform, sha256, sizeBytes, archive }` | `NodeArtifact` 行 |
| `node_artifact_list` | 主 token | `{ nodeId? }` | `NodeArtifact[]` |
| `node_build_artifact` | 主 token | `{ nodeId, baseId }` | `{ taskId }` |

`node_artifact_report` 走 A 已建立的白名单机制：**加进 `NODE_METHODS`**（漏了就是 helper
第一次调用 403），并且 `__nodeId` 是**服务端保留字段**：网关先无条件剥掉请求体里带来的值，
再用已验证 session 的节点覆盖它。主 token 没有 session，因此根本不能替任何 node 报产物。
网关层有真实 loopback 回归测试（删掉剥离那行，测试立刻红）——只在 handler 里读这个字段
是不够的，调用方可以直接伪造请求体。

`node_build_artifact` 是**控制层派活**：它进 `box-scheduler` 队列，
resource key 加 `node:<id>`，再由 SSE 把任务推给该 node 的 helper（A 的流已经能把事件送到位）。

## 关键设计点

### 1. 产物归属用 digest 而不是 trust

helper 报上来的是 `sha256`。控制层**只校验形状后入库**（长度/字符集/大小/平台/节点归属）；
真正解包前的摘要复算发生在 node 侧（C 阶段），因为控制层到不了 node 的路径（I8）。
理由与 `latest.json` 那条规则同源：一个 tag 不说明字节，同理「helper 说编好了」
也不说明那些字节就是我们想要的构建 —— 只是核对者从控制层换成了 node 自己。
见 v3 设计 §11。

### 2. `platform` 与宿主解耦

A 阶段 `NodeRecord.platform` 已经是 `{OS}-{ARCH}` 形式；产物的 `platform`
**取 node 的 platform，不取宿主的 `build_platform()`**。
这是 B 里最容易写错的一行 —— 复制现有的 `client_artifacts_present` 逻辑
就会把宿主平台带进去，而那正是要绕开的 bug。

### 3. 失败与取消不留脏记录

任务是 `box-scheduler` 的常规任务，所以失败/取消走既有路径。
约束：**只有 `node_artifact_report` 成功入库才算产物存在**；
任务失败只会留下任务记录，不会留下半份 `NodeArtifact`。
partial 归档在 helper 侧写 `.part` 再 rename，与 A 修好的 libkrun 缓存同一手法。

### 4. helper 侧不做推理

helper 收到任务就按固定次序执行：clone → install → build → tar → sha256 → report。
它**不决定**要不要编、编哪个版本 —— 那是控制层的事。
这条界线是「控制层只派意图」在代码里的落点。

## 文件清单（预期）

```
src-tauri/crates/dshboxd/src/node_artifacts.rs   新增：记录 + collection + 校验
src-tauri/crates/dshboxd/src/nodes_rpc.rs        +3 handler
src-tauri/crates/dshboxd/src/nodes.rs            NODE_METHODS 加 node_artifact_report
src-tauri/crates/dshboxd/src/dispatch.rs         +3 分支
src-tauri/crates/box-helper/src/main.rs          build 任务执行 + report
docs/design/2026-11-04-node-artifact.md          本文件
```

## 验收

1. 一个 `(node, base)` 只编一次：第二次请求命中已有记录，不再派任务（有单测）。
2. 初次导入不增加等待：构筑是异步任务，导入路径的耗时数字不变（实测对比）。
3. 失败/取消不写脏记录：任务失败后 `node_artifact_list` 仍为空（有单测）。
4. 产物 `platform` 等于 node 的 platform，**不等于**宿主 `build_platform()`（有单测，这是回归防线）。
5. 一个 node 不能为别的 node 报产物（`__nodeId` 强制，实机探针）。
6. `cargo test --workspace` + `pnpm build` + Linux `cargo check` 全绿。

## 风险

| 风险 | 对策 |
|---|---|
| 镜像 tar 撑爆运行盘（D: 仅约 8.8 GB） | 导出前预检空间，不足给明确错误，不半途写坏 |
| helper 与 daemon 版本漂移 | 复用 A 的 `helper_version`，不兼容拒绝派任务 |
| 控制层去读一个只有 node 能访问的路径 | 产物路径**只给 node 用**，控制层不 stat、不读 |
| daemon 重启后任务丢失 | 任务在 scheduler 持久化；helper 重连后重新 claim |

## 不在 B 范围

- 不改 host 容器的启动路径（C 才动）。
- 不做 docker 供给适配器（C 之后）。
- 不做面板（D）。
- 不改 `container.json` 的 `runtime` 字段（C）。

## 与 v3 设计的差异（v3 为准）

[`2026-11-07-node-persistence-and-placement.md`](2026-11-07-node-persistence-and-placement.md)
§11 逐条修订了本文的两处旧结论，B1-b1 按 v3 实现：

1. **信任模型**：本文原写「控制层核对磁盘上的实际摘要」。控制层到不了 node 的路径（I8），
   改为 node 算摘要并上报、控制层校验形状；解包前复算在 node 侧。
2. **`baseId` 拼法**：本文示例写作 `sealed-…`，实际 prepared base id 是 `base-<16 位小写 hex>`
   （`versions.rs:353`），`sealed-*` 是模板。按旧示例实现会把产物行数放大 N 倍。

另外两点在 v3 里收紧，本文当时未覆盖：

3. **平台拼法**：`node.platform` 与 `artifact.platform` 一律 `{OS}-{ARCH}`，enroll 时白名单校验，
   `node_register` 用 helper 自报的值覆盖 grant 里的值；否则「产物平台 == 节点平台」在空串或
   自由文本上恒真，I1 形同虚设。
4. **任务状态**：失联是 `miss`（终态），不伪装成 `failed`；终态任务不受迟到回报影响。
   回滚到旧 daemon 时 `miss` 会被读成 `failed`（v3 §5），所以 daemon 与桌面必须同批重建，
   不与旧版本混跑。