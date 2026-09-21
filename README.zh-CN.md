# DSH Box

[English](README.md) · **简体中文**

**DeepSeek Harness 桌面运行时管理器** —— 在本机运行、隔离、扩展多个 DeepSeek Harness 环境，不需要开浏览器标签页。

DSH Box 是一个用 [Tauri 2](https://tauri.app) 写的轻量桌面外壳，负责安装、启动和管理相互独立的 DSH **容器**（Container）——每个容器有自己的 DSH 版本、profile、插件、技能、工作区和日志——并把它们渲染在内嵌的 WebView 里。

![容器列表：每个容器有自己的 DSH 版本、profile 和宿主进程，带启动、打开、重建等操作](docs/images/containers.png)

---

## 功能亮点

- **容器相互隔离** —— 每个容器自己的 DSH 版本、profile、工作区和宿主进程。
- **内嵌 WebView** —— DSH 界面在原生窗口里打开，不用端口转发、不用复制 URL。
- **几秒就绪** —— 拉取的 Harness 版本自带已构建的客户端产物，`dshbox run` 只是复制加 store 链接（~10 秒）。
- **插件依赖图** —— 加载了什么、按什么顺序、为什么：深度分层、宿主与浏览器半身分开绘制、可以把某一层折叠掉。
- **资源：提取、注入、编辑** —— 会话、provider 密钥、插件状态，都是具名副本；一个 kind 可以跨文件，任意 YAML 片段按 key 路径编辑。
- **面向 agent** —— `dshbox apply -f` 用一份文档配置资源，所有动词支持 `--json`，容器文件可按 key 路径树读取。
- **零依赖安装** —— 自带 Node、npm、pnpm 与 Git（Windows），运行在干净环境里。
- **内置版本管理** —— 安装任意 Harness tag，按容器固定版本。
- **boxfile** —— `FROM` + `ADD` 描述一个模板；构建一次，多次运行。
- **可移植模板** —— 所有负载都物化进模板，容器不依赖任何可变的东西。
- **整合包** —— 把插件和技能编成包，快速导出（保留 URL）或完整导出（单一归档）。
- **看得见的任务** —— 排队、带耗时的日志、可取消、有历史。
- **daemon 慢也不冻窗口** —— 命令不在主线程执行，每个 RPC 都有超时。
- **RPC + 事件流** —— 一个 `POST /rpc` 加一条 SSE 流，界面、CLI 和 agent 共用。
- **对网络友好** —— GitHub 镜像、npm 仓库镜像、自动代理探测。
- **托盘与后台服务** —— 窗口关掉 `dshboxd` 仍继续工作。
- **轻量** —— Tauri，不是 Electron。
- **双语界面** —— English 与简体中文。

![插件列表：每个已安装插件一行，带类型、存储方式、缓存状态、自动收录标记，以及它来自哪些模板和容器](docs/images/resources-plugins.png)

---

## 安装

从本仓库的 **Releases** 页面下载对应平台的安装包：

| 平台 | 产物 | 说明 |
|---|---|---|
| Windows (x64) | `dshbox_<version>_x64_<locale>.msi` | 含捆绑运行时与 sidecar 的 MSI |
| Linux (x64) | `dshbox-<version>-amd64.deb` | Debian/Ubuntu 包 |
| macOS (arm64) | `dshbox-<version>-arm64.dmg` | Apple Silicon |

> 最新版本见 [Releases 页面](https://github.com/Nexus-Aethra/DSHBox/releases)，产物命名遵循 `<product>-<version>-<arch>`。每个 tag 都会由 [release workflow](.github/workflows/release.yml) 构建三个平台。

Windows 上没有运行时前置要求——捆绑的 Node/npm/pnpm/Git 就在安装包里。Linux 需要系统 Git（`apt install git` 或发行版等价命令），其配置按运行时目录隔离，因此 DSH Box 的构建不会读宿主 `~/.gitconfig`。

---

## 快速开始

1. **启动 DSH Box**，按提示选一个可写的*运行时目录*（所有 DSH 数据都在这里）。
2. 打开 **资源** → **Harness**，安装你需要的 DSH tag。
3. 打开 **资源** → **模板**，拉取官方 DSH 模板，或用 boxfile 构建一个可复用模板。
4. 打开 **DSH 容器** → 从该模板创建容器（填名字与 profile）。
5. 按 **启动**，DSH Box 会启动这份准备好的拷贝，并在内嵌 WebView 里打开 DSH 界面。创建容器只是复制 + 链接 store；客户端产物在拉取 Harness 版本时就已经构建好了。
6. 用 **资源** 导入插件/技能、组装整合包，或写 boxfile 得到可复用的插件模板。

### 托盘

关闭窗口时应用会最小化到系统托盘。用托盘菜单可以打开窗口，或启动/停止/重启 `dshboxd` 后台服务。

---

## 架构

DSH Box 拆成 Tauri **桌面外壳**、一个不依赖框架的 Rust workspace、一个后台 **daemon**（`dshboxd`）和一个小型 React 前端。这样拆是为了让所有业务逻辑（插件获取、容器生命周期、模板解析、后台任务）脱离 UI 就能测试，也让 CLI 或外部 agent 能驱动和界面完全相同的流程。

![DSH Box 架构：React 界面经 IPC 与 Tauri 外壳通信，外壳与 CLI 都通过 loopback RPC 驱动 dshboxd，daemon 用捆绑运行时为每个容器监管一个 DSH 宿主](docs/images/architecture.svg)

### 分层组件

| 层 | 内容 | 为什么这样分 |
|---|---|---|
| 前端（React 18 + Vite，`src/`） | 页面、组件、`useTaskQueue`/`useContainers`/`useResources`/`useSettings` 等 hook。**没有业务逻辑**——页面只发 RPC 并对 daemon 的 SSE 事件做出反应。 | 让 Box 界面保持轻薄，并让任何客户端（界面/CLI/agent）共用同一条代码路径。 |
| 桌面外壳（Tauri 2，`src-tauri/src/`） | 浏览器窗口、托盘、Tauri IPC 适配层。通过 loopback HTTP 与 daemon 通信。所有实际工作都交给 `dshboxd`。 | 状态变更只有一个源头，界面与 CLI 不会各自漂移。 |
| Daemon（`src-tauri/crates/dshboxd`） | 常驻后台服务。拥有任务队列、文档存储、模板索引、容器注册表和 SSE 事件总线。单一 HTTP 入口（`POST /rpc`）加 `GET /events?token=…`。 | 安装、重建、卸载等后台工作不会随窗口关闭而中断。 |
| Crate workspace（`src-tauri/crates/`） | 不依赖框架的 Rust crate：`box-foundation`、`box-runtime`、`box-scheduler`、`box-state`、`box-toolchains`、`box-dsh-versions`、`box-containers`、`box-extensions`、`box-image`、`box-template-core`、`box-data-scheduler`、`box-logger`、`box-dsh-context`、`box-server-core`、`box-api`、`box-client`。 | 纯函数 + 单元测试；只有顶层 `dshbox` 与 `dshboxd` 链接 Tauri/HTTP。 |

依赖方向是单向的：`foundation / runtime / scheduler / state` → 功能 crate → Tauri/桌面适配层。功能 crate 不依赖 Tauri，也不依赖彼此的易变状态。

### Daemon —— 双模 RPC + SSE 事件流

界面 / CLI 的每个动作都落到 `POST /rpc`，JSON 体为 `{"method": "...", "params": {...}, "token": "..."}`。daemon 的 dispatch 表决定每个 handler 是**同步**返回 JSON（列模板、读设置……）还是**异步**排入 worker（安装、构建、启动容器、重建、卸载……）。异步 handler 立即返回 `TaskRecord`；客户端订阅 `GET /events?token=…` 收 `task_stage` / `task_log` / `task_finished` / `resource_added|updated|removed` 事件。daemon 对每个请求单独开一个线程，因此一个长任务不会挡住存活探测。

这意味着同一套 HTTP 面服务所有消费者——桌面应用的 Tauri IPC、CLI（`dshbox rpc …`）、以及用 `curl -d '…' http://127.0.0.1:<port>/rpc` 的外部 agent。没有「客户端兜底」也没有本地状态分叉：daemon 的资源表和任务队列是唯一的事实来源。

任务日志是界面观察长任务的唯一窗口：daemon 写它，桌面把日志行流进面板，用户要等几分钟的步骤会说明它在做什么、花了多久。

![任务面板：一次容器启动展开日志，每步一行并带耗时](docs/images/tasks.png)

### boxfile 与模板构建流水线

**boxfile**（`.dsh`）是描述你想实例化的容器的声明式脚本。`dshbox build` 把它解析成 **封存模板（sealed template）**：不含 `node_modules` 的 Harness 源码实体，加上 profile、本地插件产物、技能、以及 `ADD` 指令所需的数据。`dshbox run <template>` 把它复制到最终容器目录并离线链接依赖；客户端产物在准备 Harness 版本时就已构建好。

完整语法见 [`docs/template-system.md`](docs/template-system.md)，覆盖所有来源形态的参考例子见 [`examples/boxfile-plugin-chains.dsh`](examples/boxfile-plugin-chains.dsh)。最小形态：

```text
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME my-team

ADD plugin github.com/owner/cordis-plugin-foo:1.2.3
ADD plugin npm:@linxin666/dsh-web-ui-all
ADD plugin ./plugins/secret
ADD skill team-conventions
```

| 指令 | 必需 | 说明 |
|---|---|---|
| `FROM <ref>` | 是（恰好一次） | GitHub 短写法（`github.com/owner/repo[:tag|@ref]`），或本地模板名（如 `web-base`）。最多四层模板继承。 |
| `PROFILE <name>` | 是（恰好一次） | 目标 DSH profile（`web`、`headless`……）。 |
| `NAME <image-name>` | 否 | 默认取脚本文件名。 |
| `VERSION <image-version>` | 否 | 默认 `latest`。 |
| `LABEL key=value` | 可重复 | 附加到已构建模板上的自由元数据。 |
| `DEF <name> @<path>` | 可重复 | 定义路径别名，供后续 `ADD` 用 `@<name>` 引用。 |
| `ADD plugin\|skill\|data <src> [@<dest>]` | 至少一条 | 要烤进每个由该模板创建的容器的资源。 |
| `CP <src> [@<dest>]` | `ADD plugin` 的别名 | 向后兼容保留。 |

`<src>` 支持几种形态：
1. **GitHub 短写法** —— `github.com/owner/repo[:tag|@ref]`，经 `pnpm pack` 走与 npm 相同的获取/导入流水线；带 tag 的发行版会成为 `ParsedSource::Github` 上的 `ref_`。
2. **压缩包** —— `https://…/pkg.tgz`、`./relative.tgz`、`/abs/path.tgz`。
3. **本地目录** —— `./plugins/foo` / `/abs/path/foo`，直接导入（不做归档往返），适合开发中的插件。
4. **裸名称** —— `name[@version]` 或 `@scope/name[@version]`，指已在资源库中的插件。
5. **显式前缀** —— `git:…`（用 libgit2 克隆，不做猜测）与 `npm:…`（把 registry spec 交给 pnpm）。

`:latest` 与显式 `latest` 关键字等价，都指向 harness 仓库主分支。

每条 `ADD` 的存储方式很重要：
- `ADD plugin` —— 来源作为不可变的本地 `artifact.tgz` 导入共享**资源库**，由封存模板记录，只在最终路径准备容器时才加入。运行中的容器不含任何资源库绝对路径，也不会跑插件依赖安装。
- `ADD skill` 与 `ADD data` —— 快照进数据存储（`<runtime>/data/<digest>/`），在已构建模板中物化，并复制进容器 profile。
- 捆绑的 `dsh-box-context` 插件（`@deepseek-ai/dsh-box-context`）会自动复制，不需要 `ADD`。

各步骤的分工是刻意的：

| 步骤 | 做什么 | 代价 |
|---|---|---|
| `dshbox pull template` | 克隆 Harness 提交，安装依赖**并构建客户端产物**（native 插件、宿主/客户端库、Web 前端）到 prepared base，同时记录构建时的 revision 与平台 | 每个 Harness 版本、每个平台一次 |
| `dshbox build` | 复制该 base（含产物），安装 profile 的插件，产出封存模板 | 每个模板一次 |
| `dshbox run` | 复制封存树、从 pnpm store 链接依赖、物化 Box 的上下文插件、启动宿主 | 每个容器 **~10 秒** |

早于这个改动创建的 base 会在第一次用它构建模板时原地补齐；树里没有「本提交 + 本平台」产物的模板（旧的，或从别的系统导入的）则在创建容器时重建——任务日志会说明走了哪条路。完整的存储、事务与迁移约定见 [`docs/specs/prepared-template-runtime.md`](docs/specs/prepared-template-runtime.md)。注意这是一次 schema 变更：旧的共享 `runtimes/<version>/source` 布局不再被新构建使用。

### 插件依赖图

`plugin_dependency_graph` 回答「加载了什么、按什么顺序、为什么」——对象可以是封存模板、容器目录或运行中的宿主。daemon 遍历包树、扫描每个包的源码，返回节点、关系、层深、加载顺序、共享服务与诊断。模板（资源 → 模板）和容器详情页上的 **依赖图** 按钮会渲染它。

![插件依赖视图：节点按深度分层绘制，被折叠的一层收成一个汇总节点，右侧是诊断列](docs/images/plugin-graph.png)

它计算的是：

- **一个挂载的插件一个节点，而不是一个包一个节点。** 同时提供宿主半身与浏览器半身的包（`dsh.client` + `exports["./client"]`）会拆成两个节点，这样 `inject`/`provide` 在同一个上下文里解析。同名服务在每个上下文各注册一次是共享服务而非冲突——这也意味着「有环」要么在一个上下文内、要么根本不存在。
- **只认已注册的声明。** `inject` 只在插件真正注册它的位置读取：`Object.assign(target, { inject })` 载体，或工厂返回的 `{ inject, apply }`。普通对象属性不是插件声明——把它当声明会在真实 profile 上凭空造出 26 条需求。
- **启动器也是提供者。** `apps/cli` 构建根上下文并把服务交给它挂载的插件，所以它被扫描并绘制为 `dsh (launcher)`；没有它，每个注入 `profileContext` 的插件看起来都在等一个不存在的人。
- **深度分层、加载顺序、以及折叠。** 节点深度 = 它注入的最深依赖 + 1（0 表示无需等待），层内按被依赖数排序。层带标签可以**把该层折叠**成一个汇总节点：该层的边改指到它、其余重新排布，被折叠的层留在画布上方当一个可展开的 chip。有环也不会掩盖顺序：仍在环里的节点标记为挂起。
- **会跟随 bundle。** bundle 的 `cordis.patch.yml` `insert` 行会被读取，因此 bundle 挂载的插件就是它们应有的节点——包括 boxfile `ADD` 的那些，这也是模板预览在还没有容器时就能显示自己插件的原因。
- **环是图的属性，跨层是绘制的属性。** 红色虚线只表示「在环上，这会卡住」，判定基于保留半身的 links 且只算同一上下文。仅仅因为默认视图把同一个包的两半身合并成一个盒子、导致 `host A → B` 与 `browser B → A` 画成回环的边，是安静的灰色虚线，并在图例里写明。把半身分开后跨层线归零——这正是分层本身正确的检验。
- **与可用容器矛盾的结论，算扫描器的 bug。** 不完整的静态视图不该声称某服务缺失、某提供者未激活、或不同上下文里的两次注册是冲突；启动器、包的两半身、isolation scope 各自消掉了其中一类误报。
- **诊断会指名文件。** 扫描器无法解析的内容会连同包与文件一起报告在可折叠面板里，而不是无声丢弃。发布包的客户端入口按 manifest 的 `dsh.client`/`exports` 子路径解析——按**前缀列表**解析，因为一个包会同时有 `lib/client.js` 和 `lib/client/`；没有 `src/` 时读 `lib/`，且从不读构建产物目录。

扫描刻意做成基于文本的——一个作用于「注释已置空」源码的小扫描器，而不是 TypeScript AST：依赖图必须在不加载 DSH 的前提下描述一个容器的 `node_modules`，而错误的节点必须便宜到一眼能看出并改掉。

### 容器资源

容器会持久化非代码状态：`profile/sessions/<workspace>/session-<id>/` 下的会话、`profile/.credentials.yaml` 里的 provider 密钥、以及插件写在自己目录下的东西。Box 把每种都抽象成 **kind**——位置、是否含机密、独立条目的深度——由两个动词搬运。

一个 kind 由**一到多个部分（part）**组成，而一个 part 要么是整个路径，要么是**某个 YAML 文档里的某一段 key 路径**。第二种形态存在的原因是 DSH 把一些状态拆在多个文件里：provider key 是 `.credentials.yaml` 里的一个 ref，而声明「哪个环境变量持有它」的路由在 `settings.yaml` 的 `llm-pi-ai.providers.<route>.apiKeyEnv`——该文件的其余部分属于别的插件。所以 `credentials` kind 同时带这两部分，提取进同一个负载、注入时深合并，密钥因此是**带着使用它的 provider** 一起来的。

- **提取（extract）** 把一个 kind（或其中一个条目，从而让单个会话可以单独搬运）复制到 `<runtime>/resources/<id>/payload/`，计算摘要，并记录进共享文档存储。`--out file.tar.gz` 还会打包以便转移到别的机器。
- **注入（inject）** 从资源库、另一个容器、或 tar 包写回。`Refuse` 是默认策略，会在写入任何东西前检查所有冲突；`Merge` 只替换负载携带的部分并对 YAML map 深合并；`Overwrite` 先替换原值。机密 kind 写出的文件在进出一路都收紧为 `0600`，运行中的容器会被拒绝，除非 `--restart` 要求「停止 → 注入 → 启动」。
- **副本有名字。** 名字来自用户、所选的条目、或来源容器；再取一份会新增 `<name>-2` 而不是覆盖已有的那份，因为「取一份副本」是显式动作——同一个 kind 从两个容器来不应该落在同一行上。

kind 有三个来源：Box 内置 `sessions` 与 `credentials`；插件包可以在 `dshbox.resources` 里自己声明；插件没声明时，Box 读取它已发布代码里的 `join(root, 'a', 'b')` 链，把这些路径作为候选提供——只作为候选，不作为可信路径。

![资源类型标签页：资源库里每一份副本，各自标注来源容器与可附加的目标](docs/images/resource-type.png)

界面上，一个资源类型就是**资源导航里的一个标签页**。`会话历史` 与 `API Key` 内置，**+ 资源类型** 可以再加一个：选一个容器，然后选 Box 在那里检测到的 kind、要**扫描的插件**（profile 里装过的包都会列出并支持输入过滤），或者在**容器存储区文件树**里选一个路径。取一份副本是一个按钮，点开是一张卡片——状态从哪个容器来，以及这份副本叫什么：

![取一份副本：选择容器并为副本命名](docs/images/resource-copy.png)

容器文件里的任意 YAML 片段都能按 key 路径编辑，用的是同一套模型：文档是一棵树，点节点载入那一段，路径不存在就是新增那一段。写回走与注入完全相同的「合并或替换」策略。

![按 key 路径编辑容器文件：文档为树，片段在文本框里](docs/images/resource-editor.png)

```bash
dshbox container resource list <id> [--plugin @scope/name]      # 类型、位置、大小
dshbox container resource types [--json]                        # 资源类型（界面的标签页）
dshbox container resource extract <id> credentials --name prod  # 取一份副本并命名
dshbox container resource stored                                # 资源库里存了什么
dshbox container resource inject <id> credentials-prod --merge --restart
dshbox container resource inject <id> --from <other-container> --kind sessions
dshbox container resource read <id> profile/settings.yaml --section llm-pi-ai
dshbox container resource write <id> profile/settings.yaml --section llm-pi-ai.providers.acme \
  --text 'apiKeyEnv: ACME_API_KEY' --merge
```

### 用 agent 驱动

CLI 是给 agent 的界面，界面是给人的界面，所以 agent 需要的动词都是非交互、可机读的。

`dshbox apply -f <file>` 吃一份文档，让资源层与之一致：

```yaml
container: test                 # 下面条目默认的容器
types:                          # 要注册的资源类型（界面的标签页）
  - label: ACME state
    kind: path
    path: profile/plugins/acme/state
copies:                         # 要提取进资源库的状态
  - name: before-upgrade
    kind: credentials
    from: test
```

apply 是幂等的——类型按 kind + container + path 定位，副本名已存在则报告 `exists`——`--dry-run` 不做任何改动，`--json` 逐条报告结果，未知 key 直接报错而不是静默忽略。读取侧同样可机读：

```bash
dshbox apply -f resources.yaml --dry-run
dshbox apply -f resources.yaml --json
dshbox container resource list <id> --json
dshbox container resource read <id> profile/settings.yaml --json
```

`dshbox help`、`dshbox container resource help`、`dshbox apply help` 不需要界面就能说明每个动词。

### 持久化与引用计数

长期索引以文档形式存放，而不是散落的文件：任务队列走 `box_foundation::collection::DocumentStore`——SQLite，由 `box-store` crate 实现在 `<runtime>/state/dshbox.db`——所以 daemon 与桌面应用共享同一个队列而不各自持有文件。旧的任务 JSON 在首次打开时导入并归档为 `*.pre-sqlite`，schema 变更只向前迁移，由 `PRAGMA user_version` 把关。内容寻址的目录（`templates/<hash>/`、`data/<digest>/`、`runtimes/`）仍然留在磁盘上，`config.json` 留在 `~/.dsh-box/`——机器本地，永不进存储。完整设计见 [`docs/specs/data-scheduler.md`](docs/specs/data-scheduler.md)。

容器、模板、插件、技能都按 id 互相引用，删除是 `软删除 → 快速队列 → 永久删除`，因此仍被使用的实体不会被回收。

### 日志

`tracing` + `tracing-subscriber` 把结构化日志写到 `<runtime>/logs/<component>.log`（按天滚动）并镜像到 stderr。用 `RUST_LOG` 过滤，例如 `RUST_LOG=info,dshboxd=debug,box_template_core=debug`。

---

## 技术选型

| 层 | 技术 |
|---|---|
| 桌面外壳 | Tauri 2、Rust（`src-tauri/` 下的 Cargo workspace） |
| 界面 | React 18、TypeScript、Vite |
| 后台服务 | `dshboxd` sidecar（单一 HTTP 入口：`POST /rpc` + `GET /events`） |
| 捆绑运行时 | Node / npm / pnpm / Git（仅 Windows），按平台归档，SHA-256 固定在 `runtime-lock.json` |
| 目标平台 | Windows x64（MSI）、Linux x64（deb、rpm）、macOS arm64（dmg），全部由 release workflow 构建 |

---

## 从源码构建

前置条件：[Node.js](https://nodejs.org) 22.13+ 与 pnpm（固定的 `packageManager` 需要 `node:sqlite`）、对应平台的 [Tauri 2 前置条件](https://v2.tauri.app/start/prerequisites/)，以及 **7-Zip**（`runtime:prepare` 解压 Windows PortableGit 归档时用一次；从 [7-zip.org](https://www.7-zip.org/) 安装或 `apt install p7zip-full`，并确保 `7z` 在 `PATH` 上）。7-Zip 只是构建期工具，不会进入安装包。

```bash
pnpm install
pnpm runtime:prepare    # 获取 + 校验 + 解压捆绑的 Node/pnpm/Git 运行时
pnpm server:prepare     # 构建 dshboxd sidecar
pnpm tauri dev          # 开发模式运行
```

`runtime:prepare` 在解压前按 `runtime-lock.json` 里的 SHA-256 校验每个归档，不匹配就中止。缺 7-Zip 时 Git 步骤会跳过并给出警告（Node/pnpm 仍会准备）；装好后重跑即可。

各平台发行包：

```bash
pnpm bundle:windows     # Windows MSI
pnpm bundle:linux       # Linux .deb
pnpm bundle:macos       # macOS .dmg
```

跑测试：

```bash
cd src-tauri && cargo test --workspace
```

---

## 发布

版本号在三处必须一致——`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`。三处都改，把变更记录写进 [`CHANGELOG.md`](CHANGELOG.md)，然后打 tag：

```bash
git tag v0.1.8 && git push origin v0.1.8
```

接下来交给 [`.github/workflows/release.yml`](.github/workflows/release.yml)：它用开发者本地同样的 `bundle:*` 脚本构建 Linux `.deb` + `.rpm`、Windows MSI 和 macOS arm64 `.dmg`，并把它们附到该 tag 的 GitHub Release 上。tag 会先与三个版本字段比对，不一致直接失败而不是发布贴错标签的安装包；如果该 tag 已有 release，产物会被重新上传覆盖。手动触发（`workflow_dispatch`）会构建全部产物但不发布，适合验证流水线改动。

安装包**未签名**：macOS 首次打开需要绕一次 Gatekeeper，Windows 会弹 SmartScreen。拿到证书后可以往 workflow 里加签名与公证身份。

---

## 仓库结构

```
src/                       React/TypeScript 管理界面
src-tauri/                 Rust workspace + Tauri 外壳
  crates/                  不依赖框架的细分 crate
    box-foundation         配置、路径、文档集合
    box-store              SQLite 文档存储 + 旧格式导入
    box-runtime            绝对路径进程执行
    box-scheduler          持久化任务队列 + 锁
    box-state              ResourceStateManager（读模型）
    box-toolchains         捆绑 Node/pnpm 解析
    box-dsh-versions       DSH GitHub 目录（harness tag + 安装）
    box-containers         容器元数据 + 活动宿主注册表
    box-extensions         资源库插件/技能扫描与传输
    box-image              .dsh 解析、manifest v6、gzip tar I/O
    box-plugin-graph       插件依赖扫描与图组装
    box-resources          资源 kind、提取与注入
    box-template-core      根/通用模板安装卸载核心
    box-data-scheduler     软删除 + 双队列异步硬删除
    box-logger             tracing 初始化与按天滚动日志
    box-dsh-context        dsh-box-context 插件（paths.dshboxHome/dshboxCli）
    box-server-core        dshboxd 辅助 + 服务安装
    box-api, box-client    RPC 面 + 客户端适配
  src/desktop/app/         领域模块（containers、extensions、tasks……）
  tools/runtime-packager   捆绑 Node/pnpm/Git 运行时打包器
examples/                  boxfile.dsh + plugin-chains 示例
docs/                      HANDOFF.md、template-system.md、specs/、design/、
                           notes/、images/
.github/workflows/         release.yml —— v* tag 构建三平台安装包
```

boxfile 语法的权威参考是 **`docs/template-system.md`**；镜像/已构建模板的设计在 **`docs/specs/image-build.md`**；完整 RPC 与事件流面在 **`docs/design/rpc-and-events.md`**。

---

## 许可

专有软件——授权条款请与仓库所有者联系。

© Nexus-Aethra
