# 2026-11-08 — node 会话协议（双向流 + 加密）与 crate 分层

> 状态：**设计，未实现**（决策已定，待按 §10 分步落地）。
> 前序：`docs/design/2026-11-07-node-persistence-and-placement.md`（数据与调度，B1 施工基准）、
> `docs/design/2026-11-04-node-core.md`（stage A 注册表）、`docs/design/2026-11-04-node-artifact.md`（产物）。
>
> 本文解决两件在 B1-b2.2 之后浮出来的事：
> 1. **node 与 control 的连接形态**：从「node 只出站 + 明文 HTTP」改成「可双向发起的一条加密封装会话」；
>    这**推翻**了 11-07 文档 §12.4「控制面不做网络入站连接」的绝对化表述（见 §6.4 与那里的变更说明）。
> 2. **crate 口径**：`src-tauri/crates/` 目前是 22 个平铺包，`dshboxd` 依赖其中 18 个；10 个包只有一个
>    crate 消费者，其中 9 个的消费者就是它。本文给出分层目录、两处合并（+ 一处归位）、
>    一条 CI 守卫，以及「不再新增小件」的判据。

## 0. 决策摘要（已定，可改但要有理由）

| 问题 | 结论 | 理由 |
|---|---|---|
| 自己写握手，还是 TLS 传输 + PSK 认证 | **自己写握手**（方案 A），原语全部来自 `ring` | 节点侧不引入 TLS 栈；会话密钥要能在帧层轮换；代价是安全边界归我们（§9） |
| 引导要素 | **PSK（长期）** 与 **token 链（限期）** 两种，可并存 | 长期设备用 PSK + 定期轮换；自动化/临时环境用 token |
| token 形态 | **哈希链为默认**（一次性、防重放、可本地续链），签名 TTL 为兼容模式 | 链模式控制面只存链端与最近接受的值，节点离线也能续 |
| 节点身份 | **从第一天就有**：节点 Ed25519 密钥 + 控制面 Ed25519 身份 + 会话证书（指纹随引导材料离线交付、之后每轮比对） | 重连不再依赖引导秘密；证书负责长期身份与变更检测，首连的双向认证由 `controlSig` + 离线指纹提供 |
| 双向都连上时谁活 | **先成功者为准**，后到者收 `already-connected` 并被关闭 | 幂等、可重连；不需要分布式仲裁 |
| 反向连（control → node）默认值 | **默认关闭**，需要显式开关 | node 一旦监听入站就是新攻击面 |
| 密钥与签发状态存哪 | **document store 的 `node-keys` scope，一行一节点** | 已有 SQLite；不引入秘密文件，不进 `config.json` |
| 首连凭什么做双向认证 | `controlSig`（控制面 Ed25519 签 `t2`）+ **引导材料里离线交付的控制面指纹** | token 的环值在线上，不能单独当控制面的认证依据（评审 R2） |

## 1. 目标与明确不做

**目标**：node 在任意一侧有入站能力的环境里都能建立会话；一条连接双向跑帧；握手**双向认证**、
换成临时会话密钥后丢弃引导秘密的会话副本；失效与吊销有明确语义。

**不做**：
- 不发明密码学。只定义**帧格式、用例、有效期**；原语一律用 `ring`（X25519 / HKDF-SHA256 /
  HMAC-SHA256 / ChaCha20-Poly1305 或 AES-256-GCM / OS 随机）。
- 不做打洞/中继。两边都在 NAT 后面时双向都失败，那是 `nodes.relay`（P 阶段）的事，本文不承诺。
- 不做证书链/CA。会话证书自签：控制面指纹**随引导材料离线交付**，节点用它校验 `controlSig`，此后每轮比对；
  只有「运维只给了 secret、没给指纹」的降级用法才退化成 TOFU（§5）。不引入公网 CA。

## 2. 三种引导要素（引导材料的交付总是「secret + 控制面指纹」两件）

| 要素 | 谁生成 | 怎么到节点 | 节点存哪 | 有效期 |
|---|---|---|---|---|
| **PSK** | 控制面（每节点 32B 随机 + nodeId） | 运维手工（文件/QR，**与控制面指纹一起给**） | `<root>/node.key`，0600 | 长期，建议 90 天轮换 |
| **token 链** | 控制面（链端 + 种子） | 运维或自动化下发（**种子 + 控制面指纹**） | `<root>/node.token`，0600 | 整链 `expiresAt`（建议 15 分钟~1 小时），一次性 |
| **节点身份** | 节点首次启动自生成 Ed25519 | — | `<root>/node.id`，0600 | 长期；引导要素只负责**绑定**它 |

绑定关系：第一次握手成功时，控制面把 `nodePubkey` 写进 `node-keys` 行（§8）并反映到注册表；
此后重连走 `node-key` 模式（`nodeSig`），引导要素只在「第一次」或「换钥」时出现。
这样 token 的有效期可以只覆盖绑定，而不影响重连。

## 3. token 链（哈希链，默认）

链是**单向哈希链**，方向必须让「下一个值」不可由「当前值」推出，否则一次窃听就能续完整条链
（评审 R1：我上一版把方向写反了，攻击者看到 `c_k` 就能算 `c_{k+1}` 并自己续链）。

```
控制面取随机种子 s，按正向算到链端：c_{i+1} = H(c_i)，c_n = H^n(s)（n 取 64–256）
控制面把**种子 s** 交给节点（0600），自己只存 c_n 作为 lastAccepted
第 k 次握手（k 从 n-1 递减到 1）：节点报 c_k = H^k(s)
控制面 O(1) 校验：H(received) == lastAccepted；成功后 lastAccepted = received
```

- **一次性**：下一次被接受的必须是上一次的**前像**，所以同一个值只能用一次；重放任何旧值都不满足
  `H(received) == lastAccepted`，直接拒绝。
- **抗窃听续链**：窃听者拿到 `c_k` 只能向**前**算（`c_{k+1}…c_n`，全部都已被用过），算不回 `c_{k-1}`
  —— 那需要 `H^{-1}`。所以拿到线上报文不能接管后续握手。
- **节点侧只需要正向 `H`**：种子 `s` 在节点手里，任意 `c_k = H^k(s)` 都能现算（O(n) 次哈希），
  也可以一次性缓存整条链；不需要反函数，也不需要控制面下发新环。
- **链用尽**：`k` 走到 1 之后必须重新 enroll（或换 PSK）。
- **节点侧必须先持久化再发送**（评审 nit 2）：节点把「已用到第 k 环」在**发出 hello 之前**落盘，任何失败、
  超时或重试一律取**更小**的 `k`（把「已发送」当作「已消费」）。否则 `hello-ack` 丢失后重试同一个 `c_k`，
  `H(c_k) = c_{k+1} ≠ lastAccepted` 会被**永久拒绝**，只能重新 enroll。这一条是 L2 的实现红线。
- **节点侧泄露 = 剩余整条链泄露**：种子在节点文件里，谁能读它就能算完后续所有环（哈希链的固有性质，
  和 S/KEY 一样）。所以节点侧必须 0600、且提供「随时换链」——换链就是控制面重写该行的 `secret/lastAccepted`，
  并把新种子重新下发。
- **有效期**：环是裸哈希，本身不带时间戳（评审 N2 指出原文「每环带 TTL」不成立）。有效期挂在**整条链**上：
  `expiresAt` 由控制面时钟判定，过期必须重新 enroll；另有会话最长生存期（§4）与「同一节点两次握手的
  最小间隔」限流。
- **兼容模式**：签名 TTL token `{nodeId, exp, nonce}` + 控制面 Ed25519 签名，可复用、可乱序，
  但需要签发表与吊销表（同一行加 `issuedAt/expiresAt/revokedAt`）。

## 4. 帧格式与记录层

一条 TCP 连接（不套 TLS 库：加密由 §5 的握手自己建）：

```
帧 = u32 长度(BE，上限 1 MiB) | u8 类型 | payload
类型 1 hello  2 hello-ack  3 request  4 response  5 event  6 rekey  7 close
payload = JSON（字段名 camelCase），加密后作为 AEAD 明文块
```

**记录层的密钥与 nonce**——自研 AEAD 帧协议里唯一必须由我们自己写死的部分（评审 5）：

- **方向分离**：`sessionKey_c2s` 与 `sessionKey_s2c` 是两把密钥（HKDF label 不同），永不互用。
- **nonce 构造**：96 位 = `0x00000000`（4 字节）‖ `seq`（8 字节 BE）。`seq` 每方向独立，握手完成后从 0 起算。
- **不重复**：同一把密钥下 `seq` 绝不复用；收到重复或倒退的 `seq` 视为攻击，立即关闭连接。
- **上限与轮换**：单把密钥最多 `2^32` 帧（远低于回绕）或 5 分钟，先到者强制 `rekey`。`rekey` 帧本身用
  **当前**密钥与当前 `seq` 加密并消耗一个序号；双方各出一个新临时公钥派生新密钥后 `seq` 归零、
  旧密钥立即销毁。
- **版本与套件**：`hello` 里协商 `ver`（本文 = 1）与 `suite`（1 = X25519 + HKDF-SHA256 + ChaCha20-Poly1305）。
  ring 暴露的是 `CHACHA20_POLY1305`（评审 N1：原文写成 HChaCha20 是错的）；AES-256-GCM 可作为同套件变体。
  **未知版本/套件直接关闭，不降级**——与现有「`helperVersion` 不兼容就拒绝」的纪律一致。
- 握手帧（hello/hello-ack）**不加密**，携带 `mac`/`sealer`；握手完成后的所有帧都走会话密钥。
- 读线程 + 写线程各一；写侧一把锁保证帧不交错。

## 5. 握手（引导秘密 → 会话密钥 → 丢弃）

```
t1 = sha256("hello" | ver | suite | nodeId | mode | clientNonce | ephA | 模式字段)

node → control  hello { ver, suite, nodeId, mode, clientNonce, ephA, 模式字段 }
  mode = "psk"      : mac = HMAC( HKDF(psk, "bootstrap"), t1 )
  mode = "token"    : chainValue = c_k, mac = HMAC( HKDF(c_k, "bootstrap"), t1 )   ← k 由节点自己记
  mode = "node-key" : nodePubkey, nodeSig = Ed25519( sk_node, t1 )      ← 重连，不需要引导秘密

t2 = sha256( t1 | serverNonce | ephB )

control → node  hello-ack { serverNonce, ephB, controlSig, sealer }
  controlSig = Ed25519( sk_control, t2 )      ← **所有模式都必带**，见下
  psk 模式  : wrapKey = HKDF( ecdh(ephA,ephB) ‖ HKDF(psk,"bootstrap"), "wrap", t2 )   ← 独立证明（PSK 不在线上）
  token 模式: wrapKey = HKDF( ecdh(ephA,ephB) ‖ HKDF(c_k,"bootstrap"), "wrap", t2 )   ← 仅纵深防御，见下
  node-key  : wrapKey = HKDF( ecdh(ephA,ephB), "wrap", t2 )
  sealer = AEAD( wrapKey, seq=0, aad=t2, plaintext = sessionCert )

两侧  sessionKey_c2s = HKDF( ecdh, "c2s", t2 )；sessionKey_s2c = HKDF( ecdh, "s2c", t2 )
握手结束  临时私钥与 wrapKey 立即销毁；引导秘密的会话副本丢弃
```

- **节点侧认证**：`mac`（psk/token）或 `nodeSig`（node-key）证明节点身份。
- **控制面侧认证**：靠 `controlSig` + 节点手里**离线拿到**的控制面公钥/指纹（引导材料里一起交付：
  运维给的文件或 QR 同时含 secret 与控制面指纹）。这条对三种模式都一样成立，因此首连就是双向的。
  - **psk 模式**：PSK 不在线路上，所以「wrapKey 里混了 PSK」本身就是控制面的**独立证明**（除了 `controlSig`）。
  - **token 模式**：`c_k` 就写在不加密的 hello 里，主动 MITM 能读到，所以这里的绑定**只是纵深防御**，
    不是认证依据（评审 R2 的根因）。认证由 `controlSig` + 离线指纹承担。
- **证书的角色**：`sessionCert` 提供长期身份与变更检测；指纹首连即被离线指纹校验过，
  之后每轮比对，变了就拒绝。
- **token 模式的残余风险（明写）**：环值在线上，主动 MITM 可以「转发」节点的一次握手（bearer 语义），
  但拿不到后续环（§3 的抗窃听续链），重放旧环也会被 `H(received) == lastAccepted` 拒绝。
- **前向保密**：会话密钥来自 X25519 临时密钥，**FS 靠临时私钥销毁**（握手结束与每次 `rekey` 后立即
  drop/zeroize），不是「丢弃引导秘密」。丢弃引导秘密的价值是缩小暴露面（评审 6）。
- **控制面侧不「丢弃」引导秘密**：它必须长期保存 `node-keys.secret` 用于下次校验；能丢的只有会话副本。
- **防裁剪与降级**：`t1` 只含客户端字段；`t2` 把 `serverNonce` 与 `ephB` 一起绑进 AAD 与派生 info，
  替换任何一侧的 nonce 或公钥都会导致解封失败或会话密钥失配（评审 3 指出的漏绑）。
- **轮换**：见 §4 的 `rekey`，不重跑握手，引导秘密永不回流。

## 6. 会话生命周期

### 6.1 建立与仲裁
- 主路径 node → control（出站）；备路径 control → node（node 注册时上报 `listenAddrs`，默认关闭监听）。
- 同一 `(nodeId, epoch)` 只允许一个会话：后到的连接收到 `close{already-connected}`。
- **首连用引导秘密（psk/token），之后用节点私钥（`node-key` 模式）**：这样 token 只需短 TTL，重连不受它限制。
  首次登记时把 `nodePubkey` 写进 `node-keys` 行与注册表（§8），此后 `nodeSig` 就是身份。
- 断线重连：指数退避（上限 30s）；`epoch` 变化（控制面重启）时用 `nodeSig` 重建，不需要新 grant。
- **首连的信任来自离线交付的控制面指纹**（随 PSK/token 一起给），不是 TOFU；会话证书指纹之后每轮比对，
  变更即拒绝并告警（§5）。只有在「运维只给了 secret、没给指纹」的降级用法下才退化成 TOFU，且必须显式记录这个状态。

### 6.2 任务与取消（与 B1/B2 的关系）
- 有下行流之后，`cancel` 从「helper 轮询取消列表」变成**下行帧直推**，可以删掉一整块轮询逻辑。
  这会让 11-07 文档 B2 里「接收取消列表」那一项作废，已在那份文档加日期说明（见 §10）。
- `node_claim_task` 保留为「流上的一个 request」，**不等于**必须靠它才拿到活：控制面也可以 push。
  两种都走同一个方法白名单，语义仍以 11-07 §2.3/§3.5 为准。

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
| 换钥 | 覆盖 `secret` + 重置 `lastAccepted/issuedAt` | 运维替换 `<root>/node.key` | 下一次握手 |
| 过期 | `expiresAt` 到期即拒 | — | 同上 |
| 剪枝 | 与 `tasks` prune 同节奏删掉「过期/吊销超过 N 天」的行 | 删自身旧文件 | — |

## 8. 存储：`node-keys` scope

新 scope = 新行集，不是迁移（`PRAGMA user_version` 仍 v1，与 `node-artifacts` 同构）：

```rust
pub const NODE_KEY_SCOPE: &str = "node-keys";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeKeyRecord {
    pub id: String,                // = nodeId，一节点一行，换钥即覆盖
    pub mode: String,              // "psk" | "token-chain" | "token-signed"
    pub secret: String,            // psk: PSK；token: 留空（控制面不存种子 s，也就无从伪造下一环）
                                   // signed-token: 控制面签名私钥的标识（不存私钥本身）
    pub last_accepted: Option<String>, // 链端 c_n 起步；每次接受后换成收到的值；下一个必须是它的前像
    pub node_pubkey: Option<String>, // 节点 Ed25519 公钥；首次登记写入，是 node-key 重连的身份
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
    pub helper_version: Option<String>,
}
```

控制面自己的长期身份另开一行、单独 scope `control-identity`（一个 store 里的单行）：
`{ id: "default", pubkey, seckey, fingerprint }`。它不跟节点行混在一起，因为「谁是谁」是两件事。

四条纪律：
1. **秘密不出库、不进日志**：`Debug` 手写并 redact `secret`/`seckey`；RPC 对外只回 `{hasKey, mode,
   issuedAt, expiresAt, revoked, fingerprint}`。整行禁止 `tracing` 打印。
2. **库文件本身要 0600**（评审 N3）：`<runtime>/state/dshbox.db` 是秘密的落点，创建/打开后收紧权限；
   否则「不进日志」只是把秘密挪到了同目录可读的文件里。
3. **不进库的三样**：会话密钥（内存/会话内）、节点私钥（节点侧 0600）、token 模式的种子 `s`
   （控制面只留链端与最近接受的值，那不是可再用的秘密原文）。
4. **签发即出库一次**：`node_enroll` 生成后把 PSK/token 明文回给运维一次，此后只存校验依据。

## 9. 安全边界：我们承担什么

自研协议意味着审计责任在我们这边，因此约束写死：
- 只用 `ring` 的原语，不自造分组密码/KDF/构造。
- **负向测试清单**（每条都必须连接失败、且不留半开会话）：
  1. 篡改 `t1`/`t2`（改一个字节）；
  2. 重放已用过的 token 环，或报一个不是 `lastAccepted` 前像的值；
  3. 降级 `ver`/`suite`；
  4. 重连时控制面指纹与记录不符；
  5. 截断帧、超长帧（> 1 MiB）；
  6. 过期链（`expiresAt`）；
  7. `hello-ack` 缺 `controlSig`，或用错密钥签名（三种模式都要测）；
  8. `rekey` 重放／在旧密钥下重复使用同一 `seq`；
  9. 收到重复或倒退的 `seq`。
- **正向测试**：首连用离线指纹校验 `controlSig` 通过 → 下一轮同指纹通过、换指纹拒绝；`node-key` 模式下
  `nodeSig` 通过而 `controlSig` 缺失时拒绝；**`hello-ack` 丢失后节点用更小的 `k` 重试能够成功**
  （这条钉的是「先持久化再发送」，见 §3）。
- 握手状态机是纯函数（输入：本端随机数/临时密钥/对端帧；输出：会话密钥或错误），便于用测试向量钉住。
- 失败一律关闭连接，不「重试到成功」；日志只记 `nodeId/原因类别`，不记秘密。

## 10. 分步落点（链路轨道 L1–L4）

**命名说明**（评审 N8）：仓库里已经在用 `B1-b2.1` / `B1-b2.2` 指任务模型与写路径两片，本文这条轨道改用
**L1–L4**，避免「B2.1」和「B1-b2.2」是两条轴却看起来像同一个序列。

| 步 | 内容 | 验收 |
|---|---|---|
| **L1** | `box-node-link` 帧层：helper 的 register/heartbeat/echo/report 从「每操作一个 HTTP POST」换成一条长连接 + 帧；方向仍只有 node→control，明文，无握手 | 同一套方法语义在新传输上跑通；`NODE_METHODS` 白名单不退化 |
| **L2** | 握手 + 双向会话密钥 + `rekey` + `node-keys` 行 + 吊销；SSE 换成同一连接的 event 帧 | 抓包无明文；断线重连不依赖新 grant；§9 的负向测试全绿 |
| **L3** | 反向发起（node 监听默认关 + control dial）+ 单会话仲裁 | 两个方向各自成会话；同时连只有一个存活 |
| **L4** | 任务走流：claim 与 push 并存、取消改下行帧 | B1 的 claim 语义在流上仍成立；轮询取消列表的代码删除 |

**与 11-07 的 B2 是什么关系**：那份文档的 B2 是**能力轨道**（clone→install→build→report、`node_purge`、
`GET /mirror.bundle`、`dshbox node join`），本文的 L1–L4 是**连接轨道**；两条可以并行，但顺序上 L1/L2 先行，
B2 的「接收取消列表」在 L4 之后作废（已在那份文档的 B2 行加日期说明）。

与既有一次性 enroll grant（`ENROLL_TTL_SECONDS = 300`）的关系：它就是 token 模式的第一环，
不另起一套；绑定完成后由节点私钥（`node-key` 模式）接管重连。

## 11. 已知代价

- **helper 不再零依赖**：加 `ring` 后二进制变大，且每个 target 需要 C 工具链（交叉编 arm/musl 要装 cc）。
  这是「自己写握手、不引 TLS 栈」的另一面，已在 §0 记为决策代价。
- **手工分发是 O(节点数 × 轮换次数)**：长期设备用 PSK + 90 天轮换；自动化用 token 链。
- **token 被盗有窗口**：链模式=一次使用；签名模式=TTL（建议 15 分钟）。

---

# 第二部分：crate 分层与合并

## 12. 现状（2026-11-08 实测）

22 个 crate + `dshbox` 二进制（Tauri 包）。`dshboxd` 21,783 行、依赖 18 个 crate。
**10 个 crate 只有一个 crate 消费者，其中 9 个的消费者就是 `dshboxd`**——例外是 `box-data-scheduler`，
它只被 `box-template-core` 用（评审 B9 纠正了原文「11 个」的口径）：

| crate 消费者数 | crate |
|---|---|
| 16 | `box-foundation` |
| 4 | `box-runtime`、`box-scheduler`、`box-dsh-versions` |
| 3 | `box-api`、`box-containers`、`box-extensions`、`box-toolchains` |
| 2 | `box-server-core` |
| 1 | `box-logger`、`box-store`、`box-state`、`box-template-core`、`box-updates`、`box-browser`、`box-image`、`box-resources`、`box-plugin-graph`（以上 9 个 → `dshboxd`）、`box-data-scheduler`（→ `box-template-core`） |
| 0 | `box-client`（只被 `dshbox` 这个二进制包使用） |

口径（评审 N6）：上表只数 **crate 之间**的依赖；`dshbox`（Tauri 包）不计入，若计入 `box-foundation` 是 17。
行数按换行符重算过（`Measure-Object -Line` 会少算约 8%，不要用它核对这类数字）。

结论：这些包没在买「依赖方向」，只在买「多一个编译单元、多一份测试入口、多一个会腐烂的边界」。

## 13. 判据：一个 crate 值得存在，当且仅当

1. **≥2 个消费者**（真的被复用），或
2. **边界挡住了一条会腐烂的依赖**（例：`box-plugin-graph` 3.9k 行、零 box 依赖、可单独测试；
   `box-updates` 零依赖 + 跨语言契约测试钉住 `latest.json`），或
3. **它是二进制**（`dshboxd`、`box-helper`）。

`box-logger`（125 行、1 个消费者、无边界价值）三条都不满足。

合并之后必须守住的一条（评审 N7）：**`box-state` 永不反依赖 `box-template-core`**。
`box-data-scheduler → box-state` 之所以不成环，唯一原因就是这条；一旦反过来，
`box-template-core → box-state` 与 `box-state → box-template-core` 立刻成环，§18 的守卫要单独把它钉住。

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
- **`box-state` 不依赖 `box-template-core`**（合并 `box-data-scheduler` 之后的成环风险，见 §13）；
- 任何 crate 都不依赖 `dshbox`（Tauri 包只能被 `src-tauri` 自己用）。

这条比目录更重要：目录只让方向可见，守卫才让它不可退化。

## 19. 未决 / 后续优化

- `box-server-core`（636 行，被 `box-client` 与 `dshboxd` 共用）是否也折叠：先确认 `box-client` 用的是
  类型还是行为，再决定是并入 `box-client` 还是并入 `dshboxd`。
- `box-client`（fan-in 0）若要继续减包，可以整体降级成 `dshbox` 包内的模块；前提是没有任何 crate 需要它。
- `Sweep::Everything` 的 daemon 侧收敛成「记录上的 owner 字段」（见 11-07 文档 §3.3 的遗留项）。
- 若将来要换 TLS 传输，`suite` 号已经留好，`node-link` 的帧层不需要改。