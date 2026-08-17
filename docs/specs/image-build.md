# DSHBox 构筑 Spec：built template（构筑模板）

状态：已实现（daemon + CLI 全链路，e2e 见 scripts/e2e-image-workflow.sh；
UI 模板列表的 built 标记展示为后续迭代）

**术语约定：架构中没有独立的 "image" 概念。** 构筑单元统一是**模板**：
`dshbox build` 产出的是 **built template**（构筑模板，纯元数据），与
pull/import 来的 **script template**（脚本模板）存放在同一个内容寻址
模板仓库、同一个索引里。"image" 一词仅作为 `dshbox image` 的历史兼容
别名存在。

---

## 1. 核心原则

1. **构筑模板只是元数据**：一个 built template = 一份 `list.json`，
   不存储任何资源实体。
2. **资源按性质分两类处理**（本 spec 的关键分类）：
   - **plugin**：稳定的代码资源，自带版本号，已登记在全局 repository。
     构筑时**不操作内容**，只在 list 中记录对 repository 条目的引用。
   - **其余资源**（skill / data / 未来的 session 等）：配置类、数据类或
     内容易变，构筑时**必须做快照**——计算内容 hash，建立
     `name → digest` 映射，内容持久化进全局 data store。
3. **容器创建时才落地**：plugin 引用 → 链接进容器；快照 → 从 data store
   **硬复制**出容器独占副本。
4. 构筑模板可重复构建、可共享、可审计；两个容器用同一模板，互不影响。

## 2. 资源分类表

| 分类 | 典型 kind | 特征 | 构筑(build)动作 | 容器创建动作 | 生命周期归属 |
|------|-----------|------|-----------------|--------------|--------------|
| plugin | `ADD plugin` | 稳定 code，有 version，仓库登记 | 仅记录 repository 条目引用 | 链接（symlink），仓库引用计数 +1 | repository 引用计数保护 |
| 其余资源 | `ADD skill` / `ADD data` / 未来 `ADD session` | 配置/数据/易变内容 | hash 快照 → `data/<digest>/` + 映射 | 从 data store 硬复制 | 跟随构筑模板（template prune 回收） |

> 判定规则：`kind == plugin` → reference 模式；其他所有 kind → snapshot 模式。
> 不做"内容启发式"判断，kind 即策略。

## 3. 存储布局（与脚本模板同一个仓库）

```
<root>/
  templates/
    <fnv1a64>/
      script.dsh              # 脚本模板（pull/import 产物）
      list.json               # 构筑模板（build 产物）——二选一
  state/
    template-index.json       # 唯一索引：name -> TemplateEntry
  data/
    index.json                # 既有 data store 索引（name → digest）
    <digest>/                 # 快照内容，内容寻址、自动去重
  repository/                 # 既有：plugin 等共享 code 资源
  instances/                  # 容器
```

`TemplateEntry { name, id, harness_ref, profile, imported_at, from_ref,
built }`：`built: true` 表示 hash 目录里是 `list.json` 而非
`script.dsh`。模板 id = fnv1a64（list.json 正文），同名重建 → 旧 hash GC。

## 4. list.json 结构（wire 类型在 box-api crate）

```json
{
  "schemaVersion": 7,
  "name": "sidebar-demo",
  "base": { "template": "github.com/deepseek-ai/deepseek-harness:latest" },
  "profile": "web",
  "harnessRef": "latest",
  "labels": {},
  "createdAt": 1786900000,
  "resources": [
    {
      "kind": "plugin", "mode": "reference",
      "name": "dsh-better-sidebar", "version": "0.12.2",
      "entryId": "a1b2c3d4"
    },
    {
      "kind": "skill", "mode": "snapshot",
      "name": "boxfile-guide", "digest": "feedface01234567",
      "destination": "profile/skills/boxfile-guide"
    },
    {
      "kind": "data", "mode": "snapshot",
      "name": "corpus", "digest": "0123feedface4567",
      "destination": "data/corpus"
    }
  ]
}
```

- `mode: reference` 必带 `entryId`（repository 条目 id）+ `version`
- `mode: snapshot` 必带 `digest`（fnv1a64 hex，与 data store 同算法）+
  `destination`（容器内相对路径）
- wire 结构体定义在 `box-api`（`TemplateResourceList` / `TemplateResource`），
  与 template 同例，防三端漂移

## 5. 构筑流程（`dshbox build <boxfile>` → 产出构筑模板，不产出容器）

1. 解析 boxfile，解析 FROM 链（模板继承，深度上限 4）
2. 逐条 ADD 按分类处理：
   - **plugin（reference）**：
     - 比对本地 repository（name/scope/version 匹配）
     - 命中 → 记录 `entryId` 引用
     - 未命中且带源形态（GitHub short form / tarball / 本地路径）→
       拉取进 repository 后记录引用
     - 未命中的 bare name → 报错并提示先导入（无源可拉）
   - **其余资源（snapshot）**：
     - 物化源内容（clone / 下载 / 本地复制）
     - 计算 fnv1a64 digest，写入 `data/<digest>/`（已存在则去重跳过）
     - 记录 `name → digest → destination` 映射
3. 生成 `list.json` → 写 `templates/<id>/` → 更新 `template-index.json`
   （`built: true`）
4. 同名模板覆盖：新 id 入索引，旧 id 无引用则 GC hash 目录
5. 输出模板 id 与 name；**全程不创建容器**

## 6. 容器创建（`dshbox run <template>` / UI 创建对话框）

daemon 按索引中 `built` 标记分流，客户端无需区分：

1. **built 模板**：读取 `list.json` → 建容器骨架（harness 版本、profile）→
   按 resources 落地：`reference` → 链接 repository 条目，引用计数 +1；
   `snapshot` → 从 `data/<digest>/` **硬复制**到容器 destination
   （快照此刻起与 store 解耦，容器内修改不回写）
2. **脚本模板**：原有流程——解析脚本、逐条 ADD 物化
3. `container.json` 记录 `template: <name>`（删除保护统一用该字段）

## 7. 生命周期与 GC

| 对象 | 保护规则 | 回收时机 |
|------|----------|----------|
| repository 条目（plugin） | 引用计数（容器链接数） | `plugin prune` 仅清零引用 |
| data store digest（快照） | 被 ≥1 个构筑模板引用 | `template prune` 扫描所有 list，删无引用 digest（另保护活容器 state/data.json 仍在用的） |
| 构筑模板 | 被容器引用时拒绝删除 | `template rm`：删索引项 + GC hash 目录 |
| 容器 | — | 删除仅移除硬复制副本，不影响 store |

data 快照生命周期**跟随构筑模板**，不跟随容器——与既定规则一致
（data 不做引用计数，容器删除不动 store）。

## 8. CLI / UI 对齐

CLI：

```
dshbox build [boxfile.dsh] [--name <template>]    # 产出构筑模板
dshbox template ls | show | rm | prune            # 统一模板管理（两种形态）
dshbox run <template> [--name <container>]        # built/script 均可
dshbox image <...>                                # 弃用别名，转发到上述命令
```

UI：

- Template tab 的 Build 产出构筑模板（`image-build` 任务完成后刷新模板列表）
- Container 创建对话框：模板选择器天然同时列出两种形态（同一索引）
- 任务 kind：`image-build`（构筑）、`template-container`（创建+启动）

## 9. 兼容与迁移

- 既有 `.dshimage` 导出格式（manifest v6）保留为 export/load 载体
- 既有容器不受影响：`container.json` 的 `template` 字段语义不变
- `dshbox image` 保留为别名（`image ls/show/rm/prune/build` →
  template 对应动作），帮助文案标注弃用

## 10. 实施状态

全部落地：box-api 类型、box-dsh-versions 存储（write_built_template /
read_built_template / referenced_snapshot_digests）、daemon build 与
materialize_built_template、CLI template/build/run/image 别名、前端刷新
接线。e2e：scripts/e2e-image-workflow.sh 覆盖 build → template ls →
run → 三种落地形态 → rm 引用保护 → prune。
