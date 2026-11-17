# 2026-11-17 — 源文件按关注点拆分

> 状态：**设计中，未实施**。前一步（`refactor/extract-inline-tests`，PR #46）已把
> 38 个文件的内联测试搬到 `<name>_tests.rs`，十大文件从 18,500 行降到 13,722 行
>（-26%）。本文处理剩下的部分——那 26% 里绝大部分是**源码本身**，剥离测试救不了它。

## 1. 为什么需要设计而不是机械拆分

测试剥离有唯一正确的答案（搬到旁边），源码拆分没有。同一个文件里的函数
可以按很多种方式切开，切错了比不切更糟：模块边界会成为后来者的思维定势，
而一个错误的边界会让「找某个函数」变成「先猜它在哪个模块」。

所以每条边界都要能说清**凭什么**，说不清的就不拆。

## 2. 拆分判据（按优先级）

1. **变更原因不同就分开。** 两个函数从不因为同一个需求而同时修改，就不该
   在同一个文件里。这是唯一不可避免的判据，其余都是它的推论。
2. **依赖方向单向。** 若 A 组调 B 组而 B 组从不调 A 组，边界就是 A → B；
   若互相调用，说明切错了，应该合并或者找第三层。
3. **可独立命名。** 拆出的模块要能用一个比原文件更具体的词命名。命名不出来
   说明这一组没有共同关注点。
4. **有测试边界佐证。** 测试文件已经按模块分开的（PR #46 的产物），其原
   内联模块的名字就是现成的关注点划分证据。

## 3. 逐文件方案

### 3.1 `dshboxd/src/dispatch.rs`（1808 行，59 项）

这是最大的一块，但它有一个已经存在的边界：**文件自己就分两半**——
`dispatch`（L91-393）是路由表，其余是它路由到的 handler。

| 新模块 | 内容 | 行数估计 | 依据 |
|---|---|---|---|
| `dispatch.rs`（保留） | `dispatch_table`、`dispatch`、`SHUTDOWN`、`HandlerResult` | ~390 | 路由表自身，判据 2 的顶层 |
| `rpc_containers.rs` | 容器生命周期与查询：`list_containers`、`*_container_rpc`、`describe_container_rpc`、`container_url*`、`record_requests_wsl_vm` | ~430 | 判据 1：容器语义的变更从不牵动镜像语义 |
| `rpc_templates.rs` | 模板与产物：`read_template_list`、`template_info`、`enqueue_pull_template`、`enqueue_build`、`enqueue_template_container` | ~300 | 判据 1+3 |
| `rpc_extensions.rs` | 仓库与插件：`list_repository_*`、`enqueue_repository_*`、`*_extension_*`、`plugin_dependency_graph_rpc`、`enqueue_plugin_export` | ~330 | 判据 1+3 |
| `rpc_settings.rs` | 配置写入：`save_mirror_settings_rpc`、`save_github_token_rpc`、`save_runtime_directory_rpc`、`harness_mirror_info_rpc`、`enqueue_purge_harness_mirror` | ~130 | 判据 1：设置项的增删与业务方法无关 |

**不拆的**：`enqueue_task_worker` / `spawn_task_worker` / `enqueue_*` 这一族
留在 `dispatch.rs`。它们是路由到 scheduler 的薄封装，共用一个 `enqueue_task`
原语，拆开只会让「加一个新任务入口」要跨文件改两处。

### 3.2 `dshboxd/src/versions.rs`（945 行，25 项）

这个文件混了三件事：索引持久化、远端目录、以及一次真实的 prepare 流程。

| 新模块 | 内容 | 依据 |
|---|---|---|
| `prepared_base_index.rs` | `PreparedBaseRecord`、`index_path`、`read/write_prepared_base_index`、`prepared_base_for_version`、`list_prepared_bases`、`migrate_runtime_runtimes_to_templates` | 判据 1：纯存储，不碰网络也不碰 pnpm |
| `dsh_catalog.rs` | `mirror_url_for`、`fetch_remote_dsh_tags*`、`refresh_dsh_catalog`、`list_dsh_versions_derived`、`list_installed_dsh_versions` | 判据 1：只读远端 |
| `versions.rs`（保留） | `pull_template_with_cancel` 及其全部 stage 辅助（`stage_mirrored_checkout`、`run_pnpm_task*`、`validate_prepared_harness`、`ensure_base_client_artifacts`） | 判据 2：这是前两者的编排者 |

注意 `sealed.rs` 里有一组同构的 pnpm/approval 辅助（`run_pnpm_command`、
`add_pnpm_build_approval`）。**本次不动它们**——跨文件合并是另一个决定，
混进来会让这个 PR 的评审面爆炸。记入 §5。

### 3.3 `dshboxd/src/debug.rs`（2211 行，67 项）

测试剥离后仍然是最大的文件。它的 67 个项里，真正的「调试会话管理」只占
前 150 行，其余全部是 **CDP 协议操作**。

| 新模块 | 内容 | 依据 |
|---|---|---|
| `debug.rs`（保留） | 会话表：`lock_sessions`、`current_browser`、`browser_status_rpc`、`write_config`、`set_browser_path_rpc`、`close_sessions_of_stopped_containers`、`open_rpc`、`close_rpc` | 判据 1：会话生命周期 |
| `debug_dom.rs` | DOM 查询与交互：`console_rpc`、`render_remote_object`、`parse_elements`、`query_elements_rpc`、`click_*`、`type_text_rpc`、`press_key_rpc`、`locate_by_name`、`page_text_rpc`、`scroll_rpc`、`hover_rpc`、`navigate_rpc` | 判据 1：CDP 方法族，变更只跟随协议 |
| `debug_shot.rs` | 截图：`screenshot_dir`、`prune_screenshots`、`png_dimensions`、`write_screenshot`、`screenshot_rpc` | 判据 1+3：自成一体，无 CDP 依赖 |

`debug_dom.rs` 仍会很大（约 1400 行）。这是诚实的上限：CDP 操作就是有
这么多种，硬按「点击 / 输入 / 查询」再切会把 `dispatch_click` 和
`click_element_rpc` 拆到两个文件，反而更难找。**这一步做到 1400 就停**，
记入 §5 作为后续候选。

### 3.4 `box-resources/src/transfer.rs`（971 行，41 项）

这个文件有四层清晰的依赖：文件系统原语 → 载荷布局 → YAML section 操作 →
对外 extract/inject。

| 新模块 | 内容 | 依据 |
|---|---|---|
| `transfer/tree.rs` | `safe_join`、`tree_stats`、`tree_digest`、`collect_files`、`copy_tree`、`restrict_permissions`、`pack_tar`、`unpack_tar` | 判据 2：被上层调用，从不调用上层 |
| `transfer/parts.rs` | `PayloadPart`、`PARTS_FILE`、`payload_parts`、`read/write_parts_manifest`、`part_dir`、`extract_parts`、`inject_parts`、`inject_one` | 判据 1：载荷布局的版本演进 |
| `transfer/section.rs` | `SECTION_FILE`、`section_value`、`nest`、`descend_mut`、`remove_section`、`entries_at`、`walk_entries`、`write_section`、`merge_yaml`、`merge_value` | 判据 1：YAML section 语义 |

`read_entries`（列举目录并排序）归 `tree.rs`：它是文件系统原语，最初被写在
`parts.rs` 里，于是 `tree.rs` 与 `section.rs` 都要反向 import 它，构成一个环。
评审 PR #47 时发现并指出——判据 2 是靠这次搬迁满足的，不是最初的摆放。
| `transfer.rs`（保留） | `Extracted`、`Injected`、`extract`、`inject`、`extract_path`、`extract_section`、`inject_section` | 判据 2：编排者 |

目录改模块只需把 `src/transfer.rs` 换成 `src/transfer/mod.rs` + 三个子文件，
对外 `pub use` 路径不变——**这是唯一不需要改任何调用方的拆法**，所以它
应该第一个做，用来验证整套流程。

### 3.5 `dshboxd/src/sealed.rs`（1476 行，42 项）

| 新模块 | 内容 | 依据 |
|---|---|---|
| `sealed_index.rs` | `SealedTemplateRecord`、`index_path`、`read_index`、`write_index`、`sealed_template_by_name`、`prune_sealed_template_snapshots` | 判据 1：纯存储 |
| `pnpm_approval.rs` | `pnpm_plugin_spec`、`pnpm_*_allow_build_key`、`pnpm_ignored_build_keys`、`build_approval_hint`、`add_pnpm_build_approval`、`seed_profile_recipe` | 判据 1：supply-chain 审批，AGENTS.md 点名不要放松的那块 |
| `client_artifacts.rs` | `CLIENT_ARTIFACT_MARKER`、`ClientArtifactMarker`、`build_platform`、`write_client_artifact_marker`、`client_artifacts_present`、`ensure_base_client_artifacts` | 判据 1+3：平台判定独立于模板语义 |
| `sealed.rs`（保留） | 构建/导出/导入/建容器：`build_sealed_template_from_script`、`seal_template`、`export/import_sealed_template`、`create_container_from_sealed` | 判据 2：编排者 |

### 3.6 其余文件：本次不动

| 文件 | 行数 | 为什么不拆 |
|---|---|---|
| `lifecycle.rs` | 1143 | 已按「启停 / 插件实体化 / 健康看护」三块分开，且 `start_dsh_container_inner` 一个函数就占 400 行——该拆的是那个函数，不是文件 |
| `extensions.rs` | 1101 | 22 个项已全部是同一关注点（仓库扩展），没有可命名的子分组 |
| `resources.rs` | 1111 | 同上，28 项全是资源 RPC |
| `containers.rs` | 1082 | 460 行是两个 guide skill 的字符串常量，不是逻辑——见 §5 |
| `bundles.rs` | 982 | 14 项，全是 bundle 生命周期 |
| `updates.rs` | 1024 | 42 项但依赖方向混乱（RPC、网络、后台任务互调），拆前需要先理清，风险高于收益 |
| `box-plugin-graph/{lib,extract}.rs` | 912 / 863 | 图构建与源码扫描是两个关注点，但 `extract.rs` 内部 37 项是一个紧密的扫描状态机，拆它等于把状态机切碎 |

## 4. 实施顺序

顺序由风险决定：先做不需要改调用方的，建立信心与流程，再碰调用面大的。

| # | 内容 | 风险 | 为什么这个顺序 |
|---|---|---|---|
| 1 | `box-resources/transfer.rs` → `transfer/` 目录 | 低 | 对外路径不变，零调用方改动；用来跑通「拆完立刻全量测试」的流程 |
| 2 | `versions.rs` 三拆 | 低 | 依赖方向清晰（索引/目录被编排者调用），无循环 |
| 3 | `sealed.rs` 四拆 | 中 | `pnpm_approval` 是 supply-chain 敏感区，独立成模块后它的测试边界更清楚 |
| 4 | `dispatch.rs` 五拆 | 高 | 调用方最多（`main.rs` 路由 + 各 RPC 名），最后做 |
| 5 | `debug.rs` 三拆 | 中 | CDP 操作独立性最强，但文件最大，放在流程成熟后 |

每一步独立一个 PR，独立跑 `cargo test --workspace` 与 `layers:check`。
任何一步的测试数变化就停下，不回滚PARTIAL。

## 5. 记录在案但不做

- **`start_dsh_container_inner` 单函数 400 行**（`lifecycle.rs:128`）。文件拆分
  不解决它，需要的是函数内提取（环境准备 / 进程启动 / 就绪探测）。
- **`containers.rs` 的 460 行 skill 文本常量**。正确归宿是 `include_str!` 指向
  `assets/` 下的 `.md` 文件，顺带让 skill 文档可以被单独 lint。
- **`versions.rs` 与 `sealed.rs` 的 pnpm 辅助重复**（`run_pnpm_task` vs
  `run_pnpm_command`）。合并它们是行为等价性审计，不是拆分。
- **`debug_dom.rs` 预计仍约 1400 行**。CDP 操作种类使然，进一步拆分需要按
  「输入 / 查询 / 导航」切，但那会把 `dispatch_click` 与 `click_element_rpc`
  分开，增加查找成本。留到有实际痛点再做。

## 6. 验收

每一步都要满足：

1. `cargo test --workspace --offline` 的 suite 数、通过数、失败数与改动前
   **完全一致**（测试数不一致 = 有测试被静默丢掉或改变）。
2. `pnpm layers:check` exit 0，成员数不变。
3. `git diff --stat` 里只有 `.rs` 文件与必要的 `mod` 声明；不动 `Cargo.toml`、
   不改任何函数签名（除了 `pub(crate) use` 的可见性调整）。
4. 对外 API 不变：第 1 步的 `transfer/` 目录化必须让 `box_resources::transfer::x`
   全部继续可用。