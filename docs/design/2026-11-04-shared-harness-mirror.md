# 2026-11-04 — 常量仓库：共享 Harness 源码镜像

> 状态：**已实现**（`feat/harness-mirror`）。实现与本文一致的部分按代码为准，
> 与初稿不一致的地方都在下文标了「实测修正」并给出数字。
> 相关：`docs/design/2026-11-04-node-artifact.md`（stage B 产物）、`docs/specs/prepared-template-runtime.md`（prepared base 规格）。

## 要解决的问题

拉一个 Harness 版本要走一次网络全量克隆。用户装 10 个版本，就是 10 次全量传输、
10 次网络失败的机会，而且失败一次就把已经下载的字节全丢掉。这和 WSL2 那条教训是
同一件事：**整体搬一次，走快的那条路，不要每个版本都重拉一遍文件树。**

### 现状核实（2026-11-05，读代码得出）

`shallow_clone_with_cancel`（`box-runtime/src/lib.rs:361`）的生产调用点：

| 调用点 | 克隆对象 | 状态 |
|---|---|---|
| `dshboxd/src/versions.rs:239` | Harness prepared base（`<runtime>/templates/base-<digest>/`） | **生产路径**，每个版本一次 |
| `dshboxd/src/data.rs:356` | 任意仓库（ADD data 的 GitHub 源） | 生产路径，按需 |
| `dshboxd/src/extensions.rs:888`、`bundles.rs:735` | 任意仓库 | 生产路径，按需 |
| `box-dsh-versions/src/lib.rs:669` | 遗留 `runtimes/<version>/source` | **遗留不可达**：无生产调用者，且被 schema-10 的 `initialize_schema_10` 拒绝 |

**生产上只有一条 Harness clone 路径。** `dshbox template install` → RPC `pull_template`
→ `dispatch.rs:220` → `versions.rs:186` → 同一条，与 UI 是同一条，所以镜像只接一处。
任意仓库那三条不在范围（一仓一镜像没有复用价值）；遗留那条也不在（当前跑不通）。

### 一个必须先说清楚的事实

生产路径上**没有本地目录 ref**：`parse_template_ref` 硬性要求
`github.com|gitlab.com|bitbucket.org` 三段路径，产出恒为 `https://{head}`。
所以镜像**总是**在联网路径上，必须自己处理离线。

## 设计

一个持久 bare 镜像，落在运行目录下：

```
<runtime>/harness-mirror.git/                  # bare repo，一台机器一份
<runtime>/harness-mirror.git/MIRROR_SOURCE_URL # 纯文本一行：镜像的 remote URL
```

### 1. 确保镜像（`box-runtime/src/mirror.rs::ensure_bare_mirror`）

镜像不存在时**只建一个空 bare 仓库**并加 remote —— 不克隆。对象由第 2 步的
fetch 下载，因为那一步才知道用户到底要哪个版本。

> **实测修正（初稿错了）**：初稿写的是「不存在时从 remote 全量克隆一次，这是唯一
> 一次全量传输」。实测不成立：用系统 git 全量裸克隆是 69.9 s / 372.4 MB，但**同样
> 的传输用 libgit2 跑 15 分钟没结束**（服务端要协商整个历史）。让每一次首拉都付这个
> 代价，比它要取代的那次克隆更贵。所以镜像是**浅的**：每个版本 depth 1，第一步只
> 建空库，体积与今天的一次克隆同量级。

`MIRROR_SOURCE_URL` 记录镜像的 remote 来源。实际 remote 是
`mirror_url(&parsed.url, config.github_mirror)`，也就是说**用户改
`dshbox config set mirror` 会让 remote URL 漂移**。规则：

- 文件不存在 → 建库 + 记录；
- 文件存在且与当前 remote 一致 → 直接用；
- 文件存在但不一致 → **不删除镜像**，用 `Repository::remote_set_url("origin", <url>)`
  改指，再按需增量 fetch。换镜像源不等于换内容，多半是同一份仓库换个入口。

### 2. 增量 fetch（`fetch_ref_to_mirror`）

用 libgit2 打开 bare 镜像，fetch 目标 ref（tag 用 `+refs/tags/{tag}:refs/tags/{tag}`，
默认分支用 `+refs/heads/*:refs/heads/*`），`depth(1)`，`AutotagOption::Auto`
（不是 `All`：每次 pull 重谈整个 tag 表正是镜像要消掉的成本）。

**实测（2026-11-05，真仓库 `deepseek-ai/deepseek-harness` 经 `gh-proxy.com`）：**

| 动作 | 耗时 | 新增对象 | 镜像体积 |
|---|---|---|---|
| 首版本 `dsh-v0.2.2-rc.1` | 10.0 s | 22591 | 45.7 MB |
| 第二版本 `dsh-v0.2.2-alpha.3` | 2.2 s | 1228 | 46.0 MB |
| 重复已有版本 | — | **0** | 不变 |

第二个版本只传 1.2k 对象、2.2 秒，这就是「不再全量」的实测证据（单测用本地仓库
断言同样的性质：`second < first` 且重复 fetch 为 0）。

### 3. 本地 checkout（`checkout_from_mirror`）—— 初稿也错了

> **实测修正**：初稿写「从本地镜像 clone 到 staging」。实测两条路都不便宜，
> 而第三条明显更好：
>
> | 方式 | 耗时 | `.git` 体积 | 说明 |
> |---|---|---|---|
> | libgit2 本地 clone + checkout | 92 s | 392.9 MB | 会复制整个对象库 |
> | 全量网络浅克隆（今天的行为） | 116 s | 172.3 MB | 现状基线 |
> | init + alternate 借用镜像 `objects/` + checkout | **64 s** | **2.5 MB** | 采用 |
>
> 注意 libgit2 在 Windows **不会硬链接**（`clone.c` 的 `can_link` 在 `GIT_WIN32` 下
> 恒返回 false），所以本地 clone 是真的复制 375 MB —— 这不是可优化项，是平台事实。
> alternate 让 checkout 直接读镜像的对象库，`.git` 只剩 2.5 MB 的 stub。

`checkout_from_mirror` 做先后顺序很要紧：先在 bare 镜像里确认 commit 存在，再在目标
目录 init，再 `odb().add_disk_alternate(<mirror>/objects)`，然后**在 borrowing 仓库里**
`find_object`。libgit2 的 `git_checkout_tree` 会校验对象归属（`checkout.c:2761`
`git_object_owner(treeish) != repo` → `object to checkout does not match repository`），
在镜像句柄里取到的对象不能拿去给另一个仓库 checkout。

**checkout 之后 `.git` 必须删掉**（`drop_checkout_metadata`，实测 0.018 s）：它含有指向
镜像的绝对路径 alternate，而 prepared base 是**源码**，发布出去不能依赖一个用户随时
可能 purge 的缓存。删掉不影响后续步骤：DSH 的 `postinstall` 在
`git rev-parse --show-toplevel` 失败时直接早返回，而 commit 是通过
`DSH_CLIENT_COMMIT_HASH` 环境变量传进去的（`versions.rs:307`）。

取消检查点是 libgit2 的 `transfer_progress`（传输）与 `CheckoutBuilder::notify`
（checkout，返回 false 即中止）；clone 之后的 `task.check_cancelled()` 是第二道闸。

### 4. 离线行为（关键）

**离线时不得删除镜像。** 先查镜像，再谈网络 —— 顺序就是离线能力本身：

```
ensure_bare_mirror → resolve_in_mirror(镜像里已有？)
    是 → checkout，全程零网络
    否 → fetch_ref_to_mirror → resolve_in_mirror
            仍失败 → 重建一次镜像（这是唯一允许重建的情形）
```

| 情况 | 判定（可机械执行） | 动作 |
|---|---|---|
| 网络不可达 | `git2::Error::class()` ∈ `ErrorClass::Net` / `Http` / `Ssl` | **保留镜像**，报错 |
| 连上了但 ref 缺失 | fetch 本身成功，`revparse_single(tag)` 在镜像里找不到 | 重建一次 |

判定靠 `ErrorClass` 这个真实枚举，不靠错误字符串匹配。注意 git2 里没有
`ErrorClass::Network`（正确名是 `Net`），也没有 `ErrorClass::Timeout`：超时以 `Net`
class 报出，所以 class 判定已覆盖它。`ErrorClass::Odb` / `Os` 既可能传输失败也可能
存储损坏，因此**不能单看 class 删镜像**：判定顺序是「先问连没连上，再问 ref 在不在」。

### 5. 磁盘预检

`box-foundation::available_disk_space`（Windows `GetDiskFreeSpaceExW`，unix `statvfs`）
在 fetch / checkout 前各查一次；查不到就不拦（「无法测量」不等于「不够」）。

> **实测修正（初稿的 8.8 GB 是旧数字）**：设计当天 D: 只剩 4.58 GiB。数量级上
> 镜像首版本约 46 MB，checkout 峰值约 145 MB（工作树 + 2.5 MB stub），已与
> 一次网络克隆（172 MB `.git`）相当甚至更省，所以这不是新的占用大头。
> 门槛取 `MIRROR_MIN_FREE_BYTES = 320 MB`、`CHECKOUT_MIN_FREE_BYTES = 320 MB`。

### 6. 删除入口

`dshbox template mirror`（`src/cli/template.rs`）显示路径 / 是否存在 / 体积；
`dshbox template mirror purge` 走 RPC `purge_harness_mirror`，**作为 scheduler 任务**
执行 —— 不能在 pull 写镜像时删它。CLI 的 `template` 只按第一个 token 分发，所以
`mirror` 是**两级分发**（`mirror` → `purge`），help 文本两处都已更新。

镜像是**缓存不是用户数据**：`prune_orphaned_data` 只扫 `data/`，不碰它；不进导出、
不进拷贝、不计入用户数据统计。UI 不给按钮，只有 CLI。

## 并发：锁的真实语义

`box-scheduler` 的 `resource_keys` 是**整任务粒度**：`try_start` 时整体占用，finish 时
整体释放。它**不支持**任务内分段持锁。所以持 `harness-mirror` 的任务整个期间都占着
它，包括本不需要排他的 checkout —— 即 **pull 之间在镜像上串行**。这是已知限制，
拆成两个 scheduler 任务（fetch 持 key、checkout 不持 key）列为后续；先落地最简单的
单任务形态，因为「正确」比「最短」重要。

镜像用**独立 key** `harness-mirror`，不与 `repository:dsh-versions` 合并：catalog 刷新
是秒级 `ls-remote`，镜像重建可能是分钟级，合并会让一次重建把 catalog 也堵上。
同一个 pull 现在持 **3** 个 key（`runtime:<version>`、`harness-mirror`、
`repository:dsh-versions`），**不要再加第 4 个**。

## 不在范围

- 任意仓库的 clone（`data.rs` / `extensions.rs` / `bundles.rs` 那三条）。
- prepared base 的索引、去重、`validate_prepared_harness` 逻辑 —— 镜像是**源码的来源**，
  不是另一层 base 缓存（prepared base 落在 `<runtime>/templates/base-<digest>/`，
  镜像不参与它的寻址）。
- `client_artifacts_present` 的平台规则。

## 文件清单（实际）

```
src-tauri/crates/box-foundation/src/lib.rs   +harness_mirror_dir / +available_disk_space
src-tauri/crates/box-runtime/src/mirror.rs   新增：镜像是实现主体（含 7 个单测）
src-tauri/crates/dshboxd/src/versions.rs     pull 改走 mirror::stage_mirrored_checkout
src-tauri/crates/dshboxd/src/dispatch.rs     +harness-mirror key / +harness_mirror_info / +purge
src-tauri/src/cli/template.rs                +mirror 动词（purge 子动作）+ help 文本
src-tauri/src/cli/mod.rs                     +help 文本
docs/design/2026-11-04-shared-harness-mirror.md   本文件
```

## 验收

1. **第二个版本不再全量**：单测
   `mirror::tests::fetch_ref_to_mirror_is_incremental_across_versions` 断言
   `second < first`，且重复 fetch 同一版本新增对象数为 **0**。
2. **UI 与 CLI 同一条镜像路径**：两者都经 `versions.rs` 的 `stage_mirrored_checkout`；
   实测一次真实 pull 的 task log 是三行：
   `serving … from the shared Harness mirror`、
   `fetched … (22591 new objects; mirror is 43 MB)`、
   `installing DSH dependencies`。
3. **离线可用**：`resolve_in_mirror` 全程不联网（单测
   `resolve_in_mirror_reports_a_missing_version_without_network`）；镜像里已有的版本
   直接 checkout，git 不会发起任何请求。
4. **损坏会重建一次**：fetch 成功但 ref 仍缺 → `rebuild_bare_mirror`；单测
   `purge_removes_the_mirror_and_reports_the_bytes_freed` 覆盖删除/重算，
   `fetch_into_a_moved_remote_url_repoints_the_mirror` 覆盖换源不丢失对象。
5. **取消仍然有效**：`MirrorCallbacks` 的取消闭包同时接到 transfer 与 checkout；
   单测 `a_cancelled_fetch_stops_and_a_cancelled_checkout_writes_nothing` 断言取消不会
   删掉已有镜像。
6. **磁盘不足明确报错**：`require_free_space` 在写入前抛错，且「无法测量」不拦。
7. `cargo test --workspace` + `pnpm build` 全绿。

## 风险

| 风险 | 对策 |
|---|---|
| 镜像挤占运行盘（设计时 D: 仅 4.58 GiB） | 实测首版本 46 MB、checkout 峰值 145 MB，均不大于它取代的网络克隆；落盘前预检；只留对象不留工作树；提供 `mirror purge` |
| 镜像损坏 | 对象层错误 → 删除重建一次；传输层错误 → 保留镜像并明确报错 |
| 离线时误删镜像 | 先查镜像再谈网络；重建只在「fetch 成功但 ref 缺失」时发生 |
| `config set mirror` 让 remote URL 漂移 | `MIRROR_SOURCE_URL` 比对 + `remote_set_url`，不因换源重建 |
| 锁粒度限制：整任务持锁，checkout 也被串行化 | 已记录为已知限制，拆两个任务列为后续 |
| 本地 clone 复制 375 MB（Windows libgit2 不硬链接） | 改用 alternate 借用对象库：`.git` 2.5 MB |
| 镜像缓存被误当用户数据 | 明确是缓存：不进导出/拷贝/统计；`prune_orphaned_data` 不扫它 |
| 浅镜像失去「取任意旧 tag」能力 | 有意取舍：只要 fetch 过该 tag 就有；未 fetch 过的按需浅取，代价与一次克隆同量级 |
