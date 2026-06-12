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

---

## 发现③（真机暴露的设计问题）：C-2 show-absent 阻止 guest 枚举

**现象**：device Live（usnvmemu 在 guest PCI 枚举前就连上）后，guest 仍把设备枚举为 `PCI\VEN_1414&DEV_0000`（Status Unknown，无驱动，无 NVMe disk）。removet 旧 devnode + `pnputil /scan-devices` 不重现（PCI rescan 不重触 vmbus VPCI offer）。

**根因**：VPCI offer 在 `assemble_device`（**Connecting** 态，usnvmemu 连上**之前**）就发生，guest 据 **offer 时刻的 config** 创建 devnode（且 Windows devnode 的 vendor/device 是 enum 时一次性定的，不随 rescan 变）。**C-2 让 Connecting 态 cfg_read 返 Err/absent → offer 捕获到 device_id=0 → guest 枚举成 DEV_0000/Unknown**。设备后来 Live（cfg_space 正确呈 DEV_00A9，已单测）也不重触 offer。即 **C-2 "show-absent via cfg-Err" 直接阻止 guest 正常枚举**——这是 architect round-2 设计的 C-2 的过度修正，真机才暴露。

**修法（option-A：槽位恒在+真身份）**：device.rs **cfg_read/write 恒呈真（declared）config**（含 Connecting），让 offer/enum 捕获 DEV_00A9 → guest 加载 stornvme.sys；**只把 MMIO 按 Live 门控**（非 Live 返 `Err`，**绝不 `Defer`** → driver 重试/报错，不挂——291d8645 hang 是 Defer 致，Err 安全）。usnvmemu 起→Live→driver MMIO 命中真 NVMe→disk+IO；usnvmemu 停→Lost→MMIO Err→driver 报错；重起→Live→恢复。boot-safety：usnvmemu 从不连时 guest 看到一个"非功能 NVMe 控制器"（driver init 失败/重试，非 hang）——可接受（等同插了块没响应的盘）。

**与决策(a)的关系**：决策(a)"复活已枚举 function" 隐含 function 得先被有效枚举；但 offer 在 Connecting + C-2 → 永远枚举不成。修法对齐二者：恒呈真 config→offer 即有效枚举(DEV_00A9)→之后靠 MMIO Live/Lost revive。

**涟漪 + 下一步**：① device.rs 改（cfg 恒真 + MMIO 仅 Live；revert 部分 C-2）；② 更新 device.rs 单测（`connecting_cfg_read_returns_err_like_lost` 等需改为"Connecting cfg 呈真、MMIO Err"）+ loopback；③ subagent review（boot-safety 不退 + 不引入 291d8645 hang）；④ rebuild vpci IGVM；⑤ 真机复验：clean guest（或 remove 旧 VEN_1414 node）+ race-start usnvmemu→Live→guest 枚举 DEV_00A9 + stornvme 加载 + NVMe disk + 真 IO（host backing-file 独立 oracle）+ revive。

**注**：本 guest 已被多轮历史实验污染（stale VEN_1414 DEV_C0DE/v1 nodes）；复验最好用干净 guest 或先清 stale node。

---

## ✅ 发现③ 已修复并真机 PROVEN（commit bbdfd7cc）

C-2 修订（cfg 恒呈真 + MMIO 仅 Live 门控）实现 + rust-reviewer APPROVE（hang-safe：
non-Live MMIO 路径无 Defer）+ VPCI 源码追踪确认机制（`VpciChannel::new` 经
`probe_hardware_ids`/`probe_bar_masks` 调设备自身 `pci_cfg_read` 在 assemble 态一次性
latch 身份；`vpci/src/device.rs:1339-1345` + `chipset_device_ext.rs:47-76`）。rebuild
vpci IGVM（增量）→ 真机复验：

- **boot（proven 配置，device cmdline，无 wait-for-start）** → VTL2 kmsg：
  `vfio_user_pci_device::resolver: device assembled (Connecting) msix_count=0x4
  bar0_size=0x4000`（C-2 修订码）+ 持久连接器 backoff 重试。
- **push musl usnvmemu（base64-stdin，remote_size=3148048 校验）+ launch（setsid 脱离）**
  → usnvmemu：listening → client connected → VERSION ok → namespace registered
  (524288 LBA) → **DEVICE_SET_IRQS count=4 fd_cnt=4**（C-3）→ underhill kmsg
  `reconnect: connected, handed transport to worker` + **`worker: reconnected, Live`**。
  （reconnect 生命周期在 C-2 修订码上无退化。）
- **guest PSDirect 枚举（决定性证据）**：
  ```
  Status Class       FriendlyName                    InstanceId
  Error  SCSIAdapter Standard NVM Express Controller PCI\VEN_1414&DEV_00A9&SUBSYS_00000000&REV_01\...
  ```
  **= 修复前 DEV_0000/Unknown/无驱动 → 修复后 DEV_00A9 + "Standard NVM Express
  Controller" + stornvme.sys 绑定（Class=SCSIAdapter）**。finding-③ 真机 FIXED。
  Status=Error + disk=0 是 C-2 设计的预期行为：stornvme 在 boot 期（device 尚
  Connecting，[107.9]s 才 Live）就 init 控制器 → MMIO Live-gate 返 Err → init 失败。
- **disable→enable devnode（device 已 Live）→ 重 init**：MMIO 现命中 Live 控制器
  （kmsg/usnvmemu 日志确认 CC.EN/CSTS/doorbell/admin-queue setup 全通），证 MMIO 数据
  路径真通。但 disk 仍 0 → 暴露 **发现④**（见下）。

**教训补充（VTL2 push）**：`ohcldiag-dev <vm> run` 的 clap 会吞 `-c`，须 `run -- sh -c '...'`
（`--` 标记 positional）。base64-stdin push 经 `run --` 转发 host stdin 到 VTL2 进程，
remote_size 校验确认完整。

---

## 发现④（真机暴露的集成缺口）：W6b underhill 设备从不发 DMA_MAP → 控制器 DMA 失败

**现象**：finding-③ 修复后 MMIO 全通（CC.EN/CSTS/doorbell），usnvmemu 起好 admin
queue（`asq=0xeae2b000`），但取 admin SQ entry 时 DMA 失败：
```
WARN vfio_user_transport::session: VfioUserSession.dma_read failed
     error=DMA_READ 0xeae2b000+64 not in any DMA region gpa=0xeae2b000 len=64
WARN nvme_firmware::controller::completion: DMA failed token=32768
```
usnvmemu 收到 **0 个 DMA_MAP**（`grep -c DMA_MAP=0`）→ 控制器无法读 guest RAM 里的
NVMe 队列/缓冲 → Identify 永不完成 → 无 namespace → 无 disk。devnode 停在 Error。

**根因**：W6b underhill 设备 worker **只发 REGION_READ/REGION_WRITE（MMIO）**，DMA 被显式
推迟（`worker.rs:14-15` 注释："无 control-socket DMA；Phase 3 走 dma_map 零拷贝 —— 删"
/ "ReadGpa/WriteGpa/guest_memory … 删"）。即 reconnect 生命周期 + 枚举（DEV_00A9）已通，
但**真正的 NVMe 数据路径（DMA_MAP 零拷贝）从未接进 underhill 集成**。这是 W6b "drive it"
的核心缺口 = **W6c**。

**scope（subagent 源码追踪，verdict=(b) medium——非架构级）**：硬件件全在且真机验过——
- 进程内 fd 在：`MshvVtlLow`（`underhill_mem/src/init.rs:221` gpa_fd；`hcl/src/ioctl.rs:683-708`
  exposes `get()->&File`，可 dup 给 SCM_RIGHTS）。
- client send API 在：`VfioUserClient::dma_map(gpa,size,flags,fd:BorrowedFd,fd_offset)`
  （`vfio_user_device/src/client.rs:362-391`）。
- server mmap 侧已处理 mshv_vtl_low 字符设备 fd（W5a fstat 修已在 `vfio_user_transport/src/dma.rs:456-516`）。
- **W5a 已在真 OpenHCL VM PROVEN 这条 dma_map(/dev/mshv_vtl_low fd) 零拷贝**
  （`experiments/2026-06-12-openhcl-vtl2-deploy/`）。
- 生产参考：`vhost_user_frontend` 走 `guest_memory.sharing()→get_regions()→ship fds`
  （`vm/devices/virtio/vhost_user_frontend/src/lib.rs:327-340,731-766`）。

**决定性 gap**：`params.guest_memory: &GuestMemory` 在 resolver 已可达
（`pci_resources/src/lib.rs:42`），但 underhill 的 VTL0 `GuestMemoryView`
（`underhill_mem/src/mapping.rs:115`）**未实现 `sharing()`**（继承默认 `None`）。故需 plumb。

**两策略（W6c 设计 either-or，待 brainstorm/review）**：
- **A**：给 underhill VTL0 `GuestMemoryView` 实现 `sharing()`（更干净、OS-portable、复用
  vhost pattern）。承重 caveat：`ShareableRegion` "全 commit、无 bitmap-gating" 契约 vs
  underhill bitmap-gated VTL0 mapping——非隔离路径基本 OK，CVM 需审。
- **B**：经 handle/resolver 显式传 `MshvVtlLow` fd / `GuestMemorySharing`（blast radius 小、
  literal 照搬 W5a）。

**reconnect 涟漪**：DMA_MAP 必须每次重连重发（server 重启＝新进程，DMA 表空）——正是 C-3
set_irqs 重发的同一生命周期理由，挂在 `reconnect.rs:169-187` 同处。

**待 POC 的承重假设**：① 单/多 DMA_MAP 能否覆盖高 GPA（0xeae2b000≈3.67GiB；W5a 只验过
0x100000 处 64KiB）——应按 `memory_layout.ram()` 每段一个 region；② `file_offset` 在
SHARED_MEMORY_FLAG/iova_offset/vtom 各情形的正确性（非-CVM 线性，已验；CVM 需 POC）；
③ ShareableRegion committed 契约 vs bitmap-gating（非隔离 OK，CVM 是真问题）。

**下一步（W6c）**：POC 假设① loosest-first（扩 W5a 客户端映射高 GPA）→ 决 A/B（brainstorm
+architect review）→ spec/plan → 实现（resolver 取 regions + reconnect 重发 DMA_MAP，照 C-3）
→ rebuild → 真机复验 guest disk + 4MiB IO（VTL2-backing 独立 oracle）+ revive。
