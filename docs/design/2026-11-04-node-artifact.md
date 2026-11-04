# 2026-11-04 — Node 产物（stage B）：异步「构筑虚拟机版」

> 状态：**设计**。前一支 `feat/node-core`（A）已合并，本文件是 B 的施工依据。
> 父文档：`docs/design/2026-11-04-node-architecture-roadmap.md`。

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
    pub node_id: String,        // 哪个 node 编出来的
    pub base_id: String,        // 哪个 prepared base（版本级）
    pub source_commit: String,  // 冗余存，便于 index 自洽检查
    pub platform: String,       // 产物自身平台，如 linux-x86_64
    pub archive: PathBuf,       // 该 node 可访问的路径
    pub sha256: String,         // 产物归档摘要
    pub size_bytes: u64,
    pub built_at: u64,
}
```

键：`(node_id, base_id)`。落在现有 `DocumentStore` 的 `node-artifacts` collection，
不引入 `PRAGMA user_version` 迁移（与 A 的 `NodeRecord` 同一策略）。

**不存什么**：不存 DSH 源码树、不存插件、不存 profile。一个 DSH 版本一个 base；
一份 (node, base) 一份镜像；N 个 sealed 模板共享该 base。

## RPC 增量

| method | 凭证 | 参数 | 返回 |
|---|---|---|---|
| `node_artifact_report` | session | `{ baseId, platform, sha256, sizeBytes, archive }` | `{ ok, artifactId }` |
| `node_artifact_list` | 主 token | `{ nodeId? }` | `NodeArtifact[]` |
| `node_build_artifact` | 主 token | `{ nodeId, baseId }` | `{ taskId }` |

`node_artifact_report` 走 A 已建立的白名单机制：**加进 `NODE_METHODS`**，
并且 `__nodeId` 由服务端注入 —— 一个 node 只能为自己的 id 报产物，
不能替别的 node 声明「我编好了」。这一点 A 的测试已经钉住机制，B 只需复用。

`node_build_artifact` 是**控制层派活**：它进 `box-scheduler` 队列，
resource key 加 `node:<id>`，再由 SSE 把任务推给该 node 的 helper（A 的流已经能把事件送到位）。

## 关键设计点

### 1. 产物归属用 digest 而不是 trust

helper 报上来的是 `sha256`。控制层**校验后再入库**：
长度/字符集先查，然后按需（首次引用时）核对归档在磁盘上的实际摘要。
理由与 `latest.json` 那条规则同源：一个 tag 不说明字节，同理「helper 说编好了」
也不说明那些字节就是我们想要的构建。

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