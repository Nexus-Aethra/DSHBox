# DSHBox 镜像构筑 Spec

状态：已实现（daemon + CLI 全链路，e2e 见 scripts/e2e-image-workflow.sh；
UI 镜像列表视图为后续迭代）
背景：早期实现中 `dshbox build` 绕过镜像层直接创建容器，与既定架构
偏离。本 spec 定义并已落地"boxfile → 镜像 → 容器"的正轨。

---

## 1. 核心原则

1. **镜像只是元数据**：一个镜像 = 一份 `list.json`，不存储任何资源实体。
2. **资源按性质分两类处理**（这是本 spec 的关键分类）：
   - **plugin**：稳定的代码资源，自带版本号，已登记在全局 repository。
     构筑镜像时**不操作内容**，只在 list 中记录对 repository 条目的引用。
   - **其余资源**（skill / data / 未来的 session 等）：配置类、数据类或
     内容易变，构筑时**必须做快照**——计算内容 hash，建立
     `name → digest` 映射，内容持久化进全局 data store。
3. **容器创建时才落地**：plugin 引用 → 链接进容器；快照 → 从 data store
   **硬复制**出容器独占副本。
4. 镜像可重复构建、可共享、可审计；两个容器用同一镜像，互不影响。

## 2. 资源分类表

| 分类 | 典型 kind | 特征 | 构筑(build)动作 | 容器创建动作 | 生命周期归属 |
|------|-----------|------|-----------------|--------------|--------------|
| plugin | `ADD plugin` | 稳定 code，有 version，仓库登记 | 仅记录 repository 条目引用 | 链接（symlink），仓库引用计数 +1 | repository 引用计数保护 |
| 其余资源 | `ADD skill` / `ADD data` / 未来 `ADD session` | 配置/数据/易变内容 | hash 快照 → `data/<digest>/` + 映射 | 从 data store 硬复制 | 跟随镜像（image prune 回收） |

> 判定规则：`kind == plugin` → reference 模式；其他所有 kind → snapshot 模式。
> 不做"内容启发式"判断，kind 即策略。

## 3. 存储布局

```
<root>/
  images/
    index.json                  # BTreeMap<name, ImageEntry>，同 template 索引
    <fnv1a64>/                  # image id = fnv1a64(list.json 正文)
      list.json                 # 镜像唯一实体（纯元数据）
  data/
    index.json                  # 既有 data store 索引（name → digest）
    <digest>/                   # 快照内容，内容寻址、自动去重
  repository/                   # 既有：plugin 等共享 code 资源
  instances/                    # 容器
```

`ImageEntry { name, id, from, created_at }`，hash 目录 + index 的架构与
template 存储（box-dsh-versions）完全对齐，复用同名覆盖 → 旧 id GC 的规则。

## 4. list.json 结构

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
  `destination`（容器内相对路径，来自 DEF 解析结果）
- wire 结构体定义在 `box-api` crate（与 template 同例，防三端漂移）

## 5. 构筑流程（`dshbox build <boxfile>` → 产出镜像，不产出容器）

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
3. 生成 `list.json` → 写 `images/<id>/` → 更新 `images/index.json`
4. 同名镜像覆盖：新 id 入索引，旧 id 无引用则 GC hash 目录
5. 输出镜像 id 与 name；**全程不创建容器**

## 6. 容器创建（`dshbox run <image|template>` / UI 创建对话框）

1. 读取镜像 `list.json`
2. 创建容器骨架（harness 版本、profile、boxfile-guide skill 等既有初始化）
3. 按 resources 落地：
   - `reference` → 链接 repository 条目进容器，引用计数 +1
   - `snapshot` → 从 `data/<digest>/` **硬复制**到容器 destination
     （快照此刻起与 store 解耦，容器内修改不回写）
4. `container.json` 记录 `image: <name>`（删除保护用）
5. 启动 DSH host

**template 兼容**：`run <template>` = 隐式 build（模板本质是 boxfile）+
创建 + 启动，保持现有单命令体验。

## 7. 生命周期与 GC

| 对象 | 保护规则 | 回收时机 |
|------|----------|----------|
| repository 条目（plugin） | 引用计数（容器链接数） | `plugin prune` 仅清零引用 |
| data store digest（快照） | 被 ≥1 个镜像 list 引用 | `image prune` 扫描所有 list，删无引用 digest |
| 镜像 | 被容器引用时拒绝删除 | `image rm`：删索引项 + GC hash 目录 |
| 容器 | — | 删除仅移除硬复制副本，不影响 store |

data 快照生命周期**跟随镜像**，不跟随容器——与既定规则一致
（data 不做引用计数，容器删除不动 store）。

## 8. CLI / UI 对齐

CLI：

```
dshbox build [boxfile.dsh] [--name <image>]   # 产出镜像（不再直接出容器）
dshbox image ls | show <name> | rm <name> | prune | export | load
dshbox run <image|template> [--name <container>]
```

UI：

- Template tab 的 Build 产出镜像，Resources 增加镜像列表视图
- Container 创建对话框：base 选择器同时列出 images 与 templates
- 任务 kind：`image-build`（重构后真正只产镜像）、`template-container`
  保持现状用于 template 直通路径

## 9. 兼容与迁移

- 既有 `.dshimage` 导出格式（manifest v6）保留为 export/load 载体；
  本地镜像注册表是新增层，export 时从 list.json + store 组装归档
- 既有容器（旧 build 直通产物）不受影响：`container.json` 的 `template`
  字段继续有效，rebuild 走原路径
- 旧行为过渡：一个版本内 `build --container` 保留直通语义并提示 deprecated

## 10. 实施拆分（建议顺序）

1. `box-image`：list.json 类型 + images 注册表读写（hash + index）
2. daemon `image.rs`：build 重定向为镜像生成（分类处理两条腿）
3. daemon：从镜像创建容器（run/create 路径切换，template 隐式 build）
4. CLI `image` 子命令补全（ls/show/rm/prune）+ help 更新
5. UI：镜像列表 + 创建对话框 base 选择器
6. e2e：build → image ls → run image → 验证 plugin 链接 / skill 硬复制 /
   data 硬复制三种落地形态；image rm 引用保护
