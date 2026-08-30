# Storage abstraction — document store & SQLite

> 状态：第一阶段（任务队列试点）已实现。
> 相关代码：`box-foundation/src/collection.rs`（契约 + JSON/Memory 后端）、
> `box-store/`（SQLite 后端 + 首启导入）、`box-scheduler`（首个消费方）。

## 目标

把"一个数据结构一个散 JSON 文件"的持久化模式收敛为**一个通用文档存储层**：
后端可插拔（SQLite / JSON / Memory，未来 Postgres），迁移与事务只实现一次，
后续业务域的迁移成本趋近于零。

## 分层

```
box-foundation::collection           契约层（不依赖任何 SQL）
  ├─ trait DocumentStore             load_scope / put_documents / delete_document
  ├─ JsonDocumentStore               <root>/<scope>.json（id → document 对象）；
  │                                  兼容读取旧"数组带 id 字段"格式（迁移源）
  ├─ MemoryDocumentStore             测试 / 未配置 runtime 的宿主
  ├─ Collection<T>                   强类型视图：upsert(T)/load_all()/remove(id)
  └─ test_kit（feature "test-kit"）  后端契约测试套件，所有后端必须跑同一套

box-store                            SQL 后端（rusqlite，bundled）
  ├─ SqliteDocumentStore             单表 documents(scope, id, value)
  ├─ 迁移                            PRAGMA user_version + 内嵌编号脚本，
  │                                  每脚本单事务，forward-only 无降级
  └─ open_task_collection(paths)     打开 store + 旧 JSON 首启导入

宿主（dshboxd / 桌面 shell）          构造 Collection<TaskRecord> 注入 TaskManager；
                                     业务 crate 自身不知道数据库存在
```

依赖方向：`box-store → box-scheduler → box-foundation`；无环，符合
AGENTS.md 的分层规则（box-store 是基础设施 crate）。

## 关键语义

- **upsert 永不删除**：`put_documents` 只按 id insert-or-replace。daemon 与
  桌面进程共享同一 SQLite 文件（WAL + busy_timeout=5s），整集重写会互相清掉
  对方的行——因此删除只能走 `delete_document`。每行 last-writer-wins，
  严格优于旧 tasks.json 整文件 last-writer-wins。
- **已知限制**：跨进程任务认领（`try_start` 的内存锁与并发上限）仍未协调；
  进程内语义不变。跨进程认领协议留待后续阶段。
- **首启导入**：`LEGACY_SCOPES` 列出待迁移的旧 scope（现为 `["tasks"]`）。
  store 对应 scope 为空则采纳旧文件全部文档，随后旧文件一律改名
  `<scope>.json.pre-sqlite` 留档、永不再读。旧文件解析失败（旧非原子写入
  产生的撕裂文件是预期成因）→ 报错且不删文件：daemon 回退 JSON 后端，
  桌面回退内存后端，错误进日志。
- **schema 演进**：所有域共用 `documents` 表；某域将来需要真正的 SQL 查询
  （如引用计数对账）时，用新 migration 为它建专属表，**不得**解析 `value`
  列的内容查询。
- **迁移框架**：不引入 refinery/sqlx。`MIGRATIONS[n]` 把 `user_version == n`
  升到 `n+1`，单事务执行，崩溃后下次打开续跑。

## 附带修复：上游 DSH 0.1.2+ browser-trust fence 兼容

e2e 回归发现上游 DSH `latest`（0.1.2-alpha.1）新增 browser-trust fence：
所有无 token 请求一律 **401**，token 只在 host 进程启动后打印到 stdout
（`dsh web: <url>?token=…`，随机生成、不落盘）。DSHBox 的就绪探测/健康
监视器原先要求 2xx，会把健康容器误判为 Crashed。兼容方案（新旧 DSH 双路径）：

1. `lifecycle.rs` 就绪等待期间轮询 host.log 解析 `dsh web:` 行
   （`parse_announced_host_url`，只信任 `http://127.0.0.1:`）；
2. 探测语义改为"**任何非 5xx 响应 = 服务器在线**"（2xx/3xx/401 都算活，
   5xx 仍代表代理劫持或应用故障）；裸 URL 返回 4xx 时必须等到认证 URL
   出现才算就绪（web server 先监听、后打印 URL 行的时序窗口）；
3. 认证 URL 持久化到 `host.json` 新字段 `authenticatedUrl`（serde default，
   旧记录兼容），`ManagedHost` 同步携带；
4. `container_url` / `describe_container` 优先返回认证 URL，webview 拿到
   即可完成 token→cookie 交换；旧 DSH（无 fence）两个接口继续返回裸 URL；
5. 健康监视器优先探测记录里的认证 URL。

老版本 DSH（rc.x，无 fence）行为完全不变：解析不到 URL 行就走裸 URL
探测，2xx 即就绪。

## 数据库位置

`<runtime>/state/dshbox.db`（`BoxPaths::store_db()`）。WAL 模式，
`synchronous=NORMAL`。任务日志本体仍是 `logs/tasks/<uuid>.log` 文件（append），
不进库。

## 后续域迁移顺序（建议）

1. `sealed-templates.json`（BTreeMap 索引，最简单）；
2. `repository/index.json` + `references.json`（注意保留
   `reconcile_owner_index` 的"从权威源重建"不变量）；
3. `resource-map.json` + `deletion-queue.json`（见
   `docs/specs/data-scheduler.md` 的发布/删除契约）；
4. `template-index.json` / `bundles.json` / `data/index.json`。

`config.json`（`~/.dsh-box/`，机器本地小配置）**明确不入库**。

## 验证

- 后端契约：`cargo test -p box-store` 跑同一套
  `assert_document_store_contract`（SQLite/JSON/Memory 三后端）。
- 导入：`cargo test -p box-store import_`（导入/跳过/损坏/无文件四用例）。
- 队列行为：`cargo test -p box-scheduler`（8 个既有行为测试改为 JSON 后端，
  断言不变）。
- 端到端：`bash scripts/e2e-buildrun-from-built-template.sh` 全 10 步。
