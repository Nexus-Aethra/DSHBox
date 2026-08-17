# DSHBox 插件安装架构重构：pnpm 全权托管

> 状态: 设计稿 / 待实施
> 分支: `feat/resource-management`

---

## 1. 背景与动机

当前插件安装管线分两层,各自维护一套逻辑:

```
第一层: 获取源码  — DSHBox 自维护
  spec 解析 → 6 种 Handler (Registry / Git / LocalPath / LocalTarball / RemoteTarball / Workspace)
  → 下载/解压到 staging → 复制进仓库 → 手动 symlink 到容器 node_modules

第二层: 源码仓库与容器关联  — DSHBox 自维护
  引用计数 → 仓库索引 → prune → container 扫描
```

**问题**:第一层本质是在复刻 pnpm 已经做的事(下载 tarball、解压、装依赖、建 node_modules 链接),自己维护导致:

- `spec.rs` 641 行格式解析逻辑,每个 `git+ssh://` vs `gitlab:` 歧义都要自己判断
- 6 个 Handler 各写一套下载/解压逻辑,`pnpm 11` 的 `pack` 行为变更还要单独适配
- `runtime:` (pnpm 12+ 原生 spec) 从未实现
- pnpm store 在 `~/.local/share` 散落,DSHBox 管不到,卸载不干净

**核心决策**:把"下载 + 装依赖 + node_modules 链接"全部交给 pnpm,DSHBox 只保留"仓库元数据管理"。

---

## 2. 新架构

### 2.1 数据流

```
用户输入 spec 字符串 (如 "github:owner/repo#v1.0" 或 "@scope/pkg@1.2.3")
  │
  ▼
pnpm add <spec> --dir <container_profile_dir>   ← pnpm 全权处理
  │                                              解析 spec → 下载 → 解压
  │                                              装依赖 → 建 node_modules
  ▼
DSHBox 元数据写入
  │
  ├─ extension_records.json  ← 记录哪个容器装了哪个插件
  └─ references.json         ← 引用计数 (控制 prune)
  │
  ▼
容器运行时从 node_modules 加载插件 (路径不变,DSH 零改动)
```

### 2.2 文件结构 (改后)

```
<runtime_dir>/
├── pnpm/
│   └── store/          ← pnpm store,DSHBox 专属缓存
├── profiles/
│   └── <container-id>/
│       └── node_modules/
│           ├── @scope/pkg/    ← pnpm 建的链接,指向 store
│           └── .pnpm/         ← pnpm 硬链接层
├── state/
│   └── extensions/
│       └── <container-id>.json   ← 插件安装记录 (元数据)
├── extensions-repo/
│   ├── plugins/
│   │   └── <entry-id>/
│   │       └── source/           ← 源码快照 (引用计数保护)
│   └── references.json           ← 全局引用计数
```

**pnpm 自己管的**:`store` + `node_modules` 里的硬链接/符号链接层

**DSHBox 自管的**:`extensions-repo` 的引用计数 + 索引 + prune 逻辑

---

## 3. 改动范围

### 3.1 删除的代码

| 文件 | 行数 | 删除原因 |
|---|---|---|
| `box-install-handlers/src/spec.rs` | 641 | 整个 spec 解析器。spec 字符串直接透传给 `pnpm add`,无需自解析 |
| `box-install-handlers/src/handler.rs` | ~433 | 6 个 Handler + 下载/解压/复制辅助函数。全部替换为 `pnpm add <spec> --dir <dir>` |
| `box-extensions/src/transfer.rs` 中 `install_plugin_to_container*` 系列 | ~180 | 手动 symlink/copy 逻辑。pnpm 已建 node_modules 链接,无需手动 |

**总计删除约 1250 行。**

### 3.2 保留的代码

| 文件 | 功能 | 保留原因 |
|---|---|---|
| `box-extensions/src/lib.rs` 仓库索引层 | `repository_root` / `write_repository_index` / `scan_repository` | pnpm store 不管 DSHBox 的插件级元数据 (name/version/digest/source) |
| `box-extensions/src/lib.rs` 引用计数层 | `increment_reference` / `decrement_reference` / `unused_repository_ids` | 容器卸载时清理 pnpm store 需要引用计数 |
| `box-extensions/src/lib.rs` 插件记录层 | `read_extension_records` / `write_extension_record` | 扫描容器已装插件,提供 UI 展示 |
| `box-extensions/src/transfer.rs` `copy_extension_source` / `extract_extension_tarball` | tarball 提取 | `dshimage` 分发格式仍需要 (dshimage archive 内嵌源码) |

**保留约 1300 行元数据管理代码。**

### 3.3 新增代码

#### 3.3.1 `command_for_toolchain()` 加 `PNPM_STORE_DIR`

`dshboxd/src/toolchains.rs` — 让 pnpm 的 store 落在 DSHBox 可控目录:

```rust
if let Ok(config) = read_config() {
    if let Some(runtime_dir) = config.runtime_directory.as_deref() {
        let pnpm_root = PathBuf::from(runtime_dir).join("pnpm");
        let _ = std::fs::create_dir_all(&pnpm_root);
        command.env("PNPM_STORE_DIR", pnpm_root.join("store"));
    }
}
```

**效果**:
- pnpm 缓存跟随 runtime 移动,不依赖用户 HOME
- 卸载 DSHBox 时 `<runtime_dir>/pnpm/store` 一起清掉
- 多容器共享 pnpm store 硬链接,零重复下载

#### 3.3.2 新的安装入口

替换 `handler_for(spec).fetch(task, staging)` → 单条 `pnpm add`:

```rust
pub fn install_plugin_to_container(
    task: &TaskContext,
    container: &DshContainer,
    spec: &str,
) -> Result<RepositoryExtension, String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let profile_dir = container.profile_dir();
    let node_modules_parent = profile_dir.parent().ok_or("invalid profile dir")?;

    // pnpm 全权处理: 解析 spec → 下载 → 装依赖 → 建 node_modules
    task.log(&format!("installing plugin via pnpm: {spec}"));
    let mut install = command_for_toolchain(&pnpm);
    install
        .args([
            "add",
            spec,
            "--dir",
            node_modules_parent.to_string_lossy().as_ref(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = install.output().map_err(|e| format!("pnpm failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pnpm add failed: {stderr}"));
    }

    // DSHBox 只写元数据
    let package_name = extract_package_name(&output.stdout);
    write_extension_record(container, &package_name, spec, now_seconds())?;
    increment_reference(Path::new(&read_config()?.runtime_directory.unwrap()), &package_name)?;

    Ok(scan_repository(profile_dir)?
        .into_iter()
        .find(|e| e.name == package_name)
        .ok_or_else(|| format!("plugin {package_name} not found after install"))?)
}
```

#### 3.3.3 `pnpm pack` 作为 tarball 来源 (dshimage 分发)

`box-image` 的 archive 打包仍需要拿到源码,不能用 `pnpm add`:

```rust
// 替换原 fetch_registry_tarball() 等
fn fetch_plugin_tarball(spec: &str, staging: &Path) -> Result<PathBuf, String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let dest = staging.join("packed.tar.gz");
    let mut cmd = command_for_toolchain(&pnpm);
    cmd.args([
        "pack",
        spec,
        "--pack-destination",
        staging.to_string_lossy().as_ref(),
        "--silent",
    ]);
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(dest)
}
```

---

## 4. 兼容性矩阵

| spec 格式 | 旧实现 | 新实现 (pnpm) |
|---|---|---|
| `@scope/name@1.2.3` | ✅ 自解析 | ✅ pnpm 原生 |
| `github:owner/repo#ref` | ✅ 自解析 | ✅ pnpm 原生 |
| `gitlab:owner/repo#ref` | ✅ 自解析 | ✅ pnpm 原生 |
| `bitbucket:owner/repo#ref` | ✅ 自解析 | ✅ pnpm 原生 |
| `git+https://...` | ✅ 自解析 | ✅ pnpm 原生 |
| `git+ssh://...` | ✅ 自解析 | ✅ pnpm 原生 |
| `npm:alias@real-pkg` | ✅ 自解析 | ✅ pnpm 原生 |
| `workspace:*` / `^` / `~` | ✅ 自解析 | ✅ pnpm 原生 |
| `file:./path` | ✅ 自解析 | ✅ pnpm 原生 |
| `link:./path` | ✅ 自解析 | ✅ pnpm 原生 |
| 远程 tarball URL | ✅ 自解析 | ✅ pnpm 原生 |
| `bun@runtime:1.3.0` | ❌ 未实现 | ✅ pnpm 12+ 原生 |
| `dshbox` 自有的 `git:` 前缀 | ✅ 自解析 | ⚠️ 需去掉前缀后透传 |

**net 新增**: `runtime:` spec 首次支持。`dshbox` 自有的 `git:` 前缀需要在入口处剥掉。

---

## 5. 缓存行为对比

| 维度 | 旧方案 | 新方案 |
|---|---|---|
| pnpm store 位置 | `~/.local/share/pnpm/store` | `<runtime_dir>/pnpm/store` |
| 卸载干净度 | ❌ store 散落,手动清理 | ✅ 删 runtime 即全清 |
| 多容器共享 | ✅ node_modules symlink | ✅ pnpm 硬链接 (更彻底) |
| 源码保留 | ✅ 仓库存源码 | ✅ 仓库存源码 (不变) |
| 跨平台 | ⚠️ pnpm store 路径平台差异 | ✅ DSHBox 统一目录 |
| 迁移安全 | ❌ 换 HOME 缓存全丢 | ✅ 跟随 runtime 迁移 |

---

## 6. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| `pnpm add` 行为变更 (pnpm 版本升级) | 安装流程可能 break | DSHBox 打包固定 pnpm 版本;`box-install-handlers` crate 删除后版本绑定更明确 |
| `pnpm pack` 在有 package.json 的 staging 里行为异常 | dshimage archive 构建失败 | `pack` 在空目录里执行,不带 staging 内的 package.json |
| 旧仓库中的 `dshbox` 自有 `git:` 前缀 spec | 旧 container 数据不兼容 | 迁移脚本:启动时扫描 extension_records,把 `git:` 前缀 strip 掉 |
| 用户手动安装了同名但不同版本的插件 | pnpm store 可能重复下载 | pnpm store 按 digest 去重,同名不同版本天然隔离 |

---

## 7. 实施顺序

| 步骤 | 文件 | 操作 |
|---|---|---|
| 1 | `toolchains.rs` | 加 `PNPM_STORE_DIR` env,确保 pnpm store 在 DSHBox 可控目录 |
| 2 | `dshboxd/src/extensions.rs` | `import_into_repository` 改为 `pnpm add` 驱动 |
| 3 | `box-install-handlers` | 删除整个 crate (spec.rs + handler.rs + profile_scan.rs),Cargo.toml 里移除依赖 |
| 4 | `box-extensions/src/transfer.rs` | 删除 `install_plugin_to_container*` 系列,保留 tarball/copy 工具函数 |
| 5 | `Cargo.toml` | 清理所有 crate 的 `box-install-handlers` 依赖 |
| 6 | 迁移脚本 | 旧 extension_records 里带 `dshbox` 前缀的 spec 做兼容处理 |
| 7 | 测试 | `dshbox plugin add` / `dshbox image build` 冒烟 |

---

## 8. 不做的事

- **不替代 `install_plugin_dependencies()` 的 `pnpm install`**——它现在跑在仓库源码目录里,保证容器 symlink 拿到的 `node_modules` 是完整的。pnpm 版本固定,行为稳定。
- **不动 dshimage archive 格式**——dshimage 内嵌源码是为了离线分发,`pnpm add` 不产生可离线分发的产物。archive 部分继续用 `pnpm pack` 拿 tarball。
- **不改 DSH 运行时加载路径**——容器里 `node_modules/<plugin>` 的位置不变,DSH 那边零改动。