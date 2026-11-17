# 2026-11-17 — node-link L1：帧层落地计划

> 状态：**施工中**。本文是 `2026-11-08-node-link-session-and-crate-layering.md` §10
> L1-L4 轨道的**第一步实施计划**；协议本身的设计（帧格式、握手、密钥、吊销）仍在
> 11-08 文档里，本文不重复它，只记录「怎么落地、边界划在哪、凭什么验收」。

## 1. 为什么先做 L1

L1-L4 是一条依赖链，L1 是后三步的共同前置。它有三个适合先做的性质：

1. **不引入密码学责任**。明文帧、无握手，安全边界仍是现有的 `NODE_METHODS` +
   `validate_session` + `bind_request_node`，不自建鉴权。
2. **不动方法语义**。同一批 handler、同一套 JSON，只换传输。这意味着它可以被
   「与 HTTP 路径逐项对比」地验证，而不是靠推理。
3. **范围可切干净**。新增一个 crate、一个新路由、helper 换传输，三块彼此独立，
   适合一个 PR 两个 commit。

## 2. 现状（实测，不是推断）

| 组件 | 位置 | 现状 |
|---|---|---|
| 节点注册表 | `dshboxd/src/nodes.rs` | `NodeManager` + `NodeRecord` + enroll grant 与会话 |
| 方法白名单 | `nodes.rs:90` `NODE_METHODS` | 6 个方法 |
| RPC 分发 | `dshboxd/src/nodes_rpc.rs` | 11 个 handler，签名 `(state, request) -> Result<Value>` |
| HTTP 网关 | `dshboxd/src/main.rs:253` | 单连接单线程；`POST /rpc` 与 `GET /events`(SSE) |
| 身份绑定 | `main.rs:237` `bind_request_node` | 剥掉客户端自报 `__nodeId`，再从会话回填 |
| helper | `box-helper/src/main.rs` | 手写 HTTP over `TcpStream`，依赖仅 `serde_json` |

三条约束决定 L1 的边界：

- **白名单是安全边界**：漏加一个方法是 403 而非静默失败，所以新传输必须复用
  `validate_session`，不能另起一套鉴权。
- **daemon 是单端口 loopback**：动态端口 + discovery 文件。L1 复用同一端口加路由，
  不开新监听（开新端口要改 discovery 并处理防火墙，那是 L3 的事）。
- **helper 必须保持低依赖**：L1 不加 `ring`；加密与握手整体推给 L2。

## 3. 帧层：新 crate `box-node-link`

帧格式沿用 11-08 文档 §4：

```
帧 = u32 长度(BE，上限 1 MiB) | u8 类型 | payload
类型 1 hello  2 hello-ack  3 request  4 response  5 event  6 rekey  7 close
payload = JSON（camelCase）
```

**L1 只用 3 与 4**，其余五种类型在本阶段是「已定义未使用」——编解码层认识它们
（否则 L2 接入时要改帧格式），但没有生产路径会产生或消费它们。

### 为什么值得一个新 crate

对照 11-08 文档 §13 的判据（有**两种载体**才配独立存在）：帧编解码是纯函数，
daemon 与 helper 两侧都要用，且两侧会各自演进（服务端读侧/写侧、客户端读侧/写侧）。
放进 `box-server-core` 会把「process boundary」这个定位稀释掉。

它是 `domain/` 层成员：只用 `box-foundation` 与 serde，不碰 `box-state`，
因此不触犯 `box-state ↛ box-template-core` 与「不得依赖 `dshbox`」两条禁令。

### L1 的范围

**做**：

- `frame.rs`：帧的读写与类型定义。长度上限 1 MiB；超限、截断、类型未知都是错误。
  纯函数 + 单元测试，无 IO 依赖之外的隐藏状态。
- daemon：同一端口新增 `GET /link` 路由，进入帧循环；每个 request 帧走**现有**
  `dispatch` → `is_node_method` → handler，结果写回 response 帧。
- helper：`link()` 取代 `rpc()`，一个后台线程持有连接，心跳/echo/report 复用同一连接。

**不做**：

- 不加密、不握手、不 rekey（L2）；不反向拨号（L3）；不推任务、不删 SSE（L2/L4）。
- 不改 `NODE_METHODS` 的语义，不新增或删除任何方法。
- 不删 HTTP `/rpc` 路径——L1 是**并行**接入，两条传输同时可用，便于对比与回退。

## 4. 身份与鉴权：沿用，不重造

这是 L1 最重要的一条纪律。帧路径上：

- 会话 token 与 epoch 从**首帧**的 request 里取出，解析成 nodeId；
- 之后**每一帧**都重新校验（`validate_session`），不复用「连接已建立」这个事实；
- 分发前调用 `bind_request_node`，剥掉客户端自报的 `__nodeId` 再从会话回填。

最后一条已有 loopback 网关测试钉住（删掉剥离子就会变红），帧路径要补一条等价测试，
因为「新传输忘记剥」是这一整类漏洞的标准入口。

## 5. 验收标准

### 必须

1. **语义等价**：register → heartbeat → echo → report → artifact_report 五种方法在
   帧路径上全部跑通，且进出 JSON 与 HTTP 路径一致。
2. **白名单不退化**：白名单外的方法走帧必须 403；自报 `__nodeId` 必须被剥掉。
3. **负向四项**：超长帧（> 1 MiB）、截断帧、未知类型帧、连接中途断流——
   每一种都关闭连接，且不留下半开会话（不写 session、不改节点状态）。
4. **helper 行为不变**：stage-A 的心跳循环、SSE 打印、退避逻辑与改动前一致。
5. **全量回归**：`cargo test --workspace` 与 `pnpm layers:check` 全绿。

### 明确不承诺

- 不承诺帧路径比 HTTP 快（没测，也不打算在 L1 测）。
- 不承诺跨网络可用——L1 明文，只用于 loopback 与 WSL gateway。这一点要写进
  AGENTS.md 的坑位表，而不是留给读者从代码里推断。

## 6. 实施顺序（一个 PR，两个 commit）

| commit | 内容 | 独立可审性 |
|---|---|---|
| 1 | `box-node-link` crate：帧类型 + 编解码 + 单元测试 | 纯函数，无 IO，可单独审 |
| 2 | daemon `GET /link` 路由 + helper 换传输 + 网关等价测试 | 依赖 1 |

分开的理由：commit 1 是「协议长什么样」，commit 2 是「协议怎么接进去」。
混在一个 commit 里，评审者无法只审协议本身。

## 7. 已知代价

- **两条传输并存**：L1 之后 helper 与 daemon 同时会说 HTTP 和帧。这是有意的——
  它让对比验证成为可能，也让回退成本为零。删除 HTTP 路径不在 L1 范围。
- **`GET /link` 复用同一端口**：好处是不动 discovery；代价是这条路子的并发模型
  沿用「一连接一线程」，长连接会占住线程。L2 引入事件帧后连接数仍是每节点一条，
  可接受；若将来节点数上百需重新评估。

## 8. 与 L2-L4 的衔接

| 后续 | L1 为它准备了什么 |
|---|---|
| L2 | 帧类型已含 hello/hello-ack/rekey/close；`box-node-link` 的编解码不用改，只需加密钥与 nonce 管理 |
| L3 | 反向拨号只改「谁发起连接」，帧层与路由不变 |
| L4 | 任务帧复用 request/response；取消走 L2 的 event 帧，届时删轮询 |