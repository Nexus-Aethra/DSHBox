# 2026-11-04 — Node / box-helper 架构：搭建路径规划

> 状态：**规划**，不是实现。本文是 `feat/vm-backends` 合并后，按分支逐步搭建的施工顺序。
> 本文件随 `feat/vm-backends` 一起合入，作为后续分支的共同基线；实现代码不在此分支。

## 目标

把「执行面」从 daemon 进程里彻底抽出来，让**宿主本身也是一个 node**：

- 一个 **node** = 一台真实执行环境（host / WSL2 / libkrun / Apple VZ / docker …）。
- 每个 node 建立后的第一件事：**连控制层并注册**，之后靠 SSE 长连接维持在线与领任务。
- **box-helper** 跑在 node 内，承接 clone / install / build / export / copy / 启动宿主等**全部文件与进程操作**。
- 控制层（Box / dshboxd）只做三件事：维护 node 注册表、按 node 调度意图、汇总进度与产物。
- 模板可以声明「运行到哪个 node」，控制层按 `kind/platform/capability` 匹配。

统一之后，`RuntimeKind::Host / Vm` 的分叉消失：所有容器都是「选一个 node，派一份任务」。
后端差异收敛成「如何供给一台 node」，新增 docker 之类只需加一个供给适配器。

## 为什么必须这样（实测依据）

| 现象 | 数字 | 结论 |
|---|---|---|
| drvfs/9p 元数据延迟 | 安静时 stat 3.7 ms、open 4.6 ms（ext4 1.3/1.1 ms） | 跨边界逐文件 I/O 不可行 |
| tree 经 tar 管道 drvfs→ext4 | packages/ 336 MB → 262.8 s（~1.3 MB/s）；整树 108,685 文件 / 2,271 MB >25 min | 拷贝单位必须是整镜像 |
| `wsl --export` / `--import` | 1327 MB tar 26.6 s / 14.5 s | 整机镜像快一个数量级 |
| 宿主构建产物 | marker.platform = 宿主 OS-ARCH，win32 addon | Linux 产物必须在 Linux 内构建 |
| guest → 控制层 | 带 body token 的 `POST /rpc` 200；`GET /events?token=` 收到 snapshot；无 token 401 | 回连通道已存在，无需新协议 |
| 一次 base 准备 | pnpm build 433~471 s | 不能挂在初次导入的同步路径上 |

另：Windows 10 Home 缺 WHPX/Hyper-V，WSL2 共享内核与 netns，只能提供文件系统级隔离；
这在 `docs/design/2026-11-04-container-runtime-seam.md` 已写清，本架构不宣称 VM 硬隔离。

## 数据模型（三个实体，职责不重叠）

```
Node           控制平面持有。一台执行环境。
  id, kind(host|wsl2|libkrun|vz|docker), platform(linux-x64…),
  status(registering|online|offline|draining|failed),
  capabilities[exec,build,image-export,copy,dsh-host],
  control{ url, session_epoch, enrolled_at, last_seen },
  backend_ref             // distro 名 / container id / VZ bundle，仅供给适配器读
  host_node: bool         // 内建、不可删除

NodeArtifact   该 node 上可复用的产物（替代本轮讨论的 GuestImageRecord）
  (node_id, base_id, source_commit, platform)
    → archive, sha256, size_bytes, built_at

Task           意图，不含文件路径操作
  resource_keys 增加 "node:<id>"；helper 只领自己 node 的任务
```

约束：

- **Node 表不存 DSH、不存插件。** DSH 是 `NodeArtifact`（按 base commit 缓存），插件是 profile 级。
  否则升级 DSH = 重装 node，颗粒度又错了。
- 颗粒度沿用现行规则：一个 DSH 版本一个 prepared base；**一个 (node, base) 一份镜像**；
  N 个 sealed 模板共享同一 base；100 个容器只各带自己的 `profile/`（62.5 MB）。
- 镜像只装 profile 无关的部分（Linux addon、`apps/web/dist`、`apps/cli/lib`、Node 运行时）。

## 注册握手（node 建立后的第一件事）

```
1. 供给适配器创建执行环境（wsl --import / libkrun / docker create / host 内建）
2. 一次性注入 enroll 信息（命令行参数或 /etc/dshbox/helper.json）：
   { control_url, enroll_token(一次性、短 TTL), node_id, kind, platform }
3. helper 首次启动 → POST /rpc  method=node_register
     · 校验 enroll_token，换出 session_token（限权：仅 node_heartbeat / node_claim_task / node_report）
     · 回报 platform / capabilities / kernel / helper_version
4. 控制平面写 NodeRecord(status=online)，广播 node_registered
5. helper 订阅 GET /events?token=<session>；断线指数退避重连
6. 30 s 心跳；N 次无心跳 → offline，但保留 NodeRecord 与已有 NodeArtifact
7. daemon 重启导致 token 轮换 → helper 用 enroll_token 重新注册（或由控制层主动 re-enroll）
```

**不要把 discovery.json 里的主 token 下发到 guest**：那是能调全部 RPC 的全局凭证。
enroll_token 只用于一次注册；session_token 走独立权限表。
**供给完成但未注册 = registering**，任何模板都不应被派过去。

control_url 不能写死 `127.0.0.1`：本机 `.wslconfig` 是 `networkingMode=mirrored` 才能通，
NAT 模式要改走 Windows 网关 IP；端口是 `127.0.0.1:0` 每次启动随机。enroll 时由控制层算好再注入。

## 分支拆分（依赖顺序，逐支独立验证）

| 分支 | 主题 | 交付 | 验收 |
|---|---|---|---|
| `feat/node-core` | A：骨架 | `NodeRecord` 数据结构 + 内建 host node + `node_register/list/heartbeat` RPC + helper 最小可执行（注册/心跳/echo/回报日志）+ 限权 session_token | guest helper 注册成功、SSE 在线、断线重连、host 容器行为零回归；`cargo test --workspace` + `pnpm build` + `command_surface` |
| `feat/node-artifact` | B：异步产物 | 模板/Harness 页的「构筑虚拟机版」按钮 → 任务下发到 node → helper 在 guest 内 clone/install/build/export → `NodeArtifact` 回报；`runtime-lock.json` 式 sha256 锁定 | 一个 (node,base) 只编一次；初次导入不增加等待；失败/取消不写脏记录；产物 digest 可复现 |
| `feat/node-scheduling` | C：调度与迁移 | 模板/容器可指定 node 或约束；`container.json` 的 `runtime` 迁移为 `node_id`（旧字段缺省=host node）；启动/拷贝下沉 helper；`RuntimeKind` 收敛 | host 与 VM 走同一条路径；旧容器不改文件可用；docker 供给适配器可作为验证样例 |
| `feat/node-dashboard` | D：面板 | 主页面「面板」：运行环境 / 容器 / 健康 / 任务 / 更新五张卡片，展示 node 在线状态与产物 | 双语 i18n、截图进 `docs/images/`、双语 README |

依赖关系：A → B → C；D 只依赖 A 的读接口，可与 B/C 并行。
每个分支各自走 PR + 一轮独立审核 + squash 合并；**改动横跨 daemon 与桌面时必须同批重建**
（`node scripts/prepare-server.mjs --force` + `cargo build --release -p dshbox --features custom-protocol`）。

### 明确不在 A/B 范围

- 不改 host 容器的启动路径（C 才动），避免一次改两半无法定位回归。
- 不引入 macOS VZ、docker 供给适配器（C 之后按适配器接口单独加）。
- 不改 `box-store` schema：`NodeRecord` 走现有 `DocumentStore`，不需要 `PRAGMA user_version` 迁移。
- 不做「把 DSH 烘进镜像」：镜像 = OS + helper，DSH = `NodeArtifact`。

## 风险与对策

| 风险 | 对策 |
|---|---|
| 把主 token 落进 guest，控制平面被完全接管 | 一次性 enroll_token + 限权 session_token；权限表随 A 一起加 |
| token 轮换后 helper 永久掉线 | 重新 enroll 流程 + `session_epoch`；SSE 断线重连 |
| NAT 模式下 `127.0.0.1` 不通 | enroll 时由控制层探测并注入可达 URL；mirrored/NAT 两种都测 |
| 大镜像 tar 落盘撑爆运行盘（本机 D: 仅 ~8.8 GB 空闲） | 镜像落盘位置可配置；导出前做空间预检，失败给明确错误 |
| 「所有操作下沉」变成大爆炸重构 | 按 A/B/C 竖切，每支只动一条路径，逐支实测汇报 |
| helper 与 daemon 版本漂移 | 注册时回报 helper_version，控制层对不兼容版本拒绝派任务并提示更新 |

## 验证基线（每支都要）

1. `cd src-tauri && cargo test --workspace`（含 `command_surface` 两个守门测试）
2. `pnpm build`（bundled runtime：`D:\dshbox\runtime\win-x64\node\node.exe` + `pnpm.cjs`）
3. 实机：host 容器启动/停止/rebuild 零回归；node 注册 + 心跳 + 断线重连
4. 改 daemon 的分支：同批重建桌面，核对 `get_info.buildStamp` 与进程 PID
5. 用户可见变化：双语 README + `docs/images/` 截图 + `AGENTS.md` 补坑
