# W6b reconnect + 可配置呈现模式 — 设计 DRAFT（待 subagent 对抗审计）

> 状态：**未定稿**。本文是 brainstorm 收敛出的设计，提交 architect/reviewer 多轮对抗审。
> 审计目标：揪出可行性/正确性/boot-safety/集成现实的硬伤；区分"设计可定"与"需用户拍板"。

## 0. 术语
- **usnvmemu** = 用户态 NVMe 模拟器（VTL2 内跑 vfio-user AF_UNIX server 的进程，crate
  `nvme_firmware --vfio-user-sock`）。**operator 拥有其生命周期**（运行时启停/换二进制）。
- **underhill** = VTL2 supervisor（openvmm_hcl 进程），vfio-user **client**。
- 已交付：W6b Phase 1（设备 crate，commit 733459d0）+ Phase 2（underhill 集成，0fb8e8b0）+
  realize-only 真机 PASS（57632f6d）。

## 1. 目标 / 约束（用户确认）
- usnvmemu 运行时随时启停/重启 + dev 迭代不重建 IGVM + operator 按需 attach/detach。
- underhill **持久 reconnect**（非 boot 一次性 connect+cap）。operator 起的 usnvmemu socket
  对 underhill 可见（POC-1 同 mnt ns 已证）。
- **呈现模式 per-实例 CLI 字段可配**：`OPENHCL_VFIO_USER_NVME=<guid>:<path>:<mode>`。
- 三模式：`slot-always-declared` / `slot-always-learned` / `hotplug`。
- usnvmemu 作"独立托管服务" = **future 实验项目，本设计不含**。
- boot-safety 不可回退（realize-only PASS：env-gated + 兜底 + boot 不挂）。

## 2. 已 POC 验证的平台事实（usnvmemu/experiments/2026-06-12-w6b-reconnect-poc/RESULT.md）
- POC-1：ohcldiag-dev run 子进程 ⇄ underhill_core 同 `mnt:[4026531832]` → operator 起的
  `/tmp/X.sock` 对 underhill 可见。
- POC-2：usnvmemu `setsid` 脱离 run-shell 持久 listening（跨 run 返回存活）。
- POC-3：另一进程（underhill proxy = W5a client）跨独立 run invocation connect+全握手+dma_map+
  Identify 零拷贝端到端 PASS。
- POC-4：kill usnvmemu→socket 残留→connect=ECONNREFUSED(111)；relaunch(rm+rebind)→fresh
  connect PASS。**结论**：ENOENT/ECONNREFUSED/握手失败三类都要当"重试"。
- **未验证**：underhill reconnect 代码本身；VPCI `offer_device/revoke_device` 运行时对本设备的行为。

## 3. 当前已交付代码形态（reconnect 要改的基线）
- `worker.rs`：`select_biased!` 三 arm（shutdown / from_device / recv_reply_or_pending）。
  **单次 wire Err → go_lost(store Lost + drain in_flight)**。**无** transport-swap arm（Phase 1
  删了，lib.rs 注记"若需 server-initiated 再加 split"）。worker 持 `writer/reader`（W6a
  into_channel 全双工 split，B-1）。
- `device.rs`：cfg/MSI-X 本地；其它 BAR MMIO defer→worker。Lost 时 cfg_read 返 InvalidRegister。
- `spawn.rs`：connect 一次性（MAX_CONNECT_ATTEMPTS=32 cap → 放弃）+ `prepare_from_client`
  （handshake+identity→PreparedVfioUserDevice）。
- `resolver.rs`：prepared 缺 → AbsentPcieDevice；有 → assemble（三件套 + set_irqs timeout +
  spawn worker + irq tasks）。**identity 来自 prepared.hardware_ids（首连学到）**。
- `state.rs`：Connecting/Live/Lost 状态机（已有，reconnect 复用）。
- underhill `worker.rs` 注册块：connect spawner + boot grace poll + add_async_resolver +
  WorkerTasks Arc 保活。

## 4. 提案设计

### 4a. reconnect 地基（服务全部 3 模式）
新 `reconnect` 模块：持久连接器 task（每实例一个，resolver/spawn 期 spawn，句柄入 WorkerTasks）。
```
loop {
  match connect(unix_path).await {            // ENOENT/ECONNREFUSED → 下面 retry
    Ok(stream) => match handshake+identity(stream).await {
      Ok((client, ids)) => {
        // identity 处理见 4c；把 client.into_channel() 的 (writer,reader) 经
        // mesh::Sender<ReconnectEvent> 投给 worker（transport swap）→ 置 Live
        send_to_worker(Connected{writer,reader,ids}); 
      }
      Err(_) => backoff,                       // 握手失败 → retry
    }
    Err(ENOENT|ECONNREFUSED|_) => backoff,     // 不可用 → retry（POC-4）
  }
  // 等 worker 通知"本连接已 Lost"（wire err）再回到 loop 顶重连
  wait_lost_signal().await;
}
```
- backoff 有界指数（如 100ms→…→2s 上限），无总 cap（持久）。CVM gate 仍在（CVM 不起 reconnect）。
- worker 改造：加 `ReconnectEvent` arm。收到 `Connected{writer,reader,ids}` → drain 旧 in_flight +
  换 writer/reader + 置 Live + （learned 模式）填 identity。wire EOF/err → 置 Lost + 通知 reconnect
  task（一个 lost channel / Notify）回到重连。**照搬 pcie_remote K-20 transport_swap 的 drain+swap+
  revive 序**（但本侧 client 主动连）。

### 4b. 三模式在 resolver/device 的落点
- `slot-always-*`：resolver **总是**呈现 `VfioUserPciDevice`（不再因 prepared 缺就 Absent）；
  reconnect task 后台驱动 Live/Lost。Connecting 初态。
- `hotplug`：resolver 呈现一个"隐藏/未 offer"的设备；reconnect 首次 Connected → `offer_device()`；
  Lost → `revoke_device()`；reconnect 再 Connected → 再 offer。
- CLI `<mode>` 字段解析进 handle → resolver 据此选行为。

### 4c. identity 处理
- `slot-always-declared`：identity 来自 CLI 声明或固定 NVMe 默认（vendor 0x1414/class 01:08:02/
  BAR0 size/MSI-X count）。**boot 即可建 cfg_space**，无需先连 usnvmemu。
- `slot-always-learned` / `hotplug`：首连 `prepare_from_client` 学 identity；
  - learned：device 先以 Connecting 呈现（cfg_read not-ready/特定返回），学到后**重建/填充 cfg_space**
    转 Live；缓存 identity，重连复用 + 校验（identity 变了 = 换了不同 usnvmemu，按错误处理）。
  - hotplug：学到 identity 后才 `offer_device`（offer 时 cfg_space 已就绪）。
- **开放问题 A（疑似硬伤）**：`ConfigSpaceType0Emulator`/`MsixEmulator`/`DeviceBars` 在 resolver
  assemble 时一次性构造。learned 模式"连上后才知 BAR size/MSI-X count"→ 需要**运行时重建 cfg_space**
  或**先用占位 BAR/MSI-X 再改**——pci_core 是否支持运行时改 BAR layout/MSI-X count？若不支持，
  learned 模式要么退化（用声明 identity）要么需要 hotplug（revoke→以新 identity 重 offer）。**审计重点**。

### 4d. operator surface + dev 迭代
- usnvmemu 启停 = operator 经 ohcldiag-dev run（setsid 持久，POC 范式）起/`pkill -x` 停/换二进制重起。
- dev 迭代：改 usnvmemu 代码 → cross-build musl → push 新二进制（**校验 size 防截断**，POC-2 坑）→
  重起 → underhill reconnect 自动重连（无需重建 IGVM、无需 restart underhill）。
- 提供 harness 脚本（push+launch usnvmemu / stop / swap）归档进 experiments。

### 4e. 分层 / 构建顺序（方案 1）
- **Layer A（地基）**：reconnect 模块 + worker swap arm + Live/Lost 重连 + `slot-always-declared`
  模式（identity 声明，最简，不依赖运行时 cfg 重建）。real-VM 验证：operator 起 usnvmemu→guest
  枚举+IO；停→Lost；重起→Live 恢复。**这是第一个"guest 真用上"的真机里程碑**。
- **Layer B**：`slot-always-learned`（识 identity 学习+缓存，解开放问题 A 后定形态）。
- **Layer C（hotplug）**：offer/revoke 集成（需 POC offer/revoke 运行时行为）。
- 每层 coarse commit + 真机验证 + subagent review。

## 5. 我认为"需用户拍板"的开放分叉（其余应设计可定）
1. backoff 上限值 + 是否暴露为 CLI/env（建议 2s 上限，先不暴露）。
2. learned 模式遇到"重连后 identity 变了"（operator 换了不同 usnvmemu）：报错置 Lost 不动 cfg vs
   按 hotplug revoke+重 offer。依赖开放问题 A 结论。
3. slot-always-declared 的固定 identity：硬编码 NVMe 默认 vs CLI 也能声明覆盖。

## AUDIT ROUND 1（Explore 事实核查，2026-06-12）— 重大修正

读 pci_core / vpci / pcie_remote 源码得到的硬事实（file:line 见 audit）：
- **Q1 pci_core 构造即定**：`ConfigSpaceType0Emulator`/`MsixEmulator`/`DeviceBars` 的 BAR
  size/layout、HardwareIds、MSI-X count **构造后无 setter，运行时不可改**（仅 OS-programmed BAR
  地址 + 易失寄存器可变）。MSI-X save/restore 甚至把 count 变化当 InvalidState。
  → **"首连学到 identity 再改活 cfg_space" 不可能**。
- **Q2 offer/revoke 是 host-owned VF bus 的 GET RPC**（`HclVpciBusControl`→`get.offer/revoke_vpci_device(bus_instance_id)`，
  仅 netvsp/MANA VF 用）。`pci_resources` 解析的设备走 `build_vpci_device` **一次性建 VpciBus，
  无运行时 revoke/re-offer 句柄**。→ **真热插拔本设备 BLOCKED，需净新增管线**。
- **Q3 pcie_remote = 槽位恒在 + cfg-read-error 表达 Live/Lost；K-20 "hotplug"=transport 重连
  （槽位持续），非 guest hot-add/remove**，且**不**涉 offer/revoke。其 `worker.rs` transport_swap
  是 reconnect 的参照实现。当前 W6b worker 是简化版（无 swap arm、一次性 connect）。

**对三模式的判决**：
- `slot-always-declared`：**唯一 as-is 可行**。identity 用固定 NVMe 默认/CLI 声明，cfg_space 构造期
  即正确；reconnect = 把 pcie_remote transport_swap revive 序移植成 client 主动连。无需改 pci_core/vpci。
- `slot-always-learned`：真运行时学习 **被 Q1 阻**。可行形态只有 **declare-then-validate**（boot 用
  声明 identity，首连只**校验**学到的 identity 是否一致，不一致则降级/报错）——本质并入 declared。
- `hotplug`：**被 Q2 阻**，需净新增"可控本地 VpciBus（detach/re-attach + 重触发 offer）"管线，是
  research/plumbing 项目（Layer C / 未来），不是配置旋钮。

## 修正后的设计（待 ROUND 2 architect 审）

**放弃"3 模式可配置旋钮"作为近期目标**（Q1/Q2 证其 2/3 不可行）。收敛为：

- **Layer A（近期唯一可行的"完整能用"单元）= slot-always-declared + client reconnect**：
  - resolver **总呈现** `VfioUserPciDevice`（identity 固定 NVMe 默认/CLI 声明；不再 prepared 缺就 Absent
    ——但保留 CVM-gated + env-gated；env 未设仍不注册，boot-safety 不退）。Connecting 初态。
  - 新 `reconnect` 模块：client 主动持久连（ENOENT/ECONNREFUSED/握手失败→backoff 重试，POC-4）；
    连上→`into_channel` 投 (writer,reader) 给 worker→Live；wire err→Lost→通知重连。
  - worker 加回 transport-swap arm（照搬 pcie_remote drain+swap+revive；与 B-1 全双工 split 协同）。
  - **交付**：operator 起 usnvmemu→guest 枚举+真 IO；停→Lost；重起/换二进制→revive Live。
    = 运行时启停 + dev 不重建 IGVM + Lost/Live 意义上的 attach/detach。**第一个"guest 真用上"真机里程碑**。
- **learned**：仅作 declare-then-validate 校验（首连核对 identity），非真学习。低增量价值（usnvmemu
  identity 实际固定），**可选小特性**，非独立模式。
- **hotplug（真 hot-add/remove）**：未来 plumbing 项目（让 build_vpci_device 的 VpciBus 可控）。
  先 POC 可控 VpciBus 可行性再定。与"独立托管服务"并列为 future。

## AUDIT ROUND 2（architect 设计审，2026-06-12）— 收敛

architect 独立复核确认 ROUND 1 三事实，判修正设计**根本可行**，但揪出 3 个 CRITICAL（已给具体解法）+ 数个 IMPORTANT，并把绝大多数其余定为"设计可定"。

**3 个 CRITICAL（必须进实现，happy-path harness 看不见，对应 [[dbbuf-shadow-doorbell-complete]] §29 教训）**：
- **C-1 pair-swap 原子性**：worker 持 writer+reader 两半（共享同一 Arc'd socket）。swap 必须**一条消息携两半** + **两个赋值间无 .await**（否则新 writer 配旧死 reader → swap 后所有 in_flight read 静默超时）。drain 是 sync，先 drain 再赋两半再 flip Live。
- **C-2 boot-safety**：`Connecting` 态当前走真 cfg（device.rs:131-135）→ guest 绑 nvme 驱动到没 usnvmemu 的死控制器 → 挂（正是 291d8645 OS-hang 类）。修：**`Connecting` 在 cfg 面等同 `Lost`（返 InvalidRegister）直到首个 Live**。= assemble-always（resolver 总建设备让 reconnect 有 worker 可换）+ show-absent-until-Live（没 usnvmemu 时 guest 看"不存在"，等同今 AbsentPcieDevice）。AbsentPcieDevice 缩为仅 assembly-failure 兜底。
- **C-3 reconnect 须重发 set_irqs**：重连是全新 client/socket，旧 eventfd 对它未知。连接器必须**用持久化的同一组 eventfd 在 into_channel 前对新 client 重发 set_irqs**，否则 revive 后 MSI-X 死（NVMe 命令全超时）。eventfd 由 device 生命周期持有（irq_tasks/events），连接器需其 BorrowedFd 句柄。

**IMPORTANT（设计可定，已决）**：
- lost 通知**边沿触发**（`go_lost` 用 `try_transition(Live/Connecting→Lost)` 成功才 `lost_tx.send(())` 一次/次断），防 level-triggered 双重连 flap。
- 握手/identity 在**连接器自己的 serial client**（round-trip 安全），只 post-`into_channel` 两半进 worker（保 B-1 不死锁）。
- 连接器 task 句柄入 keepalive Arc（非裸 detach）。CVM gate 也门控连接器 spawn。env-gate 不变（空 CLI→不注册→零变化，已核 options.rs:699-704 / vtl2_settings:1927-1936）。
- next_msg_id 跨 swap 不重置（镜像 pcie_remote）。
- §3 reframing 滴水不漏：`set_presence_detect_state`/`hotplug_*_device` 是 **PCIe-port 拓扑**（非 VPCI），且本设备无 PCIe cap → 无捷径。learned 真运行时学习被 Q1 死锁，hotplug 被 Q2 死锁，判定正确。

**收敛设计（consensus）= Layer A**：
1. resolver **assemble-always**（每配置实例总建 VfioUserPciDevice + worker + irq tasks + cfg_space 用**声明 geometry**）。AbsentPcieDevice 仅 set_irqs/PolledWait assembly-failure 兜底。
2. device **show-absent-until-Live**（`Connecting`→cfg 返 InvalidRegister，同 Lost；Live 后才真 cfg）。
3. **reconnect 连接器** task（每实例，`!is_hardware_isolated()` 门控）：persistent loop connect→prepare_from_client→**validate identity**（drift=log+continue；actual>declared BAR0/MSIX=stay-Lost+error）→**重发 set_irqs（持久 eventfd）**→into_channel→**一条 `Connected{writer,reader}`**→`lost_rx.next().await` 阻塞至本连接死→loop。backoff 100ms→2s 无总 cap；ENOENT/ECONNREFUSED/握手失败皆重试。句柄入 keepalive Arc。
4. worker **加一个 swap arm + 边沿 lost**：`reconnect_rx: mesh::Receiver<ReconnectEvent>`（MPSC）；arm 做 drain(sync)→赋两半(无 await)→state=Live；`go_lost` try_transition 边沿 `lost_tx.send(())` 一次。
5. **声明 geometry = usnvmemu 实际固定值**（vendor 0x1414/class 01:08:02/BAR0 实际窗口/MSI-X 实际数），与 firmware 一起 commit，非臆造默认。

**剩余真正需用户拍板（其余全设计可定，已决）**：
- (a) "完整"的定义：Layer A 复活**已枚举**的 function（usnvmemu 须在 guest PCI 枚举窗口内起；运行时重起=revive 已绑 function）；**at-boot-absent function 的 guest 冷插= hotplug=Layer C**。接受此为"Layer A 完整"吗？
- (b) actual BAR0/MSI-X **超过** declared 时：stay-Lost+error（安全，推荐）vs best-effort clamped-Live（冒险）？
- (c) 声明 geometry：仅 hardcode-from-usnvmemu（推荐，YAGNI）vs 加 CLI override（`...:bar0=<n>,msix=<n>`）？

## 6. 给审计者的问题（已由 ROUND 1+2 回答）
- 4a reconnect + worker swap 的并发/正确性：drain 时序、lost 通知与重连的竞态、与 B-1 全双工
  split 的交互、句柄保活/取消语义，有没有死锁/丢事件/泄漏？
- 开放问题 A（运行时 cfg_space/BAR/MSI-X 可变性）是不是真硬伤？pci_core 现实支持到哪？这决定
  learned/hotplug 可行性与形态。
- offer_device/revoke_device 怎么拿到（哪个 trait 对象、谁提供给 resolved PCI 设备）？运行时
  对一个 vpci-resolved ChipsetDevice 调 offer/revoke 现实吗？需要哪些前置？
- boot-safety：slot-always 模式"总是呈现设备"会不会回退 realize-only 的 boot 不挂 / env-gated /
  CVM-gated 保证？device 在无 usnvmemu 时长期 Connecting/Lost 对 guest nvme 驱动的影响？
- 分层顺序对不对？Layer A 是不是最小可独立真机验证的"完整能用"单元？
- 哪些是设计可定、哪些必须用户拍板（避免把可定的也丢给用户）？
