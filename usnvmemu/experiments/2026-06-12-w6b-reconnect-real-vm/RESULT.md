# W6b reconnect — 真机 e2e（真 OpenHCL VM）结果

**日期**：2026-06-12　**VM**：pcie-remote-exp（真 OpenHCL Hyper-V VM）
**IGVM**：本会话 reconnect 代码（commit `8c47c4b9`）+ **`--override-openvmm-hcl-feature vpci`** 构建。
**usnvmemu**：`nvme_firmware --vfio-user-sock`（musl，经 ohcldiag-dev setsid 推进 VTL2）。

## TL;DR
**核心 reconnect 链真机 PROVEN**（assemble→连接器持久重试→usnvmemu 起→connect+握手+**C-3 重发 set_irqs(4 eventfd)**→device **Live**）。**发现并精确定位 2 个真机问题**：①IGVM 必须带 `vpci` feature（默认 build 漏，曾 mask Phase 2 realize-only）；②**idle 连接下 usnvmemu 死，worker 不检测 EOF → 不 go Lost → 连接器不重连**（revive 缺失）——待修。

## 承重发现 ①：IGVM 必须带 vpci feature（否则设备根本不装配）
- `cargo xflowey build-igvm x64`（默认）只 `--features gdb,tpm`，**无 vpci**。
- underhill `worker.rs` 的 vpci 设备构建在 `#[cfg(feature="vpci")]`；无此 feature → `#[cfg(not(vpci))] if !vpci_devices.is_empty() { bail!("built without vpci support") }` → worker_new ERROR → **control_state 卡 "starting"**（VTL2 diag 仍可达，故"看似 boot 不挂"）。
- kmsg 铁证：`underhill_core::worker: ERROR ... failed to start VM error=built without vpci support`。
- **修复（零代码改）**：`cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci`（链路 openvmm_hcl/vpci→underhill_entry/vpci→underhill_core/vpci）。产物 `flowey-out/artifacts/build-igvm/debug/x64-custom/openhcl-x64-custom.bin`。
- **⚠️ Phase 2 realize-only(`57632f6d`) 是 false-PASS**：它只查 VTL2-reachable + Hyper-V-Running（"built without vpci support" 下这俩仍真），没查 `control_state==running` 或 guest 枚举 → 漏报。教训：真机 oracle 必须查 control_state / guest，不能只查 VTL2 可达。

## 真机 PROVEN：assemble + connect + C-3 set_irqs + Live
vpci IGVM + `OPENHCL_VFIO_USER_NVME=<guid>:/tmp/vfio_nvme.sock` boot 后：
- 设备装配（Connecting，show-absent）；连接器**持久重试**：kmsg `vfio_user_pci_device::reconnect: connect failed; backing off backoff_ms=0x7d0`（2s 间隔，usnvmemu 未起）——**这是 reconnect 引擎在真 underhill 跑通**（Phase 2 的一次性 cap 已被持久重连取代）。
- operator 经 ohcldiag-dev setsid 起 usnvmemu → 连接器连上：
  - underhill：`vfio_user_pci_device::reconnect: connected, handed transport to worker` + `vfio_user_pci_device::worker: reconnected, Live`。
  - usnvmemu 日志：`VERSION handshake ok client 0.1` + `namespace registered` + **`DEVICE_SET_IRQS idx=2 start=0 count=4 fd_cnt=4` → `SET_IRQS: MSI-X trigger eventfds assigned count=4`**（= **C-3 reconnect 重发 set_irqs 的 4 个持久 eventfd 真机生效**）。
- 即：C-2 show-absent（Connecting）+ 持久 reconnect + C-1 swap→Live + C-3 set_irqs 重发，**全在真 OpenHCL VTL2 跑通**。

## 待修问题 ②：idle 连接下 peer 死，worker 不 go Lost（revive 缺失）
- 现象：device Live 后，`pkill -x usnvmemu`（连接 idle，无 in-flight read）→ 连续 8×2s=16s 轮询 kmsg，`going Lost` **count 恒 0**。worker 一直 Live 在死连接上 → 连接器阻塞在 `lost_rx.next()` → 重起 usnvmemu 也不重连（usnvmemu 日志无第二次握手）。
- 对比：loopback `reconnect_loopback.rs` 场景 2（server drop→Lost+drain）**PASS**，但它用 `wait_inflight` 钉了一个 **in-flight read** 才 stop → 测的是"有在途 read 时 reader EOF→go_lost"；**idle（无在途）下 reader EOF 检测这条路径 loopback 没覆盖**。
- 疑点（待 Phase 2 验）：idle 时 worker 的 recv 臂 `recv_reply_or_pending(reader, is_lost)` 在等 header；peer 死→socket EOF→应 readable→唤醒→recv_exact 读 0→UnexpectedEof→go_lost。real-HW(VmTaskDriver) 未触发，loopback(DefaultPool) 触发（但 loopback 没测 idle）。
- **下一步（focused fix）**：① 在 `reconnect_loopback.rs` 加 **idle peer-death 场景**（连上 Live 后**不**发任何 MMIO，直接 drop server）→ 复现 idle-EOF 不 go_lost；② 修 worker recv/EOF 检测（或加 keepalive/连接器侧探测）使 idle peer 死也能 go_lost→revive；③ rebuild vpci IGVM 真机复验 revive（停→Lost→重起→Live）。
- 这是 happy-path harness 看不见的 async 路径 bug（[[dbbuf-shadow-doorbell-complete]] §29 类）——真机暴露的价值。

## guest 枚举（decision a：已枚举 function）
本轮未达 guest 枚举：需 device 在 guest PCI 枚举窗口前 Live。无 wait-for-start 时 VTL0 已枚举（device 那时 Connecting=absent）。wait-for-start boot 本轮遇 VM-start 瞬态（与设备无关，VM Off uptime 0；空 cmdline / 无 wait-for-start 均 boot 正常）。修完问题②后，用 wait-for-start（先 Live 再放 VTL0）或 fast-start usnvmemu 复验 guest 枚举+真 IO。

## 复跑
```bash
cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci   # 必带 vpci！
cp flowey-out/artifacts/build-igvm/debug/x64-custom/openhcl-x64-custom.bin <win>/openhcl-vfio-user.bin
# 配 OPENHCL_VFIO_USER_NVME=<guid>:/tmp/vfio_nvme.sock 启动 → ohcldiag-dev setsid 起 usnvmemu → 看 kmsg "reconnected, Live"
```

### 发现② 细化（standalone 调试 subagent，systematic-debugging，未 guess-fix）
- **worker 代码在 in-process 验证下正确**：recv 臂 `recv_reply_or_pending(reader, is_lost)` 在 Live+idle（in_flight 空）时**确实 await 真 socket read**（无 in_flight 门控）；`recv_exact` 0-byte read→`Err(UnexpectedEof)`→`go_lost(READ_ERR)`。io_uring `PollAdd(POLLIN)` 在 peer close 时**经验证**会 read=0 完成（subagent 在真 IoUringPool 上实测）。`FdReady` poll 注册在 `self.reader`（Worker 持有），select_biased! 重建 recv future 不丢注册。
- **loopback 加 scenario7（idle peer-death）→ PASS**（worker 正确 go Lost）→ **in-process 复现不出**，故 bug 是真机环境特定（standalone harness 模型不到的层）。reactor（io_uring vs epoll）+ topology（直连 AF_UNIX 无 relay）两候选已排除。
- **最高概率根因（待真机验，按序）**：① **peer `pkill` 后 EOF 没真到达 underhill**（accepted fd 被存活线程/子进程持有 / SCM_RIGHTS dup 到别处 / 未在 exit 关闭）——in-process harness 干净关 fd 故复现不出；真机用 `ss -xp`/`lsof`/strace worker fd 验。② 真 `VmTaskDriver` 下 worker task idle 时未被 IO 驱动（io_uring ring 所在 VP/线程 idle 时没 pump）。
- **稳健修法（不依赖 idle-EOF，#1/#2 通吃，且是 NVMe KATO 基础）**：worker `select_biased!` 加 **active liveness 臂**——Live 且 idle N 秒→发廉价探测帧（如 `GET_REGION_INFO`/no-op `REGION_READ`），send `Err` 或 deadline 内无 reply→`go_lost`。把"静默半开/永不 EOF 死亡"转成已处理的 write-err/timeout。**待真机确认 #1（EOF 是否到达）后实现**：若 firmware 漏关 fd，真修可能在 firmware 侧（exit 关 accepted/listener fd）。
- regression guard：`reconnect_loopback.rs` scenario7（idle peer-death→Lost）已加，PASS（7 场景全绿）。

---

## ✅ 更正：发现② 是测试假象，revive 真机 PROVEN（非 bug）

**发现② 不存在。** 根因：VTL2 的 busybox `pkill -x usnvmemu` / `pgrep -x usnvmemu` **匹配不到** comm="usnvmemu" 的真进程（busybox `-x` 语义怪癖；`pgrep -f` 能找到 pid 72 comm=usnvmemu，但 `-x` 返回 NONE）。所以之前"停 usnvmemu"的 `pkill -x usnvmemu` **啥也没杀**→ usnvmemu 一直活着 → worker **正确**保持 Live（peer 没死）。"alive=empty" 是 `pgrep -x` 也匹配不到导致的误读，不是真被杀。

**用正确的 `pkill -f "/tmp/usnvmemu --vfio-user-sock"` 复测，完整 revive 真机 PROVEN**：
- 停 usnvmemu（pkill -f）→ kmsg `going Lost` **count=1 → Lost DETECTED ✓**（worker idle-EOF 检测真机正常工作）。
- 重起 usnvmemu → kmsg `reconnected, Live` **count=2 → REVIVE ✓**（连接器重连 + worker 再 Live）。

**故完整 reconnect 生命周期全在真 OpenHCL 硬件跑通**：assemble(Connecting) → 持久重连 → usnvmemu 起 → connect+握手+C-3 set_irqs → **Live** → usnvmemu 死 → **Lost** → 重起 → **Live(revive)**。C-1/C-2/C-3/边沿-lost/持久重连/Lost-on-peer-death/revive 全部真机验证。

scenario7（idle peer-death loopback）仍是有效回归守卫（PASS 证 idle-EOF 检测逻辑正确）——与真机一致。

**教训**：VTL2 busybox 下 kill 进程用 `pkill -f <cmdline>` 或按 pid，**勿用 `pkill -x <comm>`**（busybox -x 匹配不到）。这个假"bug"耗了一轮诊断——VTL2 工具链怪癖要先验证 kill 真生效（`pgrep -f` 确认）再下结论。
