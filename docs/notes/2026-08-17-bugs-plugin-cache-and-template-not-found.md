# 2026-08-17 — 两个 bug：插件缓存未命中 + run 时 template not found

## 复现

```sh
# build：dsh-better-sidebar@0.12.3 已经在 repository 里
$ dshbox build ./boxfile.dsh
[ 15%] Resolving 1/1 (https://github.com/omdsh-dev/DSH-better-sidebar@v0.12.3)
[ 20%] cloning GitHub repository https://github.com/omdsh-dev/DSH-better-sidebar
[100%] Completed
  [1786909805] built template dsh-test (508ccfa65d9ade69) ready with 1 resource(s)

# plugin ls：出现重复条目
$ dshbox plugin ls
ID                                    KIND    NAME                VERSION
img-b108567e-df72-4697-a5c7-9650883bbd2d   plugin  dsh-better-sidebar  0.12.3
img-ba961107-5f7b-4b04-88cd-b982ba934aba   plugin  dsh-better-sidebar  0.12.3

# run：template 找不到
$ dshbox run dsh-test
[ 95%] Failed
  [1786909555] container container-1786909554 created from built template dsh-test with 1 resource(s)
  [1786909555] failed; inspect the error summary
dshbox: template not found: dsh-test
```

容器被建出来了、plugin 也装好了，但 run 任务在最后一步报"template not found"。
另一边 `dsh template ls` 显示 `dsh-test` 存在。

---

## Bug 1 — 插件缓存未命中（duplicate `img-…`）

**期望**：plugin 已经是 hash 存储 (`<root>/repository/plugins/img-<id>/source/`)，
name+version 就是天然 cache key；同一个 `github.com/.../dsh-better-sidebar@0.12.3`
再 build 一次应该直接复用现有 entry，不应该 clone 也不会出现第二条 `img-…` 记录。

**实际**：[`build_image_from_script`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/image.rs#L150-L168)
对 `ParsedSource::Github` 直接调 `fetch_github_extension` → clone → `import_into_repository`
→ 永远生成 `img-<task_id>` 全新 id。`name+version` 命中检查完全没做。

[`import_into_repository` 路径](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/extensions.rs#L125-L169)：

```rust
let entry_id = format!("img-{}", task.task_id);  // 每个 task 一次新 id
let destination = repository_root(...).join(...).join(&entry_id).join("source");
if destination.exists() { return Err(...); }   // 只挡重复 id，不挡重复 name+version
copy_extension_source(source, &destination)?;
let digest = box_extensions::extension_digest(&destination)?;
let mut entries = scan_repository(...);
entries.push(RepositoryExtension { id: entry_id, ... name, version, ... });  // 盲推
write_repository_index(...);
```

对照：[`ParsedSource::BareName`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/image.rs#L151-L158)
会先 `find_repository_entry` 复用现有 entry，说明意图早就清晰了——只有 GitHub 这条路径漏做了。

**修复方向**：在 `import_into_repository` 末尾（或者在 `build_image_from_script` 里
`ParsedSource::Github` 拿到 staging 之后调用 `repository_metadata` 之前）做一次
`name+version` 命中查询：如果已有 entry，删除 staging 克隆目录、复用现有 entry
并把 `img-<id>` 直接返回给构建器；如果没有，才走 `img-<task_id>` 新建路径。
git 仍然 clone 一次（克隆很小，比吊诡的"先猜 name"更稳），但第二次开始产物会去重。

`skill`/`data` 走的是 data store hash 路径（`fnv1a64` 内容寻址），没有这个问题，
不需要改。

**已修复**（commit 紧接此文）：[`import_into_repository`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/extensions.rs#L125-L184)
在 `repository_metadata` 解析出 `name`/`version` 之后立刻调
`find_repository_entry_by_identity` 查索引：`Plugin` 要求 `name`+`version` 同时
匹配（版本与缓存 key 绑定 —— 0.12.2 和 0.12.3 是两条独立 `img-<id>` 行，允许并存），
`Skill` 不分版本。命中则直接返回现有 entry（task log 打 `reusing cached …`），
不写新目录、不写新 index 行。回归测试 `extensions::tests::import_dedup_by_name_and_version`
已加进 [`extensions.rs` 末尾](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/extensions.rs#L594-L713)，
覆盖「同 name+version 共享 id」「不同 version 各自独立」两个分支。

---

## Bug 2 — `dshbox run dsh-test` 报 `template not found: dsh-test`

**期望**：container 建好了、plugin 装好了，下一步启动 DSH host 时不应该再去找
`templates/dsh-test.dsh` 这个文件（built template 本来就没有 `.dsh` 这种文件）。

**实际**：[`start_dsh_container_inner`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/lifecycle.rs#L49-L64)
用 `templates_directory(&root).join(format!("{name}.dsh"))` 直接拼文件路径：
```rust
match value["template"].as_str() {
    Some(name) => {
        let template_path = templates_directory(&root).join(format!("{name}.dsh"));
        if !template_path.is_file() {
            return Err(format!("template not found: {name}"));
        }
    }
    ...
}
```

这套逻辑只对**旧的扁平脚本模板**有效——`pull_template` 当时会把
`<version>.dsh` 写成 legacy alias。但**本轮重构后**：
- built template 的实体是 `templates/<fnv1a64>/list.json`，没有 `.dsh` 文件
- name 仍然是 `dsh-test`（hash 目录只是存储后端，索引里 name 字段就是用户写的）
- `materialize_built_template` 已经通过 `record_container_origin` 写入了
  `container.json: template = "dsh-test"`，但这一步的"来源"是 hash 索引，不是
  扁平文件

[`lookup_template_path`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/image.rs#L672-L686)
是 hash 索引感知的；`materialize_template_container` 早就用上它了，
只有 `start_dsh_container_inner` 这条启动路径漏了。

**修复方向**：把 `templates_directory(&root).join(format!("{name}.dsh"))` 替换成
`lookup_template_path(&root, name)`（需要从 image.rs 提升为 `pub(crate)`）。
`Err("template not found: ...") ` 错误格式也要匹配现有输出。

**已修复**：[`lookup_template_path` 提升为 `pub(crate)`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/image.rs#L672-L686)，
[`start_dsh_container_inner`](file:///home/wpp/homework/DSHBox/src-tauri/crates/dshboxd/src/lifecycle.rs#L51-L66) 改为调用它；
命中失败时仍报 `template not found: <name> (<error>)` 以兼容既有错误格式。
`templates_directory` 同步从 lifecycle.rs 的 import 中删除。

---

## 顺便提一下

- `dsh-test` 这个命名我怀疑是从 `NAME` 字段（`boxfile.dsh` 里写了 `NAME dsh-test`）来的
  ——`NAME` 解析是支持的（[script.rs L201-L204](file:///home/wpp/homework/DSHBox/src-tauri/crates/box-image/src/script.rs#L201-L204)），
  是构筑时默认的模板名（`build --name <x>` > `NAME` > 解析器默认 `"image"`），
  没问题。
- `[1786909555] container container-1786909554 created from built template dsh-test with 1 resource(s)`
  这行重复打了两次，应该是 task 的 progress tick 触发时 task.log 会被记两次
  事件（一次进度更新 + 一次 container 落库），属于小毛病，不在本次修复范围内。
