# 2026-11-04 — 常量仓库：共享 Harness 源码镜像

> 状态：**设计**（尚未实现）。本文是后续 `feat/harness-mirror` 分支的施工依据。
> 相关：`docs/design/2026-11-04-node-artifact.md`（stage B 产物）、`docs/specs/prepared-template-runtime.md`（prepared base 规格）。

## 要解决的问题

拉一个 Harness 版本要走一次网络全量克隆。用户装 10 个版本，就是 10 次全量传输、
10 次网络失败的机会。这和 WSL2 那条教训是同一件事：**整体搬一次，走快的那条路，
不要每个版本都重拉一遍文件树。**

### 现状核实（2026-11-05，读代码得出）

`shallow_clone_with_cancel`（`box-runtime/src/lib.rs:361`）有 **4 个生产调用点**，
**其中 2 条是 Harness**：

| 调用点 | 克隆对象 | 频率 |
|---|---|---|
| `dshboxd/src/versions.rs:239` | Harness prepared base（`<runtime>/prepared-bases/`） | 每个版本一次 |
| `box-dsh-versions/src/lib.rs:669` | Harness 版本目录（`<root>/runtimes/<version>/source`），`dshbox template install` 走这条 | 每个版本一次 |
| `dshboxd/src/data.rs:356` | 任意仓库（ADD data 的 GitHub 源） | 按需 |
| `dshboxd/src/extensions.rs:888`、`bundles.rs:735` | 任意仓库（容器内导入插件、bundle 回源） | 按需 |

只改第一条，`dshbox template install` 的收益是零 —— 所以镜像必须**同时服务这两条
Harness 路径**。任意仓库那三条不在范围内：克隆的是用户自己的仓库，一仓一镜像
没有复用价值。

### 一个必须先说清楚的事实

生产路径上**没有本地目录 ref**：`parse_template_ref`
（`box-dsh-versions/src/lib.rs:192`）硬性要求 `github.com|gitlab.com|bitbucket.org`
三段路径，产出恒为 `https://{head}`。`Path::new(url).is_dir()` 分支只服务
`box-runtime` 的单元测试（`box-runtime/src/lib.rs:524`）。因此镜像**总是**在联网
路径上，必须自己处理离线。

## 设计

一个持久 bare 镜像，落在运行目录下：

```
<runtime>/harness-mirror.git/                 # bare repo，一台机器一份
<runtime>/harness-mirror.git/MIRROR_SOURCE_URL # 纯文本一行：镜像的 remote URL
```

### 1. 确保镜像

镜像不存在时从 remote 全量克隆一次 —— 这是**唯一**一次全量传输。

`MIRROR_SOURCE_URL` 记录镜像的 remote 来源。实际 remote 是
`mirror_url(&parsed.url, config.github_mirror)`（`dshboxd/src/versions.rs:237` →
`box-foundation/src/lib.rs:646`），也就是说**用户改 `dshbox config set mirror` 会让
remote URL 漂移**。规则：

- 文件不存在 → 全量克隆，写入该文件；
- 文件存在且与当前 remote 一致 → 直接用；
- 文件存在但不一致 → **不删除镜像**，用 `git remote set-url` 改指，再按需增量 fetch。
  换镜像源不等于换内容，多半是同一份仓库换个入口；真要重建由用户显式删除镜像目录。

### 2. 增量 fetch

用 libgit2 打开 bare 镜像，fetch 目标 ref。对**已有镜像**来说，一次新 tag 的 fetch
只传新增对象 —— 具体字节数取决于该 tag 引入的变更量，**目前没有实测数据**
（实现后补测量数字，这里不猜）。

### 3. 本地 checkout

从**本地镜像**克隆到 staging（libgit2 本地 clone，不联网），再解析 commit。

取消检查点是 libgit2 的 `transfer_progress` 回调（`box-runtime/src/lib.rs:304-310`）——
传输途中轮询取消标志；clone 之后的 `task.check_cancelled()`（`versions.rs:252`）
是第二道闸。

### 4. 离线行为（关键）

**离线时不得删除镜像。** 镜像损坏和镜像不可达是两回事：

| 情况 | 判定 | 动作 |
|---|---|---|
| 镜像损坏 | libgit2 报对象/pack 级别错误（缺对象、pack 损坏） | 删除镜像，重建一次 |
| 网络不可达 | fetch 连不上（传输层错误） | **保留镜像**；目标 tag 已在镜像里就直接用，不在就明确报错 |

区分方式：fetch 的错误属于**传输层**（连不上、超时）还是**对象层**（缺对象、
pack 损坏）—— 前者保留镜像，后者重建。

离线解析 tag 时注意：`list_remote_tags`（`box-runtime/src/lib.rs:408`）对
`https://` URL 一律走 `remote_anonymous` + `connect_auth`（:458-471），**必须联网**；
只有传本地文件系统路径才走离线的读盘分支（:411-426）。所以离线时必须把镜像的
**路径**传给它，而不是 remote URL。

## 并发：锁的真实语义

`box-scheduler` 的 `resource_keys` 是**整任务粒度**：`try_start` 时整体占用
（`box-scheduler/src/lib.rs:756-766`），finish 时整体释放（:927-937）。它**不支持**
任务内分段持锁。

所以要拿到 `harness-mirror` 这个 key 的任务，**整个任务期间都占着它**，包括
checkout 这段本不需要排他的时间。两个选择：

1. **一个任务做完 fetch + checkout**（最简单，串行化整个 pull）；
2. **拆成两个 scheduler 任务**：`harness-mirror-fetch`（持 key）与
   `harness-mirror-checkout`（不持 key）。checkout 读 bare 仓库是安全的，多读并发没问题。

倾向 2，但**这会多一次任务切换**，而当前 pull 是一条任务链。先用 1 落地，
把并发收益记为已知限制。

### 与 `repository:dsh-versions` 的关系

`enqueue_pull_template` 已经声明 `vec![runtime:<version>, "repository:dsh-versions"]`
（`dispatch.rs:777-780`），而 `dsh-catalog-refresh` 也持 `repository:dsh-versions`
（`dispatch.rs:756`）—— 但 catalog 刷新只做一次 `ls-remote`（`versions.rs:176`），
是**秒级**操作；被一个几分钟的 pull 卡住是既有行为。

镜像用**独立 key** `harness-mirror`，不与 `repository:dsh-versions` 合并：两者粒度
不同（catalog 是秒级 ls-remote，镜像重建可能是分钟级），合在一起会让一次镜像重建
把 catalog 刷新也堵上。同一个 pull 最多持 3 个 key，**不要再加第 4 个**。

## 磁盘成本

镜像保存 Harness 仓库的完整对象集。**体积没有实测**（早期版本写的"约 1.3 GB"是
WSL2 tar 镜像的数字，与 git 裸库无关，不能挪用）。

已知约束（同批设计文档的结论）：D: 盘只剩约 8.8 GB 空闲，运行盘上已经有每个
prepared base 的完整 DSH 树（含 node_modules 与构建产物）、sealed template 副本、
容器副本。一份只用于下载源码的裸库要放进这个对比里算。

对策：
- 落盘前预检空间，不足给明确错误，不半途写坏（沿用 `node-artifact.md:111` 的做法）；
- 镜像只留**对象**，不做 checkout —— 不产生工作树；
- `git gc` / 限制历史深度是候选，但**需要实测取舍**：浅历史能省空间，会失去
  "以后能取到任意旧 tag"的能力；
- 提供"删除镜像"的入口（它是缓存，不是用户数据）。

## 不在范围

- 任意仓库的 clone（`data.rs` / `extensions.rs` / `bundles.rs` 那三条）。
- prepared base 的索引、去重、`validate_prepared_harness` 逻辑 —— 一个 DSH 版本
  一个 base 的颗粒度规则见 `docs/specs/prepared-template-runtime.md`；镜像是**源码的
  来源**，不是另一层 base 缓存。
- `client_artifacts_present` 的平台规则。

## 文件清单（预期）

```
src-tauri/crates/box-runtime/src/lib.rs      +ensure_bare_mirror / +fetch_ref_to_mirror
src-tauri/crates/box-runtime/src/lib.rs      +local_clone_from_mirror（离线 tag 解析）
src-tauri/crates/dshboxd/src/versions.rs     pull_template_with_cancel 改走镜像
src-tauri/crates/box-dsh-versions/src/lib.rs pull_template 改走镜像（第二条 Harness 路径）
src-tauri/crates/dshboxd/src/dispatch.rs     +harness-mirror 资源 key
docs/design/2026-11-04-shared-harness-mirror.md   本文件
```

## 验收

1. 第二个版本不再全量传输：镜像里已有 tag 时，pull 不产生全量 clone（有单测 +
   实测两次 pull 的传输量对比）。
2. 两条 Harness 路径都走镜像：`pull_template` 与 `dshbox template install` 都在
   镜像上完成（有测试）。
3. 离线可用：断网后 pull 一个镜像里已有的 tag 成功；失败时**镜像未被删除**（有单测）。
4. 镜像损坏会重建：人为损坏镜像后 pull 自动重建一次，而不是永久失败（有单测）。
5. 取消仍然有效：clone 途中取消，任务进入 cancelled 且不留下半个 staging（有单测）。
6. 磁盘不足时明确报错，不半途写坏（有单测）。
7. `cargo test --workspace` + `pnpm build` 全绿。

## 风险

| 风险 | 对策 |
|---|---|
| 裸库体积挤占运行盘（D: 约 8.8 GB 空闲，已有 prepared base / sealed template / 容器副本） | 落盘前预检空间，不足给明确错误；只留对象不留工作树；提供删除入口；浅历史方案待实测 |
| 镜像损坏 | 对象层错误 → 删除重建一次；传输层错误 → 保留镜像并明确报错（不删用户唯一副本） |
| 离线时误删镜像 | 自愈区分错误层次，离线不触发重建 |
| `config set mirror` 让 remote URL 漂移 | `MIRROR_SOURCE_URL` 比对 + `set-url`，不因换源重建 |
| 锁粒度限制：整任务持锁，checkout 也被串行化 | 先用单任务实现；并发收益记为已知限制，或后续拆两个任务 |
| 与 `repository:dsh-versions` 重复串行化 | 镜像用独立 key；一个 pull 最多 3 个 key |
| 两条 Harness 路径各建一份镜像 | 两条路径共用同一个 `<runtime>/harness-mirror.git` |
| 镜像缓存被误当用户数据（导出/拷贝/清理） | 镜像明确是缓存：不进导出、不进拷贝、不计入用户数据统计 |
