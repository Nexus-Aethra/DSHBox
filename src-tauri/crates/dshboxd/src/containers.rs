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
name: boxfile-guide
description: Use when writing, reviewing, or fixing a DSH Box boxfile (`.dsh`) for a 模板/容器, choosing ADD sources for 插件/技能/数据, or running `dshbox init` / `dshbox build` / `dshbox run` to 构筑 and start a template-based container.
---

# DSH Box Boxfile Guide (编写 boxfile → 构筑 template → 启动容器)

A **boxfile** is a `.dsh` script that describes one DSH Box **容器** in plain text — base 模板, profile, name, plugins, skills, data — and `dshbox build` turns it into a reproducible **template** you can spin up any number of containers from. It mirrors a Dockerfile: `FROM` picks the base template (already pulled into DSH Box's local store), `PROFILE` picks the runtime layout, `NAME` names the built template, and `ADD` lines layer 插件 (`plugin`), 技能 (`skill`), and `data` on top.

> 提示: `dshbox` 与 `dsh`(DSH 自身的 CLI)不是同一个程序。所有容器管理(init / pull / build / run / ps / logs / start / stop)走 **`dshbox`**;`dsh` 是 DSH 容器内部的 agent CLI。

## §1 — 一份完整可跑的 boxfile

复制下面这段到 `boxfile.dsh`,**不动任何字符** 就能跑通"init → build → run"全链路。它做了三件事:从官方 deepseek-harness 起一个 web 容器、起一个叫 `web-with-dsh-ui` 的模板、加一个真实存在的 dsh-web-ui 插件:

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
# 加一个真实仓库插件:来源用 GitHub short-form `host/owner/repo:tag`。
# 这条 ADD 在 build 阶段会 import 进 repository,run 阶段安装到 profile/profiles/web/node_modules。
ADD plugin github.com/zhu1090093659/dsh-web-ui:latest

# ── 数据按需取消注释;data 不会进 repository,原样拷进容器 ──────
# ADD data ./local-prompts/ @profile/prompts
```

跑这套 boxfile 的 4 步命令:

```bash
# 1. 生成(不覆盖已有 boxfile.dsh;想覆盖就加 --force)
dshbox init

# 2. 把 base template 拉进 DSH Box 的本地 template store(一次性,离线可重 build)
dshbox pull template github.com/deepseek-ai/deepseek-harness:latest

# 3. 构筑 built template;--name 可省略(省略则用 NAME 那行的值,或脚本文件名)
dshbox build ./boxfile.dsh --name web-with-dsh-ui

# 4. 从 built template 创建一个容器并启动;同一个 template 可以 run 出多个容器
dshbox run web-with-dsh-ui
```

> 顺序很关键:`pull template` 必须先于 `build`(`FROM github.com/...` 在 build 时需要去本地 store 找 base)。`run` 直接接受 template name;脚本型 template(没有走 `build` 的)也可以直接 `run`,启动时按需 materialise。

## §2 — Directives 速查

boxfile 支持 7 条指令。前 4 条决定**整个 template 长什么样**,后 3 条是**可选元数据或定制**。

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

## §3 — ADD 完整语法

```
ADD <plugin|skill|data> <source> [@<destination>]
```

`<source>` 接受下面所有形态——这一节是 **DSH Box 完整的 spec 集**,对应 pnpm `add` 接受的子集(去掉 workspace 自引用和 runtime specifier,因为我们不替 DSH 管 profile 工作区)。`@<destination>` 是可选覆盖,data 必须写。

### 简单形态(直接粘浏览器地址栏)

| shape | 例子 | 何时用 |
|---|---|---|
| **GitHub short-form(最推荐)** | `ADD plugin github.com/zhu1090093659/dsh-web-ui:latest` | 公开仓库;`:tag` 是 dshbox 官方写法 |
| GitHub short-form 备用 `@` | `ADD plugin github.com/zhu1090093659/dsh-web-ui@main` | 想跟 branch/commit 时用 |
| local relative | `ADD plugin ./plugins/my-plugin` | boxfile 同目录或上级目录 |
| local absolute | `ADD plugin /home/me/code/my-plugin` | 任意绝对路径 |
| local tarball | `ADD plugin file:///home/me/backups/foo.tar.gz` | 已经下好的 .tar.gz |
| remote tarball | `ADD plugin https://example.com/foo.tar.gz` | 任意 https 下载链接 |
| bare name(仓库已有) | `ADD plugin my-plugin` 或 `ADD plugin @scope/my-plugin` | 先 `dshbox plugin import` 再用名字 |
| container path | `ADD data container-xxx@/profile/keys.yaml @profile/apikeys.yaml` | 从其它容器拷文件 |

### 带前缀的形态(与 DSH 官方 / pnpm `add` 完全对齐)

| prefix | 例子 | 何时用 |
|---|---|---|
| `git:` | `ADD plugin git:github.com/owner/repo:v1.2.3` | `git:` 后面必须是 `host/owner/repo[:tag\|@ref]`;等同 GitHub short-form 的显式版本 |
| `github:` | `ADD plugin github:owner/repo#v1.0` | pnpm 风格短格式;`#ref` 而非 `:tag`(两种都接受) |
| `gitlab:` / `bitbucket:` | `ADD plugin gitlab:owner/repo` | 其它 git 托管平台 |
| `git+https://...` | `ADD plugin git+https://example.com/team/repo.git` | 任意 git URL |
| `npm:` | `ADD plugin npm:@scope/name@1.2.3` | npm registry 包;走 `dist.tarball` 拉镜像(不走 `pnpm pack`) |
| `npm:`(重命名 / alias) | `ADD plugin yarn@npm:yarn@1.22.22` | `my-alias@npm:<real-pkg>` 把别的包以别名装上 |
| `workspace:*` | `ADD plugin my-pkg@workspace:*` | 当前 profile 的 `pnpm-workspace.yaml` 里声明的本地包 |
| `workspace:^` / `workspace:~` | `ADD plugin my-pkg@workspace:^` | workspace protocol(pnpm 9+) |
| `file:./path` | `ADD plugin file:./plugins/my-plugin` | 等同本地路径 copy |
| `link:../path` | `ADD plugin link:../shared-plugin` | pnpm link 语义(symlink,不复制) |

### `git:` / `npm:` 前缀的语义差异

`git:` 是 **dshbox 自己**的 prefix(语义跟隐式 GitHub short-form 等价,但**显式**)。`github:` / `gitlab:` / `bitbucket:` 是 **pnpm / DSH 官方**的 prefix(与上游安装命令一一对应,README 文档里直接抄过来就能用)。`npm:` 同时存在两种身份——`npm:@scope/name` 是 npm registry;`npm:` 后面跟别名时是 registry rename alias。

`git:` / `npm:` / `workspace:` / `file:` / `link:` 前缀**只对 `ADD plugin` 有意义**;`ADD data` 不支持前缀形态(会报清晰错误)。

### `@<destination>` 怎么用

plugin / skill 一般**不用写**——`DEF plugin` / `DEF skill` 已经给好默认路径。`ADD data` 必须写目标路径,例如:

```dsh
ADD data ./seed-prompts/        @profile/prompts
ADD data ./models/weights.bin   @profile/models/weights.bin
```

### §3a — 怎么选 prefix(永远先读上游 README)

**永远先看插件上游仓库或 npm 包的 README**——它会写明它推荐哪种安装方式。常见情况:

| 上游 README 推荐 | boxfile 推荐写法 | 理由 |
|---|---|---|
| `npm install xxx` / `pnpm add xxx` | `ADD plugin npm:@scope/name@<version>` | 走 npm registry;transitive deps 自动解析 |
| `pnpm add github:owner/repo` | `ADD plugin github:owner/repo#v1.0` 或 `git:github.com/owner/repo:v1.2.3` | 仓库就是 source-of-truth |
| `pnpm add file:./my-plugin` | `ADD plugin file:./plugins/my-plugin` 或 `ADD plugin ./plugins/my-plugin` | 本地源码 |
| `pnpm add link:../sibling` | `ADD plugin link:../sibling-plugin` | pnpm link 语义(symlink) |
| `pnpm add my-pkg@workspace:*` | `ADD plugin my-pkg@workspace:*` | monorepo workspace 成员 |
| 仓库 README 给 `yarn@npm:yarn@1.22.22` 这种别名 | `ADD plugin yarn@npm:yarn@1.22.22` | 跟上游一字不差 |
| 仓库和 npm 都发布,npm 落后 | 优先 `git:github.com/...`,锁到 commit/tag | npm 版本可能不是最新 |
| 只在 npm 发,仓库是 mirror | 用 `npm:...` | 没办法走 git |

### §3b — npm 聚合包(npm bundle)运行时的展开

**有些 npm 包内部是聚合 / umbrella,本身只是空壳,实质是"一个 npm 名字 = N 个 dsh.bundle"。** 第三方维护者(典型如 `@linxin666/dsh-web-ui-all`)用 `scripts/aggregate.mjs` 把同 monorepo 多个独立 plugin 收成一个 npm release,`cordis.patch.yml` 里每条 `insert` 都引用一个兄弟包。**装一条 ADD 实际在 DSH 容器里展开为 N 个独立 plugin 启动。**

| 形态 | 例子 | 运行时展开 |
|---|---|---|
| 独立 plugin | `npm:@linxin666/dsh-pet` | 1 个 plugin 实例 |
| **聚合包**(典型) | `npm:@linxin666/dsh-web-ui-all` | 14 个实例(汇总包 + 13 个 `@linxin666/dsh-*`) |

这是一个**非官方模式**——DSH 官方没有聚合包,`@deepseek-ai/*` 全是独立 plugin。DSH Box 在 `dsh plugin add` 之后会自动:① 把 `link:` 引用改成 `workspace:*` ② 把 plugin 源加进 `pnpm-workspace.yaml` 的 `packages:` ③ 在 `pnpm-workspace.yaml` 注入 `dangerouslyAllowAllBuilds: true` ④ 重跑 `pnpm install`,**让 transitive deps 全部 hoist 到 profile 根**。你不需要做额外配置,只要在 boxfile 写一行 `npm:...`,剩下的交给 DSH Box。

> **避坑:** `dsh.profile.bundles` 里只会记载 1 个(那个聚合包名字),但 DSH harness 启动后实际挂载 14 个 plugin——`dsh ps` 之类将来显示插件数时,可能"账面"和"实际"不一致。如果你要"装一个开一个",直接 `npm:@linxin666/dsh-pet` 单独拉,绕开聚合包。

### 避免的坑

- **不要假设某个插件支持两种来源**。`@linxin666/dsh-web-ui-all` 在 npm 上是 **已 build** 的发行包(带 `lib/`、没有 devDependencies),必须走 `npm:@linxin666/dsh-web-ui-all`;DSH Box 会跳过 `pnpm run build`(你不需要装 `tsdown`)。如果同样的名字需要从源编辑,则走 `git:github.com/zhu1090093659/dsh-web-ui:latest`(源仓库里有 `src/`、`tsdown` 配齐)。
- **不要省略 version**。`npm:@scope/name` 不带版本每次 build 都拿 latest;production build 必须 `@<exact-version>`。
- **`git:` 后面必须是 short-form**。`git:https://github.com/owner/repo` 不被接受——`host/owner/repo` 就行,scheme 由 builder 补。
- **`github:` 用 `#ref` 而非 `:tag`**——pnpm 风格。我们 GitHub 短形也接受 `@` (`v1.0`) 和 `:` (`v1.0.0`),但 `github:` prefix 后只识别 `#`。
- **冒号 `:` vs at `@` 钉版本**:`git:github.com/owner/repo:v1.2.3`(tag)和 `git:github.com/owner/repo@main`(branch/commit)都合法,含义不一样。
- **`<dest>` 不要乱写**。用 prefix 的 `npm:` / `git:` 加自己的 `@<dest>` 会覆盖 `DEF plugin` 默认路径,可能让 harness 找不到插件——除非你知道自己在干嘛。
- **`workspace:*` 需要 profile 有 `pnpm-workspace.yaml`**。DSH Box 创建容器时**会自动**生成这个文件(`packages: ['.']`);DSH Box 在 build 阶段也会给外来 plugin 源加 `packages:` 条目,供 `workspace:*` 取用。
- **native module build script 已被 DSH Box 自动放行**。`ssh2` / `cpu-features` / `cloudflared` 这类依赖需要 `node-gyp` 编译。DSH Box 会在 `pnpm-workspace.yaml` 里注入 `dangerouslyAllowAllBuilds: true`(pnpm 11 的官方开关),所以你不需要手动 `pnpm approve-builds`。如果想自己控制 build 脚本,手动编辑 `pnpm-workspace.yaml` 把 `dangerouslyAllowAllBuilds` 改成 `onlyBuiltDependencies: [...]` 显式列举即可。
- **npm/registry 包的 transitive deps 已经被自动 hoist**。安装完成后 `profile/profiles/web/node_modules/@scope/...` 下能看到所有依赖;DSH Box 在 `dsh plugin add` 之后会切换为 `workspace:*` 引用 + 重跑 `pnpm install`,把 transitive deps 全部 hoist 到 profile 根——这是处理"npm 聚合包 → 多个 dsh.bundle"场景的必要步骤,DSH Box 已经为你做了。
- **不要混搭 npm 名字和 git 仓库名**。`npm:@linxin666/dsh-web-ui-all` 拉的是 npm 发行版(带 build 后的 `lib/`,无 devDependencies);`git:github.com/zhu1090093659/dsh-web-ui:latest` 拉的是源仓(有 `src/`、`tsdown`,需要本地 build)。两条 ADD 装的是**不同 entity**——前者是"全家桶已发布版",后者是"源仓本身"。要看清楚你写的是 npm 名字还是 GitHub short-form。

## §4 — 什么时候用 boxfile vs 一次性命令

| 需求 | 用什么 |
|---|---|
| 同一个组合每次重建都要一致(团队 / CI / 多容器) | **boxfile + `dshbox build` + `dshbox run <NAME>`** |
| 想给当前这个容器临时多装一个插件 | `dshbox plugin install <container-id> <source> --profile <profile>` |
| 把外部插件首次拉进仓库给后续 ADD 用 | `dshbox plugin import <source>` |
| 只想跑一个不被持久化的容器 | `dshbox run <template>` 直接跑脚本型 template,build 步骤可省 |

**`build` ≠ image**:DSH Box 没有 image registry。`dshbox build` 产出一个**轻量 built template**(插件按名引用 repository、其它资源 snapshot 进 data store),跟 `dshbox pull template` 拉回来的脚本 template 进同一个 store,`run` 都能用。

## §5 — 常见坑

1. **`FROM` 拼错 host**——必须是 `github.com/<owner>/<repo>[:tag|@ref]`,少一段就当成本地 template 名查不到。可以跑 `dshbox pull template <同一行 FROM 的内容>` 验证 base 拉得到。
2. **`:tag` 还是 `@ref`**——`github.com/owner/repo:v1.0.0`(tag)和 `github.com/owner/repo@main`(branch/commit)都合法;**粘 GitHub 浏览器地址栏通常是 `tree/main/...` 那种,记得只截到 `repo`**。
3. **`NAME` 跟 `--name` 同时写了**——以 boxfile `NAME` 那行为准,CLI 的 `--name` 会覆盖模板名(用于"一个 boxfile 出多个不同名 template")。
4. **不写 `NAME` 也不传 `--name`**——默认用 boxfile 文件名(如 `boxfile.dsh` → `boxfile`),但容易撞名,**始终显式 `NAME`**。
5. **`ADD data` 漏 `@<dest>`**——直接报错;data 没有默认路径。
6. **`pull template` 跳过了**——`build` 时 `FROM github.com/...` 去本地 store 找 base,找不到就报 "template not found"。
7. **同一插件写两次**——同名第二次会被 dedup(仓库里只有一个 entry),不会重复安装。
8. **聚合包 vs 源仓混写**——`npm:@linxin666/dsh-web-ui-all` 和 `git:github.com/zhu1090093659/dsh-web-ui` 是不同的安装目标,不能互相替换。要"全家桶"走 npm,要"自己改源"走 git。要么分别 `dsh plugin import` 装在仓库里(用仓库名显式 ADD),要么单挑其中一个来源。

## §6 — CLI quick reference

`dshbox` 在容器里就在 PATH 上;它跟本地 daemon 通信,命令行和桌面 GUI 共享同一份状态。

```bash
# ── Workflow ────────────────────────────────────────────────────
dshbox init                              # 生成 boxfile.dsh 模板(存在则拒绝覆盖,加 --force)
dshbox pull template <owner/repo>[:tag]  # 拉 base 进本地 template store(一次性)
dshbox build [boxfile.dsh] [--name tpl]  # 构筑 built template(无 --name 时用 NAME 行)
dshbox run <template>                    # 从 template 创建并启动容器

# ── 模板管理 ─────────────────────────────────────────────────────
dshbox template ls                       # 列出 script + built 两种 template
dshbox template show <name>              # 看脚本正文或资源清单
dshbox template rm <name>                # 删 template
dshbox template prune                    # GC 无人引用的 snapshot

# ── 扩展 / bundle ────────────────────────────────────────────────
dshbox plugin ls                         # 仓库里的 plugin / skill 列表
dshbox plugin import <source>            # 从 dir / tarball / github/npm 拉进仓库
dshbox bundle ls                         # 已有的扩展整合包

# ── 容器操作 ─────────────────────────────────────────────────────
dshbox ps                                # 列出容器与状态
dshbox container url <id>                # 取运行中容器的 webview URL
dshbox container open <id>               # 在 DSH Box 窗口里打开
dshbox container logs <id>               # tail 容器 host 日志
dshbox container start <id>              # 启动已停止容器
dshbox container stop <id>               # 停止运行中容器
```

完整命令清单:`dshbox help` 或 `dshbox <command> help`(例如 `dshbox run help`)。
"#;

const BOXFILE_GUIDE_SKILL_NAME: &str = "boxfile-guide";

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
    for script in ["build", "prepare"] {
        if manifest
            .pointer(&format!("/scripts/{script}"))
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            return Ok(Some(script));
        }
    }
    Ok(None)
}
