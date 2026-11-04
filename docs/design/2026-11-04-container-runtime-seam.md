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
    Process(TrackedChild),
    // VM 后端在此追加自己的臂（VMM API 客户端或模拟器进程句柄），
    // 上层动词（ps/url/stop/restart/describe）不需要知道。
}
```

句柄只暴露三个动词真正用到的操作：`id()`（观测与孤儿清理）、`has_exited()`（存活判定；
刻意不返回 `ExitStatus`——它没有稳定构造器，VM 臂无法实现）、`stop()`（优雅树杀，
Drop 兜底强杀不变）。

### 分发点

`start_dsh_container_inner` 读到非 `host` 的 RuntimeKind 时，以明确错误失败
（`container runtime vm:<backend> is not available in this build`）——这是 VM 后端的
落点，不是降级点。

### 可观测

`describe_container` 回报规范化后的 `runtime` 字段。三种情况：记录无该字段、或值无法
解析（读动词不能失败——**这是唯一一处坏值不报错的地方**，强制执行点在 start）——
答 `"host"`；`"host"` 答 `"host"`；`vm[:backend]` 答规范化的小写 `"vm:backend"`。

## 不变的东西

- agent 工具面：只认 loopback 端口 + token，VM 后端只要转发端口即零改动
- 就绪探测、host.json 形状、任务队列、SSE 事件、资源注入、模板体系
- `delete_container` 精确匹配等既有边界

## 后端落地（feat/vm-backends，实验性）

选型已经按 2026-11-04 的实测定下来，不再是待评估项：

- **Windows → WSL2 每容器 distro**。实测本机是 Windows 10 Home China 26H2：VirtualMachinePlatform 与 WSL 已启用，但 HypervisorPlatform(WHPX) Disabled、Microsoft-Hyper-V 完全缺失、Containers Absent —— Home 版无法提供真 VM 硬隔离；libkrun 的 Windows/WHP 后端官方仍标 upcoming。所以 WSL2 是当前唯一可用选项。它**不是硬隔离**：两个 distro 实测共享 boot_id、内核、hostname 与 **netns**（guest 的 127.0.0.1:45999 从 Windows 宿主与另一 distro 都能连上）；隔离的是 mntns / pidns / utsns。对「只隔离文件系统」的诉求够用，不能宣称 VM 级隔离。
- **Linux → libkrun microVM**（KVM）。libkrun 1.9.x 经 dlopen 加载，krun_start_enter 阻塞，故 VMM 跑在 dshboxd --vmm-run <cfg> 子进程里；网络走 TSI，guest 的 loopback 直接就是宿主的。本机 WSL2 内 /dev/kvm 存在（root 下 CREATE_VM/CREATE_VCPU ioctl 通过），嵌套虚拟化可用；libkrun 未安装，需源码构建后才能实机验证。
- **macOS → Apple VZ + vsock relay**（规划，未实现）。

### 实测得到的硬约束

- drvfs/9p 的**元数据**才是瓶颈，不是带宽：安静时单次 stat 3.7 ms、open 4.6 ms（ext4 为 1.3 / 1.1 ms）；packages/ 336 MB 经 tar 管道从 drvfs 进 ext4 要 **262.8 s**（~1.3 MB/s），而单个 30 MB 文件顺序读有 52.4 MB/s。整棵 harness 树 108,685 文件 / 2,271 MB，逐文件拷进去 >25 分钟，远超就绪窗口。
- **整机镜像导入/导出很快**：wsl --export 1327 MB tar 26.6 s，wsl --import 14.5 s。所以拷贝的单位必须是「整个镜像 tar + 每容器独立的 profile/(62.5 MB)」，不能逐文件。
- 产物必须按**目标平台**判定：client_artifacts_present 现在用 build_platform()（宿主 OS-ARCH）比对 marker，Windows 准备的 win32 原生 addon 对 Linux guest 永远不命中。Linux 产物需要在 Linux 内 pnpm install && pnpm run build 生成，不能从宿主拼出来。

### libkrun 的已知缺口（进程组已在 fix/libkrun-vmm-pgroup 修好；仅 virtiofs/console 缺口留后续 Linux 实机分支）

- **VMM 子进程进程组**：`dshboxd --vmm-run` 起初用裸 `Command::spawn`，Unix 下继承 daemon 的进程组；而 `TrackedChild::kill_tree` 发的是 `kill(-pgid, SIGTERM)`（`box-runtime/src/process/lifecycle.rs`），停止一个 VM 容器会把 daemon 与所有 host 容器一起 TERM。现复用 `configure_non_interactive(&mut cmd, true)` 做 setsid；Linux 实机实测：无 setsid 时子 pgid = 父 pgid 558，setsid 后子 pgid = 自身 pid 598。
- **virtiofs 不会自动挂载**：libkrun 1.9 的 guest init 只挂 `/dev /proc /sys /dev/pts /dev/shm`，`krun_add_virtiofs` 仅注册设备。config 里的 `/dsh`、`/home`、`/container` 挂载点与 `mount -t virtiofs` 必须由 guest init 显式完成，否则 `workdir /dsh`、`apps/cli/lib/bin.js`、`/container/box-web.patch.yml`、`DSH_HOME=/home` 全部不存在。此外 `krun_set_console_output` 是 VMM 侧文件，DSH 的 `dsh web:` 行不会自动追加到宿主 `logs/host.log`，`parse_announced_host_url` 因此拿不到 authenticated URL。**Libkrun 分支在本机无 libkrun.so，尚未实机验证，不能视为可用后端**；路线图 C 阶段按「供给适配器 + helper 在 guest 内挂载/启动」重做这条路径。

### 下一步（本分支之后，独立规划）

上述约束说明真正的抽象不是「给模板加一个已编译标志」，而是把宿主也抽象成一个 **node**：每个 node 是一台真实执行环境（host / WSL2 / libkrun / VZ / docker），建立后第一件事连控制层注册；box-helper 跑在 node 内，承接 clone/install/build/export/copy 等全部文件操作，控制层只派意图。这样 host 与 VM 走同一条路径，镜像按 (node_id, base_id) 缓存，且可以在模板上指定运行到哪个 node。本分支**不含**这套重构，完整施工顺序见 [`docs/design/2026-11-04-node-architecture-roadmap.md`](2026-11-04-node-architecture-roadmap.md)。
