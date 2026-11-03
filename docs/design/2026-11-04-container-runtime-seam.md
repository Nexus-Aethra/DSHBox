# 2026-11-04 — 容器运行时接缝（VM 前置重构）

## 背景

调查（见会话 2026-11-04）确认了两件事：其一，daemon 的**控制面与观测面已经资源化**——
`HostState` 七态状态机、每容器 `state/host.json` 记录、box-scheduler 任务队列、SSE 事件流、
`describe_container` 描述 RPC——虚拟运行模式可以直接挂进这套调度与观测；其二，**执行面没有抽象**：
`ManagedHost.child` 直接持有进程句柄（`TrackedChild`），`ProcessSpec` 是纯进程形状。

本 PR 只做接缝：引入运行时种类与句柄抽象，行为零变化。VM 后端（Linux Firecracker、
Windows QEMU/WHPX 或 WSL2）在此后的 PR 中落进同一接缝。

## 设计

### RuntimeKind（容器记录的新可选字段）

`container.json` 增加可选字段 `runtime`：

- **缺省即 `host`**——没有这个字段的容器（含全部既有容器）行为与今天逐字节一致；
  创建路径不主动写入该字段。
- `"host"`（大小写不敏感）显式声明进程模式。
- `"vm"` 或 `"vm:<backend>"`（如 `vm:firecracker`、`vm:wsl2`）声明虚拟运行模式。
- 其它字符串是**启动期错误**，绝不静默降级：记录请求了一个进程后端给不了的安全边界，
  沉默降级等于谎报隔离属性。

### HostHandle（守护进程持有的"已启动宿主"）

`ManagedHost.child: TrackedChild` 改为 `handle: HostHandle`：

```rust
pub(crate) enum HostHandle {
    /// 本守护进程的子进程；TrackedChild 拥有 kill/reap。
    Process(Box<TrackedChild>),
    // VM 后端在此追加自己的臂（VMM API 客户端或模拟器进程句柄），
    // 上层动词（ps/url/stop/restart/describe）不需要知道。
}
```

暴露三个动词真正用到的操作：`id()`（观测与孤儿清理）、`try_wait()`（存活判定）、
`stop()`（优雅树杀，Drop 兜底强杀不变）。VM 臂的同名操作走 VMM API。

### 分发点

`start_dsh_container_inner` 读到非 `host` 的 RuntimeKind 时，以明确错误失败
（`container runtime vm:<backend> is not available in this build`）——这是 VM 后端的
落点，不是降级点。

## 不变的东西

- agent 工具面：只认 loopback 端口 + token，VM 后端只要转发端口即零改动
- 就绪探测、host.json 形状、任务队列、SSE 事件、资源注入、模板体系
- `delete_container` 精确匹配等既有边界

## 后端规划（后续 PR）

- Linux：Firecracker（KVM），rust-vmm 生态同构
- Windows：QEMU/WHPX（真 VM 硬隔离，QMP 驱动）或 WSL2 每实例 distro（省事，容器间共享内核）——
  选型待独立评估
- 镜像流水线：guest rootfs 复用 sealed 模板构建产物；`runtime-lock.json` 的锁定模式照抄