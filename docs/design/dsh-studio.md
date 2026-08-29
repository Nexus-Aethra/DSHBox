# DSH Studio — 插件构筑工作台设计计划

> 状态：构想 / 调研完成，未立项实施。
> 调研基线：DSHBox 分支 `feat/resource-management`（2026-08）；DSH 官方仓库本地克隆
> `/home/wpp/homework/deepseek-harness`（含 `vendor/cordis`、`packages/boot/app-boot` 等）。

## 1. 概念与定位

把 DSH 当作"框架"：做一个工作台，让用户**自由搭配插件**（勾选 DSH 版本、插件、
skills、数据源），点一次"构筑"，产出一个**定制版 DSH 成品**，并封装为终端用户可直接
安装的安装包（NSIS / deb / rpm / dmg）。

- 形态：Tauri 2（不选 Electron——整个引擎层是现成的 Rust workspace + dshboxd RPC，
  换 Electron 等于重写宿主层且无收益）。
- 与 DSHBox 的关系：不是另起仓库。DSHBox 长出第二个产品面——现有 "Box"
  （管理本机 Container）+ 新增 "Studio"（构筑发行版），共享同一条密封流水线，
  只是末端消费者不同：Box 的产物流向本地 Container，Studio 的产物流向安装包。
- 安全叙事（卖点）：构筑期做完全部安装/编译，终端用户拿到的是**密封成品树**，
  安装期零 pnpm、零网络、零生命周期脚本——插件供应链风险被留在构筑者一侧。

## 2. 现有地基（DSHBox 侧，直接复用）

| Studio 需要的能力 | 现有实现 |
|---|---|
| DSH 版本选择与源码准备 | `box-dsh-versions`（GitHub 目录 + install/prepare） |
| 插件目录 / 扫描 | `box-extensions`（仓库插件/skill 扫描）+ 插件三源语法（github 短格式 / `git:` / npm） |
| 构筑（编译+装插件+密封） | `dshbox build` → sealed template，权威设计 `docs/specs/prepared-template-runtime.md` |
| 构筑清单解析 | `box-image`（boxfile / manifest v6） |
| 运行时资产 | `tools/runtime-packager`（按 target 打包 Node/pnpm，`runtime-lock.json` 校验） |
| 长任务编排 | `box-scheduler`（`kind` 为字符串，可加 `package.*` 任务；进度/取消/持久化） |
| UI 骨架 | React 三区布局 + dshboxd RPC，Studio 作为新的顶部分区 |

**唯一缺失的环节**：目前 sealed tree 只流向本地 Container，没有"封装为独立安装包"
的出口。

## 3. DSH 侧机制调研结论

### 3.1 插件安装 = pnpm 转发，无自有管理逻辑

`apps/cli/src/plugin.ts`（158 行）本质是往 profile 目录跑一次 `pnpm add`：

1. 首次使用初始化 profile 目录（`package.json` + `pnpm-workspace.yaml` 模板）；
2. 相对路径参数锚定到调用方 cwd（防止 `add .` 自链进 profile）；
3. 成功后 `reconcilePlugins` 调和 `dsh.profile.bundles`：依赖中凡 package.json
   声明了 `dsh.bundle` 的加入激活层列表；没声明的按普通库处理（警告一次）。

因此 pnpm 的全部源形态天然支持：registry 包名、`git+https://`、`github:o/r`、
`./path` / `link:`（本地实时链接）、`file:`（本地拷贝）。

Git 托管插件的 `prepare` 构建脚本会被 pnpm ≥10 拦截，官方 CLI 的处理是**打印
提示让用户手动把 key 加进 profile 的 `allowBuilds`**——DSHBox 的 `dshboxd/src/sealed.rs`
已把这一步自动化（从 `[ERR_PNPM_IGNORED_BUILDS]` /
`[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED]` 标记派生 key + 多轮重试）。

### 3.2 Profile：插件状态的分层清单堆栈

`$DSH_HOME/profiles/<name>/`，权威代码 `packages/boot/app-boot/src/profile.ts`：

```
profiles/<name>/
  package.json        ← 双重身份：dependencies（安装事实）
                                   + dsh.profile.bundles（有序激活层列表）
  cordis.patch.yml    ← 用户补丁层（可热重载，永远最后叠加）
  node_modules/       ← pnpm 管理的树外插件
```

- 插件自声明：包 manifest 写 `"dsh": { "bundle": { "patch": "./cordis.patch.yml" } }`
  即成为 bundle（插件层）；未声明则只是普通依赖。
- 组合顺序（固定）：空 entry 列表 → 各 bundle 的 patch 层（按
  `dsh.profile.bundles` 顺序）→ profile 的 `cordis.patch.yml` → 启动器 `--patch` 层。
- **双锚点解析**：插件名先从 dsh 安装本体解析（内置插件，仓库源码），再从
  profile 的 `node_modules` 解析（树外）。另有扁平回退目录
  `$DSH_HOME/profiles/node_modules`（内置包 symlink），保证任意 profile 都能
  普通父目录上溯解析到内置插件。

### 3.3 官方仓库本身

- 第一方"插件"就是 `packages/` 下 40+ 个 cordis 插件包，代码全在仓库内；
  `apps/cli` 对它们的依赖全是 `workspace:^`（`linkWorkspacePackages: true`），
  clone 后 `pnpm install` **不会**从网上重新下载任何官方插件，只下载框架的
  第三方 npm 依赖。
- 第三方插件无中央 registry，就是普通 npm 包；官方仓库根目录下的
  `DSH-better-sidebar` 等是本地测试克隆（git 未跟踪）。

**对 Studio 的推论**：构筑清单 ≈ cordis.yml / profile 的翻译（boxfile 已在做）；
构筑出的定制 DSH 与官方版结构完全同质，只是插件列表不同且树已预编译密封，
不需要 DSH 官方任何配合。

## 4. 依赖建模：服务二部图 + DAG 校验

### 4.1 模型：不是树，是二部图

```
插件(节点) ──requires──▶ 服务(节点) ◀──provides── 插件(节点)
```

树会在三处失真：服务多对多（扇入扇出）、菱形依赖、覆盖顺序与依赖关系正交。
二部图直接回答全部问题：

- **排序**：对 provides→requires 边做拓扑排序 = `dsh.profile.bundles` 的合法
  顺序（校验用户拖拽结果，用户可在约束内自由排列以控制覆盖优先级）；
- **缺依赖**：某插件 requires 的服务全图无人 provides → 构筑时报错并列出候选；
- **环检测**：拓扑排序失败 ⇔ 有环（Kahn / DFS 三色，一次遍历两个产出）。
  运行时 cordis 对环的处理是插件**静默 pending**，静态校验把它提前成构筑期的
  可点击报错——这是 DAG 校验的真实价值；
- **UI**：服务放中间一列的"能力中枢"图，比插件直连线可读。

覆盖顺序是独立一根轴（`dsh.profile.bundles` 是列表，天然全序无环），不要和
依赖图混在一张图里。

### 4.2 依赖语义（cordis `vendor/cordis/src/registry.ts`）

- requires：`static inject = ['llm', 'tokenMeter']`（数组）或对象形式
  （每服务可带 intercept 配置，如 `loader: { await: true }`）；另有 `@Inject`
  装饰器。语义是**等待而非失败**：服务不可用则插件一直挂起。
- provides：`ctx.provide('name', ...)` / `ctx.reflect.provide('name', ...)`。

### 4.3 静态扫描可行性（已在官方仓库实测）

- `packages/` 下（排除 test）**68 处 `static inject`**，全为字面量数组；
  `@Inject` 装饰器 **0 处**——官方只有一种声明风格。
- `ctx.provide('…')` / `reflect.provide('…')` 约 300 处，几乎全为字符串字面量。
  高频服务：`sessions`、`sessionPersistence`、`sessionQuery`、`connection`、
  `locale`、`webServer`、`remote`、`workspaces`、`conversation`、`shell` …
- **服务权威目录在类型系统里**：每个服务有对应的
  `declare module … interface Context` augmentation（仓库 180 处）。优先从
  `.d.ts` 提取服务目录，再用 provide 调用点核对提供者。
- 扫描噪声：`src/` 与 `lib/types/*.d.ts` 重复命中（取 src 去重）；provide 可能
  在条件分支（扫得出"存在"，不知何时生效，对构筑图无影响）。

**结论**：官方侧零成本一次性解决（扫一遍 + 人工抽查 → 完整 requires/provides
表 + 依赖图种子）。第三方插件采用双轨：约定插件在 manifest 里显式声明
provides/requires（与 `dsh.bundle` 同一落点），AST 扫描兜底，扫不出时构筑 UI
让用户确认。

## 5. 分阶段落地

### v1 — 构筑编排（零新工具链依赖）

- Studio 页面：表单化构筑清单（DSH 版本 + 插件勾选 + allow-build 授权确认，
  前端生成 boxfile，用户不见 DSL）。
- 链路：构筑清单 → `dshbox build` sealed tree → 导出独立 `.dsh` 包。
- 依赖图 v1：官方种子数据 + 缺依赖/环校验。

### v2 — 出本机安装包

- scheduler 新任务 `package.distribution`：输入构筑清单 + 目标平台，输出安装包。
- 把 `bundle:linux` / `bundle:windows` / `bundle:macos` 逻辑从 package.json
  scripts 下沉为可参数化模块；`bundle-windows.mjs` + `dshbox-install-hooks.nsh`
  已是参数化 NSIS 雏形。
- 成品启动器：dshboxd 砍掉 container/template 概念，保留 runtime 解析 +
  端口/token 逻辑 → 瘦启动器（装好 → 拉起 DSH web server → 开窗）。
- 第一版只做宿主平台原生包；跨平台出包降级为明确提示。

### v3 — 跨平台编译矩阵 + 签名

- Linux 构建 Windows NSIS（wine+nsis 容器化或专用构建镜像）；dmg 锁 macOS。
- Authenticode / codesign / notarization 槽位；默认未签名 + 明确警告。

## 6. 风险与开放问题

- **成品启动器**是新组件（dshboxd 瘦身版），需要独立设计进程模型与更新流
  （定制发行版的 DSH 更新是否走 Studio 重新构筑，还是允许原地更新？）。
- **体积**：密封树 + 捆绑 Node 运行时，安装包偏大；可评估按 target 裁剪。
- **两个顺序维度**易混淆：依赖 DAG 决定能否启动，patch 层叠顺序决定配置覆盖，
  UI 需分开呈现。
- **provides 静态提取覆盖不全**：manifest 显式声明为主、扫描兜底的双轨必须
  先立约定，否则第三方生态长大后补成本高。
- 交叉编译与签名是唯一需要新建基础设施的部分，放在 v3 是刻意的。

## 7. 参考文件

| 主题 | 位置 |
|---|---|
| `dsh plugin` / pnpm 转发 / reconcile | DSH `apps/cli/src/plugin.ts` |
| profile / patch 层叠 / 双锚点解析 | DSH `packages/boot/app-boot/src/profile.ts` |
| cordis inject / 服务注册 | DSH `vendor/cordis/src/registry.ts` |
| loader entry tree | DSH `vendor/loader/src/index.ts` |
| 密封模板权威设计 | `docs/specs/prepared-template-runtime.md` |
| allowBuilds 自动派生（已实现） | `src-tauri/crates/dshboxd/src/sealed.rs` |
| 插件 pnpm 安装流（现有设计） | `docs/design/pnpm-managed-plugin-install.md` |
