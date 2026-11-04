# 2026-11-04 — 任务阶段化进度：每个阶段一条进度条

> 状态：**设计**。独立分支 `feat/task-stage-progress`，与 node 系列无关。

## 问题

安装 Harness 时进度条会在 35%→50% 之间停留 **7–8 分钟**（`pnpm run build`
实测 433–471 s，只对应 15 个百分点）。用户在默认视图里看到的是一行不动的
百分比，无法与「进程已经死掉」区分。

根因有三条，缺一条都还是猜：

1. **锚点是人写的整数**：`Cloning 10 / Installing 35 / Building 50 / Validating 60 /
   Publishing 84 / Ready 100`，与实际工作量无关。
2. **没有时间信息**：一个数字不动，读者无法判断是慢还是卡死。
3. **日志藏在按钮后面**：daemon 一直在写 `task.log`，但默认视图看不到，
   而真实原因（pnpm 正在编什么）恰恰在那里。

## 设计

### 阶段是数据，不是字符串

```rust
pub struct TaskStage {
    pub id: String,            // 稳定标识，用于 i18n 与重试计数
    pub label: String,         // 回退显示文本（daemon 语言中立）
    pub state: StageState,     // pending | running | done | skipped | failed
    pub progress: u8,          // 该阶段自己的 0-100
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub duration_hint_secs: u64,   // 预计耗时，用于算阶段内进度
    pub attempts: u32,             // 重试次数（默认 0）
    pub failure_detail: Option<String>, // 失败时停在哪一子步
}
```

`TaskRecord` 新增 `stages: Vec<TaskStage>`，`#[serde(default)]`：老任务是空数组，
UI 遇到空数组退回单条总进度——**旧记录不崩，只是没有分阶段视图**。
`stage` / `progress` 两个旧字段保留且继续维护（总进度由阶段折算），
因为 `command_surface` 的守门测试和其它读点都依赖它们。

### 阶段内进度：观测优先，时间兜底，永不自动满

这条是「真进度」与「装饰性进度条」的唯一分界：

- **里程碑优先**：`node_modules` 出现 → install 置满；`apps/cli/lib/bin.js`
  出现 → build 置满。有观测就不猜。
- **时间兜底**：`min(已用秒数 / duration_hint_secs, 0.9)`，**封顶 90%**。
  只有真正完成才置 100。
- 于是「卡住」的表现是：停在 85% 且**已用时间持续增长**——一眼可辨。
  这正是旧版缺失的信息。

### 失败停在子步，重试可见

- `state=failed` 的那个阶段带 `failure_detail`（哪一步、什么错），UI 标红并展开。
- `attempts` 由 daemon 在重试时累加，UI 显示「重试 2 次」。
- 未执行的后续阶段保持 `pending`，不假装成功也不消失。

### 日志内嵌默认视图

卡片默认显示**最后 3 行**日志（已有一行总进度时这行就是「证据」），
点「查看日志」仍是完整日志。这样不需要点开就能回答「它在干什么」。

### agent 侧同样受益

`task_get` / `task_list` 返回 `stages` 与 `lastLogLines`，使 agent 能说
「正在打包 web，已用 3 分钟」而不是「已提交任务」。

## 阶段表（Harness 安装）

| id | 预算秒 | 依据 |
|---|---|---|
| `clone` | 20 | 实测浅克隆 |
| `install` | 120 | `pnpm install` |
| `build` | 450 | `pnpm run build` 实测 433-471 s |
| `validate` | 15 | `validate_prepared_harness` |
| `publish` | 40 | 发布 prepared base |

合计 645 s，与实测一次 base 准备 9-12.5 min 同量级。
### 为什么不拆 build 的子阶段（一个刻意的取舍）

初稿把 build 拆成 native / host / web 三个子阶段。实现时发现：`pnpm run build`
是**一次子进程调用**，daemon 看不到它内部的真实次序，任何子阶段进度都只能靠
猜时间比例——而那正是本设计要消灭的东西。所以不拆。

想要真子阶段，就得把 build 拆成三条独立命令分别观测产物；那会改变构建语义
（`pnpm run build` 内部有依赖顺序），风险大于收益。留作后续：若
`harness/scripts/build.ts` 将来支持分步执行，子阶段就能从估算变成观测。

### 阶段推进靠后台定时器 + 里程碑

`build` 运行期间，daemon 起一个 5 s 定时器调用 `stage_tick`，把「已用/预算」
折算成进度（**封顶 90%**）。同时检查里程碑：`apps/cli/lib/bin.js` 出现即
提前置满。于是 bar 是活的，卡住时停在 90% 且已用时间继续涨。

## 文件清单

```
crates/box-scheduler/src/lib.rs      TaskStage / StageState / plan_stages / stage_*
crates/dshboxd/src/versions.rs       声明阶段 + tick 线程 + 里程碑
src/shared/types/domain.ts           TaskStage 类型
src/features/tasks/TaskPanel.tsx     纵向阶段条 + 内嵌日志
src/i18n.ts                          阶段名双语
```

## 验收

1. 阶段列表与阶段表一致，正在跑的阶段有独立填充（截图）。
2. 把某阶段拖慢，界面显示 <90% 且已用时间增长。
3. 失败时停在具体阶段并带上 detail，后续阶段保持 pending（单测 + 截图）。
4. 重试累加 `attempts` 并可见（单测）。
5. 老任务无 `stages` 时不崩，退回单条总进度（单测）。
6. `cargo test --workspace` + `pnpm build` 绿；双语 README + 截图。

## 风险

| 风险 | 对策 |
|---|---|
| 加字段破坏持久化记录 | `#[serde(default)]`，老记录空数组，UI 降级 |
| 阶段进度又变成猜测 | 里程碑优先；时间兜底封顶 90%，不自动满 |
| `command_surface` 回归 | 不改 Tauri 命令签名，只加字段 |
| 定时器泄漏 | 与任务生命周期绑定，任务结束即 join |