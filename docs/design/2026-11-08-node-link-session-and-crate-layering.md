# 2026-11-08 — node 会话协议（双向流 + 加密）与 crate 分层

> 状态：**设计，未实现**（决策已定，待按 §10 分步落地）。
> 前序：`docs/design/2026-11-07-node-persistence-and-placement.md`（数据与调度，B1 施工基准）、
> `docs/design/2026-11-04-node-core.md`（stage A 注册表）、`docs/design/2026-11-04-node-artifact.md`（产物）。
>
> 本文解决两件在 B1-b2.2 之后浮出来的事：
> 1. **node 与 control 的连接形态**：从「node 只出站 + 明文 HTTP」改成「可双向发起的一条加密封装会话」；
>    这**推翻**了 11-07 文档 §12.4「控制面不做网络入站连接」的绝对化表述（见 §6.4 与那里的变更说明）。
> 2. **crate 口径**：`src-tauri/crates/` 目前是 22 个平铺包，`dshboxd` 依赖其中 18 个、而 11 个包的
>    唯一消费者就是它。本文给出分层目录、三处合并与一条 CI 守卫，以及「不再新增小件」的判据。

## 0. 决策摘要（已定，可改但要有理由）

| 问题 | 结论 | 理由 |
|---|---|---|
| 自己写握手，还是 TLS 传输 + PSK 认证 | **自己写握手**（方案 A），原语全部来自 `ring` | 节点侧不引入 TLS 栈；会话密钥要能在帧层轮换；代价是安全边界归我们（§9） |
| 引导要素 | **PSK（长期）** 与 **token 链（限期）** 两种，可并存 | 长期设备用 PSK + 定期轮换；自动化/临时环境用 token |
| token 形态 | **哈希链为默认**（一次性、防重放、可本地续链），签名 TTL 为兼容模式 | 链模式控制面只存链根与已用序号，节点离线也能续 |
| 节点身份 | **从第一天就有**：节点 Ed25519 密钥 + 控制面签发并被 pin 的会话证书 | 纯对称秘密只认证一侧；证书让节点也能验控制面 |
| 双向都连上时谁活 | **先成功者为准**，后到者收 `already-connected` 并被关闭 | 幂等、可重连；不需要分布式仲裁 |
| 反向连（control → node）默认值 | **默认关闭**，需要显式开关 | node 一旦监听入站就是新攻击面 |
| 密钥与签发状态存哪 | **document store 的 `node-keys` scope，一行一节点** | 已有 SQLite；不引入秘密文件，不进 `config.json` |

## 1. 目标与明确不做

**目标**：node 在任意一侧有入站能力的环境里都能建立会话；一条连接双向跑帧；握手用引导秘密认证、
换成临时会话密钥后**丢弃**引导秘密；失效与吊销有明确语义。

**不做**：
- 不发明密码学。只定义**帧格式、用例、有效期**；原语一律用 `ring`（X25519 / HKDF-SHA256 /
  HMAC-SHA256 / ChaCha20-Poly1305 或 AES-256-GCM / OS 随机）。
- 不做打洞/中继。两边都在 NAT 后面时双向都失败，那是 `nodes.relay`（P 阶段）的事，本文不承诺。
- 不做证书链/CA。会话证书自签，靠 pin 指纹信任；不引入公网 CA。

## 2. 三种引导要素

| 要素 | 谁生成 | 怎么到节点 | 节点存哪 | 有效期 |
|---|---|---|---|---|
| **PSK** | 控制面（每节点 32B 随机 + nodeId） | 运维手工（文件/QR/粘贴） | `<root>/node.key`，0600 | 长期，建议 90 天轮换 |
| **token 链** | 控制面（链尾 + 链根） | 运维或自动化下发 | `<root>/node.token`，0600 | 每环 15 分钟，链长上限可控 |
| **节点身份** | 节点首次启动自生成 Ed25519 | — | `<root>/node.id`，0600 | 长期；引导要素只负责**绑定**它 |

绑定关系：第一次握手成功时，控制面把 `nodeId ↔ 节点公钥` 写进注册表；此后重连用节点私钥证明身份，
引导要素只在「第一次」或「换钥」时出现。这样 token TTL 可以短到 15 分钟而不影响重连。

## 3. token 链（哈希链，默认）

```
控制面取随机 s；t_n = s；t_i = H(t_{i+1})；只保存链根 t_0 与已用最高序号 used
节点持 t_n，每次握手消费一环：报 t_k，控制面校验 H^k(t_k) == t_0 且 k > used，成功后 used = k
```

- **一次性**：成功用过的环不能再用（`used` 单调递增），因此抓到旧流量也重放不了。
- **可离线续链**：节点本地派生 t_{k-1}，不需要控制面发新环；链用尽或节点丢状态 → 重新 enroll。
- **过期**：每环带一个 TTL（节点与控制的时钟差用 `issuedAt + ttl` 判定，控制面为准），过期环作废。
- **兼容模式**：签名 TTL token `{nodeId, exp, nonce}` + 控制面私钥签名，可复用、可乱序，但需要签发/吊销
  状态表（同一行加 `issued_at/expires_at/revoked_at` 即可）。

## 4. 帧格式

一条 TCP 连接（TLS 由 §5 的握手自己建，不套 TLS 库）：

```
帧 = u32 长度(BE，上限 1 MiB) | u8 类型 | payload
类型 1 hello        2 hello-ack    3 request     4 response
     5 event        6 rekey        7 close        （8.. 保留）
payload = JSON，字段名 camelCase（与 RPC 一致）
```

- 版本与套件在 `hello` 里协商：`ver`（本文件 = 1）、`suite`（1 = X25519+HChaCha20Poly1305+SHA256）。
  **未知版本/套件直接关闭，不降级**——与现有「`helperVersion` 不兼容就拒绝」的纪律一致。
- 握手帧（hello/hello-ack）**不加密**，但带 `mac`/`sealer` 字段；握手完成后的所有帧都走会话密钥。
- 读线程 + 写线程各一；写侧一把锁保证帧不交错。

## 5. 握手（引导秘密 → 会话密钥 → 丢弃）

```
node → control  hello { ver, suite, nodeId, mode, clientNonce, ephA, mac }
                mac = HMAC( HKDF(psk_or_token, "bootstrap"), transcript )
                transcript = sha256(ver|suite|nodeId|clientNonce|ephA)
control → node  hello-ack { serverNonce, ephB, sealer }
                sealer = AEAD( HKDF( ecdh(ephA,ephB), "wrap" ), sessionCert )
两侧             sessionKey = HKDF( ecdh(ephA,ephB), "session", transcript‖serverNonce )
握手结束         引导秘密（PSK/当前 token 环）从内存丢弃；只保留 token 已用序号
```

- **双向认证**：`mac` 证明「我是持有该秘密的节点」；`sealer` 里的 `sessionCert` 证明「对端是真控制面」
  （节点用已 pin 的指纹校验）。少任何一半都只有单向认证。
- **前向保密**：会话密钥来自 X25519 临时密钥，握手后引导秘密即丢弃，事后拿到 PSK 也解不开历史流量。
- **防裁剪**：transcript（含两个 nonce 与两个临时公钥）进 AEAD 的 AAD；偏移或替换任一字段都会解封失败。
- **轮换**：会话内按帧数或时间（建议 5 分钟）发 `rekey` 帧，用当前会话密钥 KDF 出下一把，
  **不重跑握手**，引导秘密永不回流。

## 6. 会话生命周期

### 6.1 建立与仲裁
- 主路径 node → control（出站）；备路径 control → node（node 注册时上报 `listenAddrs`，默认关闭监听）。
- 同一 `(nodeId, epoch)` 只允许一个会话：后到的连接收到 `close{already-connected}`。
- 断线重连：指数退避（上限 30s），握手可重放无需新 grant；`epoch` 变化（控制面重启）时凭节点私钥重建。

### 6.2 任务与取消（与 B1 的关系）
- 有下行流之后，`cancel` 从「helper 轮询取消列表」变成**下行帧直推**，可以删掉一整块轮询逻辑。
- `node_claim_task` 保留为「流上的一个 request」，**不等于**必须靠它才拿到活：控制面也可以 push。
  两种都走同一个方法白名单，语义以 §5 的 B1 文档为准。

### 6.3 鉴权不变
会话绑定 `nodeId`（服务端绑定，沿用 `main.rs::bind_request_node` 剥掉客户端自报身份的纪律），
`NODE_METHODS` 白名单原样适用。变的只是传输，不是权限模型。

### 6.4 对 11-07 文档 §12.4 的变更
11-07 写的是「node 只出站、控制面从不 dial node」。本文把它改成：
**主路径仍是 node 出站；反向连是可选能力，默认关闭，开启后仍由握手认证**。
理由：复杂环境里两侧的可达性不对称，只支持一个方向等于把「谁能连上谁」写死。
同时保留原文档的正确内核：控制面**默认不监听公网**，反向连需要用户显式开启并绑定地址。

## 7. 吊销、轮换、剪枝

| 动作 | 控制面 | 节点 | 生效点 |
|---|---|---|---|
| 吊销 | `node-keys` 行写 `revokedAt`（不删，留审计） | — | 下一次握手拒绝；已建会话发下行 `close{revoked}` |
| 换钥 | 覆盖 `secret` + 重置 `chainIndex/issuedAt` | 运维替换 `<root>/node.key` | 下一次握手 |
| 过期 | `expiresAt` 到期即拒 | — | 同上 |
| 剪枝 | 与 `tasks` prune 同节奏删掉「过期/吊销超过 N 天」的行 | 删自身旧文件 | — |

## 8. 存储：`node-keys` scope

新 scope = 新行集，不是迁移（`PRAGMA user_version` 仍 v1，与 `node-artifacts` 同构）：

```rust
pub const NODE_KEY_SCOPE: &str = "node-keys";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeKeyRecord {
    pub id: String,               // = nodeId，一节点一行，换钥即覆盖
    pub mode: String,             // "psk" | "token-chain" | "token-signed"
    pub secret: String,           // PSK 或链根 t0（base64）；只有库里有，从不外发
    pub chain_index: Option<u64>, // 哈希链已消费到第几环（防重放）
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
    pub helper_version: Option<String>,
}
```

三条纪律：
1. **秘密不出库、不进日志**：`Debug` 手写并 redact `secret`；RPC 对外只回 `{hasKey, mode, issuedAt,
   expiresAt, revoked}`。整行禁止 `tracing` 打印。
2. **不进库的三样**：会话密钥（内存/会话内）、节点私钥（节点侧 0600）、握手后被丢弃的引导秘密原文
   （库里留的是链根与已用序号，那不是秘密原文）。
3. **签发即出库一次**：`node_enroll` 生成后把 PSK/token 明文回给运维一次，此后只存校验依据。

## 9. 安全边界：我们承担什么

自研协议意味着审计责任在我们这边，因此约束写死：
- 只用 `ring` 的原语，不自造分组密码/KDF/构造。
- 每条必须有**负向测试**：篡改 transcript、重放已用 token 环、降级 suite、错误证书指纹、截断帧、
  超长帧、过期环——七种都必须连接失败且不留半开会话。
- 握手状态机是纯函数（输入：本端随机数/临时密钥/对端帧；输出：会话密钥或错误），便于用测试向量钉住。
- 失败一律关闭连接，不「重试到成功」；日志只记 `nodeId/原因类别`，不记秘密。

## 10. 分步落点

| 步 | 内容 | 验收 |
|---|---|---|
| **B2.1** | `box-node-link` 帧层：把 helper 的 register/heartbeat/echo/report 从「每操作一个 HTTP POST」换成一条长连接 + 帧；方向仍只有 node→control，明文，无握手 | 同一套方法语义在新传输上跑通；`NODE_METHODS` 白名单不退化 |
| **B2.2** | 握手 + 会话密钥 + `node-keys` 行 + 吊销；SSE 换成同一连接的 event 帧 | 抓包无明文；断线重连不依赖新 grant；吊销立即生效 |
| **B2.3** | 反向发起（node 监听默认关 + control dial）+ 单会话仲裁 | 两个方向各自成会话；同时连只有一个存活 |
| **B2.4** | 任务走流：claim 与 push 并存、取消改下行帧 | B1 的 claim 语义在流上仍成立；轮询取消列表的代码删除 |

与既有一次性 enroll grant（`ENROLL_TTL_SECONDS = 300`）的关系：它就是 token 模式的第一环，
不另起一套；绑定完成后由节点私钥接管。

## 11. 已知代价

- **helper 不再零依赖**：加 `ring` 后二进制变大，且每个 target 需要 C 工具链（交叉编 arm/musl 要装 cc）。
  这是「自己写握手、不引 TLS 栈」的另一面，已在 §0 记为决策代价。
- **手工分发是 O(节点数 × 轮换次数)**：长期设备用 PSK + 90 天轮换；自动化用 token 链。
- **token 被盗有窗口**：链模式=一次使用；签名模式=TTL（建议 15 分钟）。

---

# 第二部分：crate 分层与合并

## 12. 现状（2026-11-08 实测）

22 个 crate + `dshbox` 二进制。`dshboxd` 21.8k 行、依赖 18 个 crate；**11 个 crate 的唯一消费者是 dshboxd**：

| fan-in | crate |
|---|---|
| 17 | `box-foundation` |
| 4 | `box-runtime`、`box-scheduler`、`box-dsh-versions` |
| 3 | `box-api`、`box-containers`、`box-extensions`、`box-toolchains` |
| 2 | `box-server-core` |
| 1 | `box-logger`、`box-store`、`box-state`、`box-template-core`、`box-updates`、`box-data-scheduler`、`box-browser`、`box-image`、`box-resources`、`box-plugin-graph` |
| 0 | `box-client`（只有桌面二进制用） |

结论：这些包没在买「依赖方向」，只在买「多一个编译单元、多一份测试入口、多一个会腐烂的边界」。

## 13. 判据：一个 crate 值得存在，当且仅当

1. **≥2 个消费者**（真的被复用），或
2. **边界挡住了一条会腐烂的依赖**（例：`box-plugin-graph` 3.9k 行、零 box 依赖、可单独测试；
   `box-updates` 零依赖 + 跨语言契约测试钉住 `latest.json`），或
3. **它是二进制**（`dshboxd`、`box-helper`）。

`box-logger`（125 行、1 个消费者、无边界价值）三条都不满足。

## 14. 目标目录（四层）

```
src-tauri/crates/
  core/                # 无业务语义，谁都能用
    box-foundation     # config/paths/JSON/collection/校验（吸收 box-logger）
    box-runtime        # 进程/exec/git 原语
    box-store          # SQLite DocumentStore + 旧 JSON 导入
    box-scheduler      # 任务队列/锁/状态机
    box-api            # 线路类型（零依赖；domain 与 edge 都要用它，所以必须在 core）
    box-node-link      # 新：帧 + 握手 + 密钥派生（只依赖 ring/serde）
  domain/              # 业务能力，互相成 DAG
    box-containers  box-image  box-resources  box-extensions
    box-plugin-graph  box-toolchains  box-dsh-versions  box-updates
    box-template-core  box-browser
    box-node-ops       # 新：操作层（起停容器/构件/purge）
    box-state          # 聚合读模型（吸收 box-data-scheduler），坐在 domain 顶部
  edge/                # 进程边界：传输、客户端
    box-client         # RPC 客户端适配（fan-in 0，见 §15）
    box-server-core    # daemon 辅助 + 服务安装（client 也用它）
  apps/
    dshboxd/           # 控制面二进制
    box-helper/        # 节点二进制（= node-link + node-ops）
```

`dshbox`（Tauri 壳）留在 `src-tauri/src`，仍是唯一允许依赖 Tauri 的包。

## 15. 合并两处 + 一处归位（行为零变化）

| 动作 | 判据 |
|---|---|
| `box-logger` → `box-foundation` | 125 行、1 消费者；tracing 初始化 + 日志滚动本就属于 foundation 的职责 |
| `box-data-scheduler` → `box-state` | 947 行、1 消费者；资源映射与「待删除队列」是同一个读模型的两半。合并后 `box-template-core → box-state`，不成环（`box-state` 不依赖 `box-template-core`） |
| `box-api` **归位到 `core/`**（不合并） | 零依赖的线路类型，被 `box-dsh-versions`、`box-extensions`、`dshboxd` 使用——**domain 必须能依赖它**，所以它属于 core 而不是 edge |

**为什么不做「`box-api` + `box-client` → `box-ipc`」**：草案最初是这么写的，但 `box-api` 被 domain 层的包依赖，
并进 `edge/` 就会产生 `domain → edge`，直接违反 §18 的守卫。写成目录之后这个坑一眼可见——
这也是要那条守卫的理由之一。`box-client`（377 行、fan-in 0）先留在 `edge/`；如果它长期只被 `dshbox`
（Tauri 包）使用，可以再降级成那个包里的模块，那时再删一个 crate。

22 → 20（两处合并）；加 `box-node-link`、`box-node-ops` = **22**，但每一层都能说出它为什么在这一层。

## 16. 两条禁令

1. **不再新增「小件」**：`node-keys` 做成 `dshboxd` 的模块（与 `node-artifacts` 同形态），不新开 crate。
   唯一值得新增的是 `box-node-link` / `box-node-ops`，因为**各有两种载体**（daemon 进程内 / helper 跨会话）。
2. **新增 crate 必须在本文的判据下写明理由**，否则用模块。

## 17. 迁移两步

1. **先合并**（小、可审）：新建目标文件、改 `use`、删旧包、更新 root `Cargo.toml` 与依赖方；
   验收就是全 workspace 测试 + `command_surface` 仍绿。
2. **再搬层**（纯路径）：`members` 改成 `crates/core/*` 一类通配、21 个 crate 的 `path =` 依赖、
   `scripts/prepare-server.mjs` 里三处硬编码的 `crates/box-helper` 路径、`scripts/e2e-bundled-git.sh`、
   `crates/box-image/tests`、`AGENTS.md` 的 crate 表；文档里的旧路径保留（历史记录），只在新文档注新位置。

## 18. 守卫：`scripts/check-layering.mjs`

读每个 `Cargo.toml` 的 `box-*` 依赖，断言：
- `core/` 不依赖 `domain/`、`edge/`、`apps/`；
- `domain/` 只依赖 `core/`（含 `box-api` 的线路类型）与本层其它包，不依赖 `edge/`、`apps/`；
  `box-state` 是聚合层，允许依赖 `domain/` 内的其它包；
- `edge/` 不依赖 `apps/`；
- `apps/` 之间不互相依赖；
- 任何 crate 都不依赖 `dshbox`（Tauri 包只能被 `src-tauri` 自己用）。

这条比目录更重要：目录只让方向可见，守卫才让它不可退化。

## 19. 未决 / 后续优化

- `box-server-core`（636 行，被 `box-client` 与 `dshboxd` 共用）是否也折叠：先确认 `box-client` 用的是
  类型还是行为，再决定是并入 `box-client` 还是并入 `dshboxd`。
- `box-client`（fan-in 0）若要继续减包，可以整体降级成 `dshbox` 包内的模块；前提是没有任何 crate 需要它。
- `Sweep::Everything` 的 daemon 侧收敛成「记录上的 owner 字段」（见 11-07 文档 §3.3 的遗留项）。
- 若将来要换 TLS 传输，`suite` 号已经留好，`node-link` 的帧层不需要改。