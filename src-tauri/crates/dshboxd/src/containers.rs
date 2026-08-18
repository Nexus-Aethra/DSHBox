//! Container creation for daemon-run tasks. Mirrors the desktop's
//! `containers.rs` shareable core (`create_dsh_container_sync` and the
//! profile scaffolding it needs) plus the startup helpers the daemon
//! lifecycle uses (workspace, context snapshot, profile preflight).

use crate::toolchains::{command_for_toolchain, resolve_toolchain, wait_for_process};
use box_containers::DshContainer;
use box_dsh_context::{
    render_patch_yml, render_snapshot, DshContextFiles, DEFAULT_ORDER, PATCH_FILENAME,
    SNAPSHOT_FILENAME,
};
use box_dsh_versions::version_directory as dsh_version_directory;
use box_foundation::{is_safe_identifier, read_config};
use box_scheduler::TaskContext;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

/// Built-in skill dropped into every freshly-created container so new users
/// can open the workspace and immediately read how a boxfile is written.
/// The body covers every supported directive and source shape so users do
/// not have to leave the container to consult documentation.
const BOXFILE_GUIDE_SKILL: &str = r#"---
name: dshbox-guide
description: Use when working with DSH Box — authoring a boxfile (`.dsh`), choosing ADD sources for 插件/技能/数据, running `dshbox` commands to manage templates and containers, or troubleshooting anything that touches container lifecycle, plugins, skills, or the dshbox daemon.
---

# DSH Box Guide (用 dshbox 管理 DSH 容器)

`dshbox` 是 DSH 容器**外部**的管理 CLI——它装模板、拉插件、起停容器、跑 boxfile build/run 全链路。`dsh` 是 DSH 容器**内部**的 agent CLI——你在容器里跟 agent 对话时调的是 `dsh`,不是 `dshbox`。

> 记住这条边界: **容器管理走 `dshbox`;容器内跟 agent 对话走 `dsh`。** 容器里 `dshbox` 也在 PATH 上(它是 thin RPC client,跟本地 daemon 通讯),但你日常基本不会直接调它。

`dshbox` 命令分四组:**Workflow**(init / pull / build / run 起一个完整容器)、**Template 管理**(template ls / show / rm)、**Plugin & Skill**(plugin ls / import / install / skill install)、**Container 操作**(ps / container start / stop / restart / rm / logs / url)。完整列表 `dshbox help`。

## §0 — 一个 30 秒能跑通的完整流程

下面四步把官方 `deepseek-harness` 拉下来,加一个 GitHub 公开插件,启动一个 web 容器并自动弹 webview:

```bash
dshbox init                                                     # 生成 boxfile.dsh 模板
dshbox pull template github.com/deepseek-ai/deepseek-harness:latest   # 拉 base 进本地 store
dshbox build ./boxfile.dsh --name web-with-dsh-ui              # 构筑 built template
dshbox run web-with-dsh-ui                                      # 创建并启动容器
```

如果 `dsh` 在 PATH 上找不到(常见于 Windows 安装后): `dshbox setup-path` 然后**新开一个 terminal**。

## §1 — `dshbox` CLI 速查

### 容器全生命周期(最常用)

| 命令 | 作用 |
|---|---|
| `dshbox init [path] [--force]` | 生成 starter `boxfile.dsh`(已存在则拒绝覆盖) |
| `dshbox pull template <ref>` | 把 base template 拉进本地 store(`github.com/<owner>/<repo>[:tag]`);build 之前必须先做这一步 |
| `dshbox build [boxfile.dsh] [--name tpl]` | 构筑 built template;`--name` 省略时用 boxfile 里的 `NAME` 行,再省则用脚本文件名 |
| `dshbox run <template> [--force]` | 从 built template 创建并启动容器;同一个 template 可以 run 出多个独立容器 |
| `dshbox ps` | 列出所有容器 + 当前 state(`starting` / `ready` / `running` / `crashed` / `stopped` / `orphaned`) |
| `dshbox container url <id>` | 取运行中容器的 webview URL |
| `dshbox container open <id>` | 在 DSH Box 桌面窗口里打开容器 webview |
| `dshbox container logs <id>` | tail 容器 host 进程日志 |
| `dshbox container start <id>` | 启动已 stopped 的容器 |
| `dshbox container stop <id>` | 停止运行中容器(优雅 kill process group) |
| `dshbox container restart <id>` | 启动被 watcher 标 `crashed` 的容器;不重建 build artifacts |
| `dshbox container rebuild <id>` | 重建容器:重跑 `pnpm install` + `pnpm build`,然后重启 |
| `dshbox container rm <id>` | stop + 删目录(`rm` / `remove` 同义) |

### Template 管理

| 命令 | 作用 |
|---|---|
| `dshbox template ls` | 列出 script template 和 built template |
| `dshbox template show <name>` | 看脚本正文或资源清单 |
| `dshbox template rm <name>` | 删 template(被容器引用会拒绝) |
| `dshbox template prune` | GC 无人引用的 snapshot |

### Plugin / Skill / Data / Bundle

| 命令 | 作用 |
|---|---|
| `dshbox plugin ls` | 仓库里的 plugin / skill 列表(`Extensions.json`) |
| `dshbox plugin import <source>` | 从 dir / tarball / github / npm 拉进仓库(后续 ADD 用 bare name) |
| `dshbox plugin install <container-id> <source> [--profile web]` | 给一个已运行的容器临时多装一个 plugin |
| `dshbox plugin rm <name>` | 从仓库删除 entry |
| `dshbox skill install <container-id> <source>` | 同 plugin,但走 SKILL.md frontmatter 路径 |
| `dshbox bundle ls / create / rm / save / load` | bundle(多扩展打包)管理 |
| `dshbox bundle install <container-id> <bundle-id>` | 一次把整个 bundle 灌进容器 |

### 配置 / 元信息 / 调试

| 命令 | 作用 |
|---|---|
| `dshbox config show` | 读 `~/.dsh-box/config.json` |
| `dshbox config set runtime <dir>` | 改 runtimeDirectory |
| `dshbox config set mirror.github <url>` | GitHub 镜像 |
| `dshbox config set mirror.npm <url>` | npm registry 镜像 |
| `dshbox info` | 摘要:storage 大小、bundled runtime、registry 等 |
| `dshbox --version` / `dsh -V` | 版本号 |
| `dshbox rpc <method> [json]` | 直发 raw JSON-RPC,debug 用 |
| `dshbox setup-path` | Windows:写 `HKCU\Environment\Path`;POSIX:append 到 shell rc |
| `dshbox ui` | 启动桌面 GUI(Tauri WebView);Windows 下双击 EXE 也会走这条路径 |

### 子命令约定

- `dsh <command> help` 看某个 command 的 action 级帮助,例如 `dshbox container help`、`dshbox run help`。
- `dsh --help` / `dsh -h` 同样打顶层帮助。
- `dsh` 的所有选项分两类:路径类(`--config`,`--name`, ...)和开关类(`--force`,`--json`,...)。

## §2 — Boxfile (`.dsh`) 写法

`boxfile.dsh` 是 DSH Box 的 Dockerfile 等价物:plain text 描述一个 template,从 `FROM` 起 base、`PROFILE` 选 runtime 布局、`NAME` 给模板起名,`ADD` 把 插件 / 技能 / 数据 layer 上去。

### 一份完整可跑的 boxfile

复制下面这段到 `boxfile.dsh`,**不动任何字符** 就能跑通"init → build → run"全链路:

```dsh
# ── base ───────────────────────────────────────────────────────────
# 这行必须是 FROM;tag 用冒号 `:latest` 是 dshbox 的官方推荐写法(直接复制 GitHub 浏览器地址栏也能粘)。
FROM github.com/deepseek-ai/deepseek-harness:latest

# ── runtime layout ────────────────────────────────────────────────
# web / headless / cli 三选一。`web` 会自动挂载 webview 端口并打开 UI。
PROFILE web

# ── template name ─────────────────────────────────────────────────
# 这就是 `dshbox run <NAME>` 里那个名字;不加 NAME 则用脚本文件名。
NAME web-with-dsh-ui

# ── extensions (插件 / 技能 / 数据) ───────────────────────────────
ADD plugin github.com/zhu1090093659/dsh-web-ui:latest
```

跑这套 boxfile 的 4 步命令前面 §0 已经给了。

### 7 条 directive

| 指令 | 必填 | 作用 | 例子 |
|---|---|---|---|
| `FROM <base>` | 是 | 指定 base template | `FROM github.com/deepseek-ai/deepseek-harness:latest` |
| `PROFILE <name>` | 是 | runtime 布局,决定挂载路径和启停端口 | `PROFILE web` |
| `NAME <name>` | 否 | 给 built template 起名字 | `NAME web-with-dsh-ui` |
| `ADD <kind> <src> [@<dest>]` | 否(可重复) | 加 插件 / 技能 / 数据 | `ADD plugin github.com/zhu1090093659/dsh-web-ui:latest` |
| `VERSION <ver>` | 否 | template 自己的版本号(语义化字符串) | `VERSION 1.0.0` |
| `LABEL key=value` | 否(可重复) | 元信息,注入 manifest.labels | `LABEL maintainer=alice@example.com` |
| `DEF <name> @<path>` | 否(可重复) | 路径别名(`@profile` = DSH profile 根) | `DEF skill @profile/skills` |

**`DEF` 三个内置默认值**(没写 DEF 也能用):

| name | path | 含义 |
|---|---|---|
| `plugin` | `@profile/profiles/<profile>/node_modules` | 插件安装目录 |
| `skill`  | `@profile/skills` | 技能安装目录(= `$DSH_HOME/skills`) |
| `data`   | (无默认值,必须显式 `@<dest>`) | 数据载荷 |

### ADD 完整语法

```
ADD <plugin|skill|data> <source> [@<destination>]
```

`<source>` 接受下面所有形态——这一节是 **DSH Box 完整的 spec 集**,对应 pnpm `add` 接受的子集(去掉 workspace 自引用和 runtime specifier)。

**简单形态(直接粘浏览器地址栏):**

| shape | 例子 |
|---|---|
| **GitHub short-form(最推荐)** | `ADD plugin github.com/zhu1090093659/dsh-web-ui:latest` |
| GitHub `@` 备用 | `ADD plugin github.com/zhu1090093659/dsh-web-ui@main` |
| local relative | `ADD plugin ./plugins/my-plugin` |
| local absolute | `ADD plugin /home/me/code/my-plugin` |
| local tarball | `ADD plugin file:///home/me/backups/foo.tar.gz` |
| remote tarball | `ADD plugin https://example.com/foo.tar.gz` |
| bare name(仓库已有) | `ADD plugin my-plugin` 或 `ADD plugin @scope/my-plugin` |
| container path | `ADD data container-xxx@/profile/keys.yaml @profile/apikeys.yaml` |

**带前缀的形态(与 pnpm `add` / DSH 官方一一对应):**

| prefix | 例子 |
|---|---|
| `git:` | `ADD plugin git:github.com/owner/repo:v1.2.3` |
| `github:` | `ADD plugin github:owner/repo#v1.0`(pnpm 风格,用 `#ref`) |
| `gitlab:` / `bitbucket:` | `ADD plugin gitlab:owner/repo` |
| `git+https://...` | `ADD plugin git+https://example.com/team/repo.git` |
| `npm:` | `ADD plugin npm:@scope/name@1.2.3`(走 `dist.tarball` 拉镜像) |
| `npm:` alias | `ADD plugin yarn@npm:yarn@1.22.22` |
| `workspace:*` | `ADD plugin my-pkg@workspace:*` |
| `workspace:^` / `workspace:~` | `ADD plugin my-pkg@workspace:^`(pnpm 9+) |
| `file:./path` | `ADD plugin file:./plugins/my-plugin` |
| `link:../path` | `ADD plugin link:../shared-plugin` |

### `git:` / `npm:` / `github:` 怎么选

| 上游 README 推荐 | boxfile 推荐写法 |
|---|---|
| `npm install xxx` / `pnpm add xxx` | `ADD plugin npm:@scope/name@<version>` |
| `pnpm add github:owner/repo` | `ADD plugin github:owner/repo#v1.0` 或 `git:github.com/owner/repo:v1.2.3` |
| `pnpm add file:./my-plugin` | `ADD plugin file:./plugins/my-plugin` 或 `ADD plugin ./plugins/my-plugin` |
| `pnpm add link:../sibling` | `ADD plugin link:../sibling-plugin` |
| `pnpm add my-pkg@workspace:*` | `ADD plugin my-pkg@workspace:*` |
| 仓库 README 给 `yarn@npm:yarn@1.22.22` 别名 | `ADD plugin yarn@npm:yarn@1.22.22` |
| 仓库和 npm 都发布,npm 落后 | 优先 `git:github.com/...`,锁到 commit/tag |
| 只在 npm 发,仓库是 mirror | 用 `npm:...` |

`git:` 是 dshbox 自己的 prefix,语义跟隐式 GitHub short-form 等价但显式。`github:` / `gitlab:` / `bitbucket:` 是 pnpm / DSH 官方 prefix。`npm:` 后面跟 registry 包名时是 npm registry,跟别名时是 rename alias。

`git:` / `npm:` / `workspace:` / `file:` / `link:` 前缀**只对 `ADD plugin` 有意义**;`ADD data` 不支持前缀形态。

### npm 聚合包 (umbrella bundle)

**有些 npm 包内部是聚合 / umbrella,本身只是空壳,实质是"一个 npm 名字 = N 个 dsh.bundle"。** 第三方维护者(典型如 `@linxin666/dsh-web-ui-all`)把同 monorepo 多个独立 plugin 收成一个 npm release,`cordis.patch.yml` 里每条 `insert` 都引用一个兄弟包。**装一条 ADD 实际在 DSH 容器里展开为 N 个独立 plugin 启动。**

| 形态 | 例子 | 运行时展开 |
|---|---|---|
| 独立 plugin | `npm:@linxin666/dsh-pet` | 1 个 plugin 实例 |
| **聚合包**(典型) | `npm:@linxin666/dsh-web-ui-all` | 14 个实例(汇总包 + 13 个 `@linxin666/dsh-*`) |

DSH Box 在 build 阶段会自动:
1. 把 `link:` 引用改成 `workspace:*`(直接注册到 profile 的 `pnpm-workspace.yaml`)
2. 把 plugin 源加进 `pnpm-workspace.yaml` 的 `packages:`
3. 在 workspace yaml 注入 `dangerouslyAllowAllBuilds: true`(让 native build script 不被 pnpm 11 拦)
4. 重跑 `pnpm install`,让 transitive deps 全部 hoist 到 profile 根

你不需要做额外配置,只要在 boxfile 写一行 `npm:...`,剩下的交给 DSH Box。

> **避坑:** `dsh.profile.bundles` 里只会记载 1 个(那个聚合包名字),但 DSH harness 启动后实际挂载 14 个 plugin——`dsh ps` 之类将来显示插件数时,可能"账面"和"实际"不一致。如果你要"装一个开一个",直接 `npm:@linxin666/dsh-pet` 单独拉,绕开聚合包。

## §3 — DSHBox 的运行时架构(出问题先看这里)

### 单 daemon + 多 thin client

DSH Box 只有一个长进程:**`dshboxd`**。它拥有 task queue、所有运行中的容器 host 进程、plugin 安装、template store。`dshbox` CLI 和桌面 Tauri 都是 thin RPC client,跟 daemon 通过 `127.0.0.1:<port>` 上的 `POST /rpc` 通讯(token 在 JSON body 里)。

```
        ┌────────────────────────┐
        │ dshboxd (single owner) │
        │  - tasks / plugins     │
        │  - container hosts     │
        │  - template store      │
        │  - ~/.dsh-box/server/  │
        └───────────▲────────────┘
                    │ JSON-RPC (token in body)
        ┌───────────┴────────────┐
        │  thin clients         │
        │  - `dsh` CLI          │
        │  - Tauri desktop UI   │
        │  - WebView container  │
        └────────────────────────┘
```

### 单实例保护

`dshboxd` 启动时读 `~/.dsh-box/server/discovery.json` 里的旧 daemon 的 PID/端口,做 `TCP connect_timeout(250ms)`。连得上 → 旧 daemon 还活着,新 daemon 退出。连不上 → 僵尸 discovery,清掉继续 bind。所以两个 `dshboxd` 不会并发跑。

### 容器 host 进程的生命周期

每个容器启动时:

1. `start_dsh_container_inner` 用 `setsid` 让 host 成为 process group leader,spawn `pnpm dsh --profile ...`
2. 写入 `instances/<id>/state/host.json`(字段: `hostPid`, `hostPgid`, `hostPort`, `hostUrl`, `state`, `generation`, ...)
3. 等 readiness probe(URL 200 OK)
4. 启动 `spawn_health_watcher` 后台线程,**每 2 秒 HTTP probe + PID 探活(zombie-aware)**
5. **连续 2 次 unhealthy → 写 `state=crashed`,watcher 退出**(不 auto-restart)
6. 容器 destroy 时调 `terminate_process_group_grouped`:`SIGTERM` 等 5 秒,未退出则 `SIGKILL`(避免僵尸)

容器 host 崩溃后,用户必须跑 `dshbox container restart <id>` 才能恢复。

### Daemon 重启时 reconcile

`dshboxd` 启动时扫所有 `state/host.json`:
- PID `kill -0` 探测返回 ESRCH(包括 zombie 状态)→ 记录丢弃,清掉 host.pid
- EPERM(权限拒绝) → 标记 `state=orphaned`,提示 PID 已被另一个进程复用
- alive → 保留记录,信任 watcher 后续判定

## §4 — 容器里 `dsh` agent 能看到的 skills

启动时,DSH Box 把 `<container>/profile/skills/<name>/SKILL.md` 全部铺进 `$DSH_HOME/skills/`(DSH 子进程启动时 Cordis loader 扫描这个目录)。所以容器里 agent 一进上下文就能看到这些 skill。

**DSH Box 自带的 skill(每个 web/headless/cli 容器都有):**

| name | 说明 |
|---|---|
| `boxfile-guide` | 这份文档的前身——专门讲 boxfile DSL |

**用户用 `ADD skill` 加的 skill:** `boxfile` 写 `ADD skill my-tool from ./my-tool/` 后,build 把 `SKILL.md` 拷进 template;run 时再独立 copy 到 container instance。容器 agent 即时看到。

> 想看当前容器加载了哪些 skill,在 DSH agent 里直接调 `dsh skill ls`(或类似命令,具体看 DSH agent 的 help)。要新增一个,改 boxfile 重 build → run 即可。

## §5 — 常见坑

### Boxfile 错

1. **`FROM` 拼错 host**——必须是 `github.com/<owner>/<repo>[:tag|@ref]`,少一段就当成本地 template 名查不到。可以跑 `dshbox pull template <同一行 FROM 的内容>` 验证 base 拉得到。
2. **`:tag` 还是 `@ref`**——`github.com/owner/repo:v1.0.0`(tag)和 `github.com/owner/repo@main`(branch/commit)都合法;**粘 GitHub 浏览器地址栏通常是 `tree/main/...` 那种,记得只截到 `repo`**。
3. **`NAME` 跟 `--name` 同时写了**——以 boxfile `NAME` 那行为准,CLI 的 `--name` 会覆盖模板名。
4. **不写 `NAME` 也不传 `--name`**——默认用 boxfile 文件名(如 `boxfile.dsh` → `boxfile`),**始终显式 `NAME`**。
5. **`ADD data` 漏 `@<dest>`**——直接报错;data 没有默认路径。
6. **`pull template` 跳过了**——`build` 时 `FROM github.com/...` 去本地 store 找 base,找不到就报 "template not found"。
7. **同一插件写两次**——同名第二次 dedup,不会重复安装。
8. **聚合包 vs 源仓混写**——`npm:@linxin666/dsh-web-ui-all`(已 build 的发行包,无 devDependencies)和 `git:github.com/zhu1090093659/dsh-web-ui`(源仓,带 `src/` + `tsdown`)是**不同 entity**,不能互相替换。

### Plugin 装不进容器

1. **`<dest>` 不要乱写**。`npm:` / `git:` 加自己的 `@<dest>` 会覆盖 `DEF plugin` 默认路径,可能让 harness 找不到插件。
2. **`workspace:*` 需要 profile 有 `pnpm-workspace.yaml`**。DSH Box 创建容器时自动生成(`packages: ['.']`);build 阶段也会给外来 plugin 源加 `packages:` 条目。
3. **native module build script 已被 DSH Box 自动放行**。`ssh2` / `cpu-features` / `cloudflared` 等需要 `node-gyp` 编译的依赖,DSH Box 会在 workspace yaml 注入 `dangerouslyAllowAllBuilds: true`,你不需要手动 `pnpm approve-builds`。
4. **不要混搭 npm 名字和 git 仓库名**。`npm:foo` 拉 npm 发行版(可能不是最新),`git:github.com/...` 拉源仓(可能需要本地 build)。

### 容器状态异常

| 状态 | 含义 | 怎么办 |
|---|---|---|
| `starting` | host 进程已 spawn 但 URL 还没就绪 | 等(通常 < 30s) |
| `ready` | 首次 probe 成功 | 等下一次 probe 升级到 running |
| `running` | 持续被 watcher 探测成功 | 一切正常 |
| `crashed` | host 进程死了 或 连续 2 次 unhealthy | `dsh container restart <id>` 拉起 |
| `stopped` | 用户显式 stop | `dsh container start <id>` 重新启动 |
| `orphaned` | daemon 重启时发现 PID 还活着但 EPERM | 说明 PID 已被其他进程复用;`dsh container rm <id>` 重建容器 |

### 平台差异

- **Windows 安装后 `dsh` 不在 PATH**: 跑 `dshbox setup-path` 后**新开 terminal**。
- **Windows 双击 dshbox.exe**: 现在直接弹 UI(不再只是打 help)。
- **macOS / Linux daemon 后台进程**: `systemctl --user start|stop|restart dshboxd.service`(Linux),macOS 用 `dsh` 菜单或 `dshboxd` 直接启停。

## §6 — 调试技巧

- `dsh rpc <method> [json]` 直发 JSON-RPC。例如 `dsh rpc get_info` 看 daemon 信息,`dsh rpc list_containers '{}'` 列容器。
- `dsh container logs <id>` 实时 tail 容器 host 进程日志。host 启动失败(比如 `pnpm dsh` 报错)都打这里。
- `instances/<id>/state/host.json` —— 容器的"心跳"。`state`、`probeCount`、`unhealthyCount`、`lastSeen` 字段直接告诉你 daemon 看到的真实情况。
- `~/.dsh-box/server/discovery.json` —— daemon 的端口/token/PID。CLI 连接不上时先看这个文件存在不存在。
- `~/.dsh-box/config.json` —— `runtimeDirectory`、`mirror.github`、`mirror.npm`、`npmRegistry` 等。
- `dsh --version` 确认 CLI 和 daemon 来自同一 build batch。Daemon 启动时做 build-stamp 校验,如果不一致会自动重启。
- `tail -f ~/.dsh-box/logs/daemon.log`(或 `dsh logs daemon`)—— daemon 自身的 stdout/stderr,看 start-up / reconcile / RPC error。

完整命令清单:`dsh help` 或 `dsh <command> help`(例如 `dsh run help`)。
"#;

const BOXFILE_GUIDE_SKILL_NAME: &str = "dshbox-guide";

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    is_safe_identifier(version)
}

/// Create a container without any UI side-effects. This is the shareable
/// core that both the daemon RPC and the desktop command use.
pub(crate) fn create_dsh_container_sync(
    name: &str,
    version: &str,
    profile: &str,
) -> Result<DshContainer, String> {
    let name = name.trim().to_owned();
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    if name.is_empty() || name.len() > 80 {
        return Err("container name must contain 1 to 80 characters".to_owned());
    }
    if !is_safe_identifier(profile) {
        return Err("profile must use letters, numbers, dots, dashes, or underscores".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // The completion marker (written after a successful clone) is the
    // installed criterion — `.git` exists from the moment a clone starts,
    // so keying on it would let containers build against a half-downloaded
    // harness.
    if !dsh_version_directory(&root, version)
        .join(".dshbox-runtime.json")
        .is_file()
    {
        return Err(format!("DSH version is not installed: {version}"));
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let id = format!("container-{timestamp}");
    let directory = std::path::PathBuf::from(&root).join("instances").join(&id);
    for name in ["profile", "workspace", "logs", "state"] {
        fs::create_dir_all(directory.join(name))
            .map_err(|error| format!("cannot create container: {error}"))?;
    }
    create_profile_manifest(&directory, profile)?;
    let metadata = serde_json::json!({
        "id": id,
        "name": name,
        "version": version,
        "profile": profile,
        "source": dsh_version_directory(&root, version),
    });
    fs::write(
        directory.join("container.json"),
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write container metadata: {error}"))?;
    // Drop the built-in boxfile-guide skill into the freshly-created
    // container so first-time users can open the workspace and immediately
    // read how a boxfile is written. The skill is a per-container copy so
    // edits stay local; the source string is bundled with the daemon.
    write_boxfile_guide_skill(&directory)?;
    Ok(DshContainer {
        id,
        name,
        version: version.to_owned(),
        profile: profile.to_owned(),
        template: None,
        directory: directory.to_string_lossy().into_owned(),
        status: "stopped".to_owned(),
    })
}

/// Write the bundled boxfile-guide skill under
/// `<container>/profile/skills/boxfile-guide/SKILL.md`. Idempotent: a
/// pre-existing copy is left untouched so users who edited the file keep
/// their changes.
fn write_boxfile_guide_skill(container_directory: &Path) -> Result<(), String> {
    let destination = container_directory
        .join("profile/skills")
        .join(BOXFILE_GUIDE_SKILL_NAME);
    let skill_md = destination.join("SKILL.md");
    if skill_md.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&destination)
        .map_err(|error| format!("cannot create skill directory: {error}"))?;
    fs::write(&skill_md, BOXFILE_GUIDE_SKILL)
        .map_err(|error| format!("cannot write boxfile-guide skill: {error}"))
}

pub(crate) fn create_profile_manifest(
    container_directory: &Path,
    profile: &str,
) -> Result<(), String> {
    let directory = container_directory.join("profile/profiles").join(profile);
    if directory.exists() {
        return Err(format!("profile already exists: {profile}"));
    }
    fs::create_dir_all(&directory).map_err(|error| format!("cannot create profile: {error}"))?;
    let manifest = serde_json::json!({
        "name": format!("dsh-profile-{profile}"),
        "private": true,
        "dependencies": {},
        "dsh": { "profile": { "bundles": profile_template_bundles(profile) } }
    });
    fs::write(
        directory.join("package.json"),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write profile manifest: {error}"))?;
    write_profile_support_files(&directory)
}

pub(crate) fn profile_template_bundles(profile: &str) -> Vec<&'static str> {
    match profile {
        "web" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        "headless" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        _ => vec!["@deepseek-ai/dsh-base"],
    }
}

pub(crate) fn write_profile_support_files(directory: &Path) -> Result<(), String> {
    let patch = directory.join("cordis.patch.yml");
    if !patch.exists() {
        fs::write(&patch, "# User overrides for this DSH profile.\n[]\n")
            .map_err(|error| format!("cannot write profile patch: {error}"))?;
    }
    let workspace = directory.join("pnpm-workspace.yaml");
    if !workspace.exists() {
        fs::write(
            &workspace,
            "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n",
        )
        .map_err(|error| format!("cannot write profile workspace: {error}"))?;
    }
    Ok(())
}

pub(crate) fn ensure_container_workspace(directory: &Path) -> Result<PathBuf, String> {
    let workspace = directory.join("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("cannot create container workspace: {error}"))?;
    Ok(workspace)
}

/// Render the per-container JSON snapshot Box writes on every container start.
/// The snapshot becomes a `dsh-box:container` PromptContext section (order
/// 130) that the agent receives as a user-role history snapshot.
pub(crate) fn write_dshbox_context_snapshot(
    directory: &Path,
    container: &serde_json::Value,
    profile: &str,
) -> Result<DshContextFiles, String> {
    let workspace = ensure_container_workspace(directory)?;
    let container_name = container["name"].as_str().unwrap_or("DSH Container");
    let container_id = container["id"].as_str().unwrap_or("unknown");
    let version = container["version"].as_str().unwrap_or("unknown");
    let profile_home = directory.join("profile");
    let plugins_root = directory.join("extensions/plugins");
    let skills_root = directory.join("profile/skills");
    let logs_root = directory.join("logs");

    // Read the env-var names Box already wrote into the container's
    // .credentials.yaml via the DSH settings UI; only the names ship.
    let api_key_envs = read_credentials_env_names(&profile_home);

    let state_dir = directory.join("state");
    fs::create_dir_all(&state_dir)
        .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;
    let snapshot_path = state_dir.join(SNAPSHOT_FILENAME);
    let patch_path = state_dir.join(PATCH_FILENAME);

    let snapshot_body = render_snapshot(
        container_id,
        container_name,
        version,
        profile,
        &workspace,
        &profile_home,
        &plugins_root,
        &skills_root,
        &logs_root,
        &api_key_envs,
    );
    // Atomic write: stage to .tmp then rename so a racing read never sees a
    // half-written snapshot.
    let snapshot_tmp = snapshot_path.with_extension("json.tmp");
    fs::write(&snapshot_tmp, snapshot_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", snapshot_tmp.display()))?;
    fs::rename(&snapshot_tmp, &snapshot_path)
        .map_err(|error| format!("cannot rename {}: {error}", snapshot_tmp.display()))?;

    let patch_body = render_patch_yml(&snapshot_path, DEFAULT_ORDER);
    let patch_tmp = patch_path.with_extension("yml.tmp");
    fs::write(&patch_tmp, patch_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", patch_tmp.display()))?;
    fs::rename(&patch_tmp, &patch_path)
        .map_err(|error| format!("cannot rename {}: {error}", patch_tmp.display()))?;

    Ok(DshContextFiles {
        snapshot_path,
        patch_path,
    })
}

/// Extract the `apiKeyEnv` names that the DSH settings UI wrote into
/// `<DSH_HOME>/.credentials.yaml`. Tolerant of missing or malformed files.
fn read_credentials_env_names(profile_home: &Path) -> Vec<String> {
    let path = profile_home.join(".credentials.yaml");
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return Vec::new(),
    };
    let value: serde_yaml::Value = match serde_yaml::from_str(&body) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    if let Some(map) = value.as_mapping() {
        for (key, _) in map {
            if let Some(key) = key.as_str() {
                names.push(key.to_owned());
            }
        }
    }
    names.sort();
    names
}

/// Repairs Box-created, empty named profiles from builds before profile
/// templates were persisted.
pub(crate) fn repair_known_profile_template(
    container_directory: &Path,
    profile: &str,
) -> Result<(), String> {
    if !matches!(profile, "web" | "headless") {
        return Ok(());
    }
    let directory = container_directory.join("profile/profiles").join(profile);
    let manifest_path = directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("cannot read profile: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile: {error}"))?;
    let empty = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        manifest["dsh"]["profile"]["bundles"] =
            serde_json::json!(profile_template_bundles(profile));
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("cannot repair profile: {error}"))?;
    }
    write_profile_support_files(&directory)
}

/// Ensures every non-bundled DSH plugin selected by a profile has its
/// declared runtime entry, preparing TypeScript sources before the DSH
/// loader attempts to import them.
pub(crate) fn preflight_profile_plugins(
    container_directory: &Path,
    profile: &str,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    let profile_directory = container_directory.join("profile/profiles").join(profile);
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(profile_directory.join("package.json"))
            .map_err(|error| format!("cannot read profile manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile manifest: {error}"))?;
    let bundles = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or("profile manifest has no dsh.profile.bundles")?;
    for bundle in bundles.iter().filter_map(serde_json::Value::as_str) {
        if bundle.starts_with("@deepseek-ai/") {
            continue;
        }
        let plugin_directory = profile_directory.join("node_modules").join(bundle);
        let plugin_manifest_path = plugin_directory.join("package.json");
        if !plugin_manifest_path.is_file() {
            return Err(format!(
                "profile plugin {bundle} is not installed; re-add it from Container details"
            ));
        }
        let plugin_manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&plugin_manifest_path)
                .map_err(|error| format!("cannot read plugin {bundle} manifest: {error}"))?,
        )
        .map_err(|error| format!("cannot parse plugin {bundle} manifest: {error}"))?;
        let Some(entry) = plugin_runtime_entry(&plugin_manifest) else {
            continue;
        };
        // Repository plugins expose `node_modules/` as a link. Resolve its
        // parent before invoking pnpm: running in the container-side hybrid
        // directory makes pnpm try to remove that link interactively.
        let source_directory = plugin_source_directory(&plugin_directory);
        let entry_path = source_directory.join(&entry);
        let rebuild = plugin_source_is_newer_than_entry(&source_directory, &entry_path)?;
        if entry_path.is_file() && !rebuild {
            continue;
        }
        if let Some(task) = task {
            task.update(format!("Preparing plugin {bundle}"), 32);
            let reason = if rebuild {
                "is older than its source"
            } else {
                "is missing"
            };
            task.log(&format!(
                "plugin {bundle} entry {entry} {reason}; installing dependencies and building its source"
            ));
            prepare_plugin_source(&source_directory, bundle, &entry, rebuild, task)?;
        } else {
            return Err(format!(
                "plugin {bundle} has no built entry {entry}; start it from DSH Box so it can be prepared"
            ));
        }
    }
    Ok(())
}

fn plugin_source_directory(plugin_directory: &Path) -> PathBuf {
    fs::canonicalize(plugin_directory.join("node_modules"))
        .ok()
        .and_then(|modules| modules.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| plugin_directory.to_path_buf())
}

pub(crate) fn plugin_runtime_entry(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("main")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .pointer("/exports/./default")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

pub(crate) fn prepare_plugin_source(
    directory: &Path,
    name: &str,
    entry: &str,
    rebuild: bool,
    task: &TaskContext,
) -> Result<(), String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.task_id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let frozen = if directory.join("pnpm-lock.yaml").is_file() {
        "--frozen-lockfile"
    } else {
        "--no-frozen-lockfile"
    };
    let mut install = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            directory.to_string_lossy().as_ref(),
            "install",
            "--force",
            frozen,
        ])
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .spawn()
        .map_err(|error| format!("cannot install dependencies for plugin {name}: {error}"))?;
    let status = wait_for_process(&mut install, Some(task), "installing plugin dependencies")?;
    if !status.success() {
        return Err(format!(
            "plugin {name} dependency installation exited with {status}"
        ));
    }
    if !rebuild && directory.join(entry).is_file() {
        return Ok(());
    }
    if let Some(script) = plugin_build_script(directory)? {
        task.update(format!("Building plugin {name}"), 38);
        let mut build = command_for_toolchain(&pnpm)
            .args(["--dir", directory.to_string_lossy().as_ref(), "run", script])
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|error| format!("cannot build plugin {name}: {error}"))?;
        let status = wait_for_process(&mut build, Some(task), "building plugin")?;
        if !status.success() {
            return Err(format!("plugin {name} build exited with {status}"));
        }
    }
    if directory.join(entry).is_file() {
        Ok(())
    } else {
        Err(format!(
            "plugin {name} build completed but did not create its declared entry {entry}"
        ))
    }
}

/// Returns true when an already-built entry predates a source file.  Linked
/// source directories are intentionally followed: repository-backed plugins
/// expose `src/` and `lib/` through links inside each container profile.
fn plugin_source_is_newer_than_entry(directory: &Path, entry: &Path) -> Result<bool, String> {
    let Ok(entry_modified) = fs::metadata(entry).and_then(|metadata| metadata.modified()) else {
        return Ok(false);
    };
    plugin_source_tree_is_newer(directory, entry_modified)
}

fn plugin_source_tree_is_newer(
    directory: &Path,
    entry_modified: SystemTime,
) -> Result<bool, String> {
    for item in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let item = item.map_err(|error| error.to_string())?;
        let name = item.file_name();
        if matches!(
            name.to_str(),
            Some(
                ".git" | "node_modules" | "lib" | "dist" | "build" | "out" | ".cache"
                    | "pnpm-lock.yaml" | "pnpm-workspace.yaml" | "package.json"
            )
        ) {
            continue;
        }
        let path = item.path();
        let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            if plugin_source_tree_is_newer(&path, entry_modified)? {
                return Ok(true);
            }
        } else if metadata.is_file()
            && metadata.modified().map_err(|error| error.to_string())? > entry_modified
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Prefer the conventional build script, but accept `prepare` as the build
/// hook. Some DSH plugins (including dsh-better-sidebar) use tsdown solely
/// through `prepare`.
fn plugin_build_script(directory: &Path) -> Result<Option<&'static str>, String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(directory.join("package.json"))
            .map_err(|error| format!("cannot read plugin manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin manifest: {error}"))?;

    let get_script = |name: &str| -> Option<String> {
        manifest
            .pointer(&format!("/scripts/{name}"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };

    // Skip scripts that are known non-builders: they need .git, a network
    // dependency, or are purely hook utilities. husky is the most common
    // offender — `dsh-context` sets `prepare: husky` for git hooks but its
    // actual build lives in `build`.
    let is_buildable = |script: &str| -> bool {
        !script.starts_with("husky")
            && !script.starts_with("lint-staged")
    };

    // Prefer `prepare` (npm lifecycle hook, works from tarballs) when it
    // looks like a real build command; otherwise fall back to `build`.
    if let Some(s) = get_script("prepare") {
        if is_buildable(&s) {
            return Ok(Some("prepare"));
        }
    }
    if let Some(s) = get_script("build") {
        if is_buildable(&s) {
            return Ok(Some("build"));
        }
    }
    Ok(None)
}

