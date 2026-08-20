# 数据调度器（Data Scheduler）Spec

状态：设计阶段

**目标：统一资源映射管理，通过软删 + 后台异步硬删解决删除冲突、引用计数分散、半删残留等问题。**

---

## 1. 核心原则

1. **单真相源**：所有受管资源（插件、模板、容器）的存在与状态以
   `state/resource-map.json` 为唯一依据；不再分散维护
   `template-index.json`、`repository/index.json`、`references.json`。
2. **软删优先**：`remove` 操作只从 map 中移除 active 记录并加入
   删除队列，不立即碰磁盘；磁盘文件由后台线程异步硬删。
3. **最终一致性**：硬删失败不阻塞流程，进入慢队列重试；重试耗尽后
   标记永久失败并写诊断日志，map 保留软删状态供审计。
4. **路径即身份**：受管资源存储路径中嵌入 `fnv1a` hash，同身份的资源
   （同名同版本插件 / 同 ref 模板）天然落到同一目录，去重变成结构性
   问题而非运行时扫描。
5. **引用图统一管理**：跨资源引用（容器引用模板、容器引用插件、bundle
   引用插件）通过 map 内 `refs` 字段维护，删除保护直接查 refs。

---

## 2. 资源分类表

| 资源 | 是否纳入 | 路径哈希方式 | 现有管理方式 | 迁移收益 |
|------|----------|-------------|-------------|---------|
| **插件**（repository extensions） | **核心** | `fnv1a(name + ":" + version)` | `repository/index.json` + `references.json` | 去重结构化为路径；删除后重装路径不变；引用计数统一 |
| **模板**（templates） | **核心** | `fnv1a(harness_ref)` | `template-index.json` | 消除"删除后重装冲突"和"半删残留"两个已知 bug |
| **容器**（containers） | **核心** | 使用现有 timestamp id | `container-store.json` | 引用枢纽：`refs` 字段连接模板和插件 |
| **Bundle**（extension bundles） | 引用聚合 | 无独立路径（元数据） | `repository/bundles.json` | `entries` 从 `repository_id` 改为 `resource_id` |
| DSH 版本（Harness） | **不纳入** | — | 已派生自模板索引 | 无额外收益 |
| Workspace 扩展扫描 | **不纳入** | — | 是插件 import 入口 | 结果落到插件 |
| DSH runtime 数据 | **不纳入** | — | `runtime-lock.json` 独立管理 | 不参与生命周期 |
| 构建产物（.dsh tar） | **不纳入** | — | 短期物，materialize 消费 | 复杂度不划算 |

---

## 3. 存储布局

```
<runtime-root>/
  state/
    resource-map.json          # 唯一真相源
    deletion-queue.json        # 快队列 + 慢队列 + 永久失败
  repository/
    plugins/
      <fnv1a64>/               # 插件源目录（按 hash 命名）
        source/
    bundles.json               # bundle 元数据（引用 resource_id）
  runtimes/
    <fnv1a64>/                 # 模板 runtime 目录（按 hash 命名）
      source/
      .dshbox-runtime.json
  containers/
    <timestamp>/               # 容器目录（保留现有命名）
```

资源 map 与删除队列各占一个 JSON 文件，与 `box-foundation` 既有
`read_json / write_json` 工具对接。

---

## 4. 数据结构

### ResourceEntry

```
struct ResourceEntry {
    id:        String,      // "<type>:<hash>"
    path:      String,      // 相对于 runtime-root 的路径
    status:    ResourceStatus,
    refs:      Vec<String>, // 引用此资源的 resource_id 列表
    meta:      BTreeMap<String, String>, // 类型相关元数据
    createdAt: u64,         // unix 秒
}

enum ResourceStatus {
    Active,        // 正常在线
    Deleted,       // 已软删（map 移除，磁盘未清理或清理中）
}
```

### DeletionQueue

```
struct DeletionQueue {
    fast:              Vec<DeletionQueueEntry>,
    slow:              Vec<DeletionQueueEntry>,
    permanentFailures: Vec<PermanentFailure>,
    lastProcessedAt:   u64,  // unix 秒
}

struct DeletionQueueEntry {
    id:          String,
    path:        String,
    enqueuedAt:  u64,
    retryCount:  u32,
}

struct PermanentFailure {
    id:        String,
    path:      String,
    lastError: String,
    failedAt:  u64,
}
```

### 资源类型前缀

| 类型 | 前缀 | 示例 id |
|------|------|--------|
| 插件 | `plugin:` | `plugin:a3f2b1c9d4e5` |
| 模板 | `template:` | `template:010035d14ddc6780` |
| 容器 | `container:` | `container:1787115066` |

`ResourceEntry.id` 同时充当 map 的 key（`<type>:<name>`）的 value，
map key 用可读名称（如 `github.com/deepseek-ai/deepseek-harness:dsh-v0.1.0-rc.7`），
id 才是稳定跨表引用用的标识符。

---

## 5. 调度流程

### 软删 → 快队列

```
resource.remove(name)
  → map: remove active entry (keep "deleted" marker in a separate tombstone map or in deletion-queue metadata)
  → 实际行为: map 中删除该 entry，同时向 fast queue 追加 {id, path}
  → 如果 refs 非空: 报错拒绝删除（与现有引用保护一致）
```

### 快队列处理

- **触发时机**：每次 map 写操作后（add / remove / refs 变更）
- **处理逻辑**：
  ```
  while fast queue not empty:
      entry = fast.dequeue()
      if map 中该 entry 仍存在 (可能刚被恢复):
          fast 跳过，继续下一个
      if path 不存在 (已被外部清理):
          标记清理完成
      else:
          try remove_dir_all(path):
              success → 清理完成
              fail  → entry.retryCount += 1, enqueue to slow queue
  ```

### 慢队列处理

- **触发时机**：后台 worker 定时器（默认 60 秒周期）
- **处理逻辑**：
  ```
  while slow queue not empty:
      entry = slow.dequeue()
      if entry.retryCount >= MAX_RETRIES (默认 5):
          permanentFailures.append(entry.id, entry.lastError)
          write diagnostic log
          continue
      try remove_dir_all(path):
          success → 清理完成
          fail  → entry.retryCount += 1, re-enqueue to slow queue
  ```

- **慢队列取出一条处理一条，不批量**：避免一个卡住的目录阻塞后续条目。

### 永久失败

- 写入 `permanentFailures` 数组 + 诊断日志（`state/diag/`）
- 不再重试
- map 中该资源状态为 `deleted`（tombstone），UI 不展示
- 下次重启 daemon 时仍检查永久失败列表，但不自动重试（用户手动干预）

---

## 6. 后台 Worker 行为

`dshboxd` 启动时启动一个后台线程：

```
loop:
    if fast queue non-empty:
        drain fast queue
    if now - lastSlowPollAt > SLOW_INTERVAL:
        process slow queue
        lastSlowPollAt = now
    sleep(100ms)   // 不阻塞主事件循环
```

- 线程生命周期 = daemon 进程生命周期
- daemon 退出时不做最终 flush（软删条目在下次启动的队列里仍在）
- 队列文件每次操作后原子写回

---

## 7. 资源映射表变更操作

| 操作 | map 变更 | 队列影响 | refs 影响 |
|------|---------|---------|----------|
| `add(resource)` | 插入新 entry | 无 | 无 |
| `remove(name)` | 删除 entry + 加入 tombstone | fast enqueue | 无 |
| `addRef(resource_id, ref_id)` | entry.refs 追加 | 无 | 更新双向 |
| `removeRef(resource_id, ref_id)` | entry.refs 移除 | 无 | 更新双向 |
| `recover(name)` | tombstone → active | 从队列移除 | 保留 |

---

## 8. 插件路径迁移

### 当前（迁移前）

```
repository/plugins/img-<uuid>/source/
```

`img-` 前缀来自 import task id，随机生成。同名同版本插件多次 import 会产生
多个目录，去重靠 `reconcile_owner_index` 扫描。

### 迁移后

```
repository/plugins/<fnv1a64>/source/
```

`fnv1a(name + ":" + version)` 确定性 hash。同插件永远落到同一路径，
去重变成天然属性。

### 迁移策略

- **新建插件**：直接走 hash 路径
- **已有插件**：**不迁移**（数据迁移工作量超出范围，用户确认暂缓）
- **共存期**：hash 路径和旧 `img-<uuid>` 路径并存，通过 `meta.legacy_path`
  记录旧路径以便清理

---

## 9. 与现有系统交互

### 与 DSH 运行时的交互

`dsh plugin add <path>` 由 DSH 运行时在 Node 环境里执行。数据调度器传给
DSH 的路径从 `.../img-<uuid>/source/` 改为 `.../<fnv1a64>/source/`，是纯
路径字符串替换，不涉及 DSH 运行时代码修改（前提：DSH 运行时无路径前缀
假设）。

### 与 `box-api` wire 类型的交互

现有 `RepositoryExtension`、`TemplateInfo` 等 wire 类型保持不变，
`resource-map` 是 daemon 内部的存储层抽象，不改变 IPC 协议。

### 与 `box-scheduler` 的关系

数据调度器的删除 worker 是 **独立的后台线程**，不通过 `box-scheduler` 的
任务队列（`box-scheduler` 处理的是用户提交的任务，如 build / pull / clone）。
删除队列由 daemon 进程在后台循环消费，无需用户可见任务。

---

## 10. 实施路线

| 阶段 | 范围 | 预估文件数 |
|------|------|-----------|
| **1. 数据调度器核心** | `box-data-scheduler` 新 crate：ResourceMap + DeletionQueue + Worker | ~5 |
| **2. 插件路径 hash 化** | `box-extensions` import 走 hash 路径，remove 走软删 | ~3 |
| **3. 模板迁移到 map** | `box-dsh-versions` + `dshboxd/versions.rs` 走 map | ~4 |
| **4. 容器引用枢纽** | `dshboxd/containers.rs` 创建/删除走 map refs | ~2 |
| **5. Bundle 引用改写** | `box-extensions` bundle entries → resource_id | ~1 |
| **6. 前端刷新** | 资源列表从 map 派生（非必需，IPC 协议不变） | ~1 |

---

## 11. 已知约束

- 现有数据不做自动迁移，新资源走新路径，旧资源保持原状直到自然淘汰
- UI 不提供删除恢复入口（后续可加 `permanentFailures` 查询接口）
- `MAX_RETRIES = 5`，`SLOW_INTERVAL = 60s` 为初始值，可通过配置调整
- hash 算法固定为 FNV-1a 64-bit（与 data store 的 `fnv1a64` 一致）
- 删除队列文件每次写回后 atomically replace，防止崩溃时半写
