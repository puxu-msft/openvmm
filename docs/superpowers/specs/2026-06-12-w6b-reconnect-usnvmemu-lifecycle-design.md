# W6b reconnect — usnvmemu 启停灵活性（operator 拥有 + underhill 持久 reconnect）设计

**日期**：2026-06-12　**状态**：设计（经 2 轮 subagent 对抗审计 + POC 链验证 + 用户决策收敛）
**前序**：W6b Phase 1（设备 crate `733459d0`）+ Phase 2（underhill 集成 `0fb8e8b0`）+ realize-only 真机 PASS（`57632f6d`）已交付。本设计是其上的 reconnect 层。

## 0. 术语
- **usnvmemu**：用户态 NVMe 模拟器（VTL2 内跑 vfio-user AF_UNIX server 的进程，crate `nvme_firmware --vfio-user-sock`）。**operator 拥有其生命周期**——运行时自由启动/停止/重启/换二进制。
- **underhill**：VTL2 supervisor（openvmm_hcl 进程），vfio-user **client**，持久 reconnect。
- **device**：`vfio_user_pci_device` crate 向 guest 呈现的 ChipsetDevice（已交付）。

## 1. 目标 / 约束（用户确认）
- usnvmemu 运行时随时启停/重启 + dev 迭代不重建 IGVM + Lost/Live 意义上的 attach/detach。
- underhill **持久 reconnect**（替代 Phase 2 的一次性 connect+cap）。
- boot-safety 不可回退（env-gated + CVM-gated + 无 usnvmemu 时 guest 看"不存在" + boot 不挂）。
- usnvmemu 作"独立托管服务" + 真 guest **冷插**（hot-add/remove）= **future / Layer C**，本设计不含。

## 2. POC 已验证的平台事实（`usnvmemu/experiments/2026-06-12-w6b-reconnect-poc/`）
- **POC-1**：ohcldiag-dev run 子进程 ⇄ underhill_core 同 mount namespace → operator 起的 `/tmp/X.sock` 对 underhill 可见。
- **POC-2/3**：usnvmemu setsid 脱离 run-shell 持久 listening；另一进程（underhill proxy）跨独立 run invocation connect + 全握手 + dma_map + Identify 零拷贝端到端 PASS。
- **POC-4**：kill usnvmemu → socket 残留 → connect=**ECONNREFUSED(111)**；relaunch(rm+rebind)→fresh connect PASS。→ ENOENT/ECONNREFUSED/握手失败三类都当"重试"。
- **审计事实（ROUND 1，pci_core/vpci 源码）**：① pci_core 构造即定（BAR/identity/MSI-X 运行时不可改）；② VPCI offer/revoke 仅 host-owned VF bus（非 build_vpci_device 建的 emulated 设备）；③ pcie_remote = 槽位恒在 + cfg-read-error 表达 Live/Lost + transport_swap 重连（非 guest hotplug）。
- **未验证**（实现期验）：underhill reconnect 代码本身（standalone loopback + 真机）。

## 3. 用户决策（收敛点）
- **(a) "完整"= 复活已枚举 function**：usnvmemu 在 guest PCI 枚举窗口内起 → guest 枚举+真 IO；运行时停/重起/换二进制 → 已绑 function 走 Lost→Live revive。**at-boot-absent 设备的 guest 冷插归 Layer C（未来）**。
- **(b) actual > declared 策略**：usnvmemu 实际 BAR0 size / MSI-X count **超过** underhill 声明窗口时，连接器 **stay-Lost + 响亮报错**，不转 Live（防 guest 拿到半映射控制器）。
- **(c) 声明 geometry**：hardcode 默认（从 usnvmemu 实际固定值，与 firmware 一起 commit）+ **CLI override**（`,bar0=<n>,msix=<n>`）。

## 4. 架构（Layer A：slot-always-declared + client reconnect）

唯一 as-is 平台可行的"guest 真用上"单元。在已交付 `vfio_user_pci_device` crate 内演进（非新 crate）。

### 4.1 resolver — assemble-always
每个**已配置**实例（env 设了才有；空 CLI→不注册→零变化，boot-safety 不退）：**总是**构造完整 `VfioUserPciDevice`（worker + irq tasks + cfg_space 用**声明 geometry**），让 reconnect 有 worker 可换。`AbsentPcieDevice` 缩为仅 **assembly-failure 兜底**（set_irqs/PolledWait 构造失败）。CVM-gated 不变。

### 4.2 device — show-absent-until-Live（CRITICAL C-2）
`pci_cfg_read`：`Lost` **和 `Connecting`** 都返 `Err(InvalidRegister)`（VPCI 层 fill(!0)，guest 看"function 不存在"）；仅 `Live` 走真 `cfg_space.read_u32`。→ 没 usnvmemu 时 guest 看不到设备（等同今 AbsentPcieDevice 的 boot-safety），usnvmemu 真连上 Live 后才呈现真 cfg。MMIO 同理：非 Live 一律 Err。
> 防的是：`Connecting` 走真 cfg → guest 绑 nvme 驱动到没后端的死控制器 → OS hang（291d8645 类）。

### 4.3 reconnect 连接器（新 `reconnect` 模块，每实例一 task，`!is_hardware_isolated()` 门控）
持久 loop（句柄入 keepalive Arc，非裸 detach）：
```
loop {
  let client = retry_connect(&path).await;            // ENOENT/ECONNREFUSED/IO → backoff 100ms→2s 无总 cap (POC-4)
  let prep = match prepare_from_client(client).await { // 握手+identity，在连接器自己的 serial client 上（round-trip 安全，保 B-1）
    Ok(p) => p, Err(_) => { backoff; continue }        // 握手失败 → 重试
  };
  match validate_identity(&prep, &declared) {          // 决策(b)：
    Ok(()) => {}                                       //   drift(vendor/class)=log+continue
    Err(Exceeds) => { error!; backoff; continue }      //   actual BAR0/MSIX > declared → stay-Lost+重试，不转 Live
  }
  reissue_set_irqs(&mut prep.client, &persisted_eventfds).await?; // CRITICAL C-3：新 client 重发 set_irqs（同一组持久 eventfd）
  let (writer, reader) = prep.client.into_channel();
  if reconnect_tx.send(Connected { writer, reader }).is_err() { return } // worker 没了
  if lost_rx.next().await.is_none() { return }         // 阻塞至本连接 wire 死；worker 没了则退出
}
```
- **持久 eventfd**：irq 的 `pal_event::Event` 由 device 生命周期持有；连接器持其 `BorrowedFd` 以便每次重连重发 set_irqs（否则 revive 后 MSI-X 死）。
- backoff 内部常量（100ms→2s），不暴露；boot 期总 grace timeout 仍走 CLI `handshake_timeout_ms`。

### 4.4 worker — 加一个 swap arm + 边沿 lost（CRITICAL C-1 + IMPORTANT）
- `reconnect_rx: mesh::Receiver<ReconnectEvent>`（MPSC，单消费者=worker）。新 arm：
  ```
  ev = self.reconnect_rx.next() => match ev {
    Some(Connected { writer, reader }) => {
      self.drain_in_flight();      // sync：旧 in_flight token 全 complete_error(NoResponse) 恰一次
      self.writer = writer;        // 两半一条消息携入，两赋值间【无 .await】(C-1：防新 writer 配旧死 reader)
      self.reader = reader;
      self.state.store(Live);      // next_msg_id 不重置（跨重连单调，镜像 pcie_remote）
    }
    None => break,                 // 连接器 drop = 进程退出
  }
  ```
- `go_lost`：**边沿触发** —`if try_transition(Live→Lost) || try_transition(Connecting→Lost) { lost_tx.send(()) }`；drain 总跑，但 lost 通知**一次/次断**（防 level-triggered 双重连 flap）。wire EOF/err（reader recv 或 writer send 失败）→ go_lost。Lost 期 read arm 走 `pending`（不 busy-loop 死 socket，镜像 pcie_remote `read_inbound_or_pending`）。

### 4.5 identity / 声明 geometry（决策 c）
- 声明默认 = usnvmemu 实际固定值：vendor `0x1414` / class `01:08:02` / BAR0 实际 NVMe 寄存器窗口 / MSI-X 实际向量数（常量，与 firmware 一起 commit；non-arbitrary）。
- CLI override：`OPENHCL_VFIO_USER_NVME=<guid>:<path>[,bar0=<n>][,msix=<n>][,handshake_timeout_ms=N]`（对齐 pcie_remote `,key=val` 方案：entry 先 `:` 分 guid/rest，rest 按 `,` 分 path + kv 选项）。
- `validate_identity`（决策 b）：连接器首/每次连上读真 identity 比对 declared——vendor/device/class drift → log warn + 继续 Live；**actual BAR0 size / MSI-X count > declared → stay-Lost + error**（不转 Live）；actual ≤ declared → OK。

## 5. 三个 CRITICAL 不变量（必须进实现，happy-path harness 看不见 — [[dbbuf-shadow-doorbell-complete]] §29）
1. **C-1 pair-swap 原子**：writer+reader 一条 `Connected` 消息携入；worker 两赋值间无 `.await`。
2. **C-2 show-absent-until-Live**：`Connecting` 在 cfg/MMIO 面等同 `Lost`，直到首个 Live。
3. **C-3 reconnect 重发 set_irqs**：每次重连用持久 eventfd 对新 client 重发，再 into_channel。

## 6. 数据流（四场景）
- **boot, usnvmemu 已起**：resolver assemble（Connecting，cfg=absent）→ 连接器连上 → 重发 set_irqs → 投 (writer,reader) → worker Live → guest 枚举（此刻 cfg 真）+ 真 IO。（依赖 boot grace poll 把首连拉进枚举窗口前。）
- **boot, usnvmemu 未起**：assemble（Connecting=absent）→ 连接器持续重试 → guest 枚举时看"不存在"（boot-safe）。usnvmemu 后起 → Live，但 guest 已过枚举不会自动重扫（= 决策 a：冷插归 Layer C）。
- **运行时 usnvmemu 停**：wire EOF → worker go_lost（边沿）→ drain in_flight → device Lost（已绑 guest function 的 MMIO 报错）→ 连接器收 lost → 回重试。
- **运行时 usnvmemu 重起/换二进制**：连接器重连 → 重发 set_irqs → 投新两半 → worker drain+swap+Live（revive）→ 已绑 function 恢复工作。

## 7. 错误处理 + boot-safety 调和
- assemble-always（reconnect 有 worker 可换）+ show-absent-until-Live（无 usnvmemu = guest 看不存在，等同今 PASS）+ env-gate 不变（空 CLI→不注册）+ CVM-gate 不变（也门控连接器）。**保 realize-only PASS 同时使能 revive**。
- 三类 connect 失败（ENOENT/ECONNREFUSED/握手）+ identity-exceeds → 重试/stay-Lost，不挂 boot、不 panic。

## 8. 测试
- **standalone loopback（核心，可 Linux 跑）**：真 `vfio_user_transport` server。验：① 连上→Live→MMIO 工作；② server drop（模拟 usnvmemu 停）→ worker Lost + in_flight drain（token NoResponse）+ 边沿 lost 一次；③ server 重起 → swap → revive Live + MMIO 再工作；④ pair-swap 原子（并发 in_flight read 跨 swap 全 NoResponse 不串）；⑤ identity-exceeds → stay-Lost。
- **真机里程碑**：operator 起 usnvmemu（POC harness 范式）→ guest 枚举 `OpenHCL Userspace NVMe` + 真 NVMe IO；停 → guest function 报错；重起 → revive。host backing-file 独立 oracle。

## 9. Layer A 构建顺序（每段 coarse commit + 真机/loopback 验 + subagent review）
- A1：device show-absent-until-Live（Connecting→absent）+ resolver assemble-always + 声明 geometry/CLI override 解析 + validate_identity。standalone 单测。
- A2：reconnect 模块 + worker swap arm + 边沿 lost + 持久 eventfd 重发 set_irqs。standalone loopback（场景①②③④⑤）。
- A3：underhill 集成（连接器接 resolver/worker keepalive）+ 真机里程碑（operator 起 usnvmemu → guest 枚举+IO+revive）。

## 10. 未来（明确不在本设计）
- **Layer C 真 hotplug**：让 build_vpci_device 的 VpciBus 可控（detach/re-attach + 重触 offer），实现 at-boot-absent 设备的 guest 冷插。先 POC 可控 VpciBus 可行性。
- **learned 作独立模式**：被 Q1 死锁（真运行时学习不可能）；本设计的 declare-then-validate 已覆盖其安全子集。
- **usnvmemu 独立托管服务**：保留为 future 实验项目。

## 11. 风险
- reconnect 并发正确性（C-1/C-2/C-3 + 边沿 lost）是主风险 → standalone deterministic loopback 覆盖 + subagent review（非 happy-path）。
- declared geometry 必须真等于 usnvmemu 实际几何（决策 b 的 stay-Lost 是安全网，但默认值错会让所有实例 stay-Lost）→ A1 用真 usnvmemu 几何校准 + 文档化。
