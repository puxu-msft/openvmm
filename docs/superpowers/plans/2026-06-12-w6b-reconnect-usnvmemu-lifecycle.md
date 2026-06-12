# W6b reconnect — usnvmemu 启停灵活性 实现计划（Layer A）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 underhill 持久 reconnect 到 operator 自由启停/换二进制的 usnvmemu，guest 看到的 NVMe 设备随 usnvmemu 生灭走 Lost↔Live revive（已枚举 function 范畴）。

**Architecture:** 在已交付的 `vfio_user_pci_device` crate 内演进。device 在非-Live 态对 guest 呈"不存在"（show-absent-until-Live）；resolver 总装配设备（assemble-always）用声明 geometry；一个持久 `reconnect` 连接器 task 主动连 usnvmemu、握手、重发 set_irqs、把全双工 `(writer,reader)` 经通道交给 worker；worker 加一个 swap arm 收新连接并 revive，wire 死时边沿通知连接器重连。

**Tech Stack:** Rust / pal_async (PolledSocket/Task/PolledWait) / pal_event::Event (eventfd) / mesh channels / pci_core (ConfigSpaceType0Emulator/MsixEmulator) / vfio_user_wire 协议。

**前序已交付**：Phase 1 设备 crate (`733459d0`) + Phase 2 underhill 集成 (`0fb8e8b0`) + realize-only 真机 PASS (`57632f6d`)。
**Spec**：`docs/superpowers/specs/2026-06-12-w6b-reconnect-usnvmemu-lifecycle-design.md`。
**审计/POC 轨迹**：`usnvmemu/experiments/2026-06-12-w6b-reconnect-poc/`（DESIGN-DRAFT.md + RESULT.md）。

**3 个 CRITICAL 不变量（贯穿全程，happy-path 看不见）**：
- **C-1 pair-swap 原子**：writer+reader 一条消息携入 worker，两赋值间无 `.await`。
- **C-2 show-absent-until-Live**：`Connecting` 在 cfg/MMIO 面等同 `Lost`，直到首个 `Live`。
- **C-3 reconnect 重发 set_irqs**：连接器持 `pal_event::Event` 的 **owned clone**（非 BorrowedFd），每次重连对新 client 重发 set_irqs 再 into_channel。

---

## 文件结构（创建/修改 + 职责）

| 文件 | 动作 | 职责 |
|------|------|------|
| `vm/devices/pci/vfio_user_pci_device/src/device.rs` | 修改 | C-2：`Connecting` cfg/MMIO 等同 `Lost`。 |
| `vm/devices/pci/vfio_user_pci_device/src/identity.rs` | 创建 | 声明 geometry 常量 + `DeclaredGeometry` + `validate_identity`（决策 b）。 |
| `vm/devices/pci/vfio_user_pci_device/src/worker.rs` | 修改 | reconnect-capable worker：`writer/reader` 改 `Option`；加 swap arm + 边沿 `go_lost`(lost_tx)；`Worker::new` 不再要求初始连接。 |
| `vm/devices/pci/vfio_user_pci_device/src/reconnect.rs` | 创建 | 持久连接器 task：connect→prepare→validate→重发 set_irqs→into_channel→投 Connected→等 lost。 |
| `vm/devices/pci/vfio_user_pci_device/src/resolver.rs` | 修改 | assemble-always（声明 geometry 建 cfg_space）+ eventfd clone + 建两条通道存共享 map + spawn 连接器。 |
| `vm/devices/pci/vfio_user_pci_device/src/lib.rs` | 修改 | 导出 `identity` / `reconnect` / `ReconnectChannels` 等。 |
| `openhcl/underhill_core/src/options.rs` | 修改 | `VfioUserNvmeCliConfig` 加 `bar0`/`msix` 可选 override + `,key=val` 解析。 |
| `openhcl/underhill_core/src/worker.rs` | 修改 | boot grace poll 改判 `SharedState==Live`；连接器/通道 map 接线。 |

---

## Layer 分层与详度（[[handoff-needs-full-prompt-plus-plan]] 混合详度纪律）
- **A1 / A2 = execution-ready（no-placeholder，Linux standalone 可测）**：直接逐步执行。
- **A3 = execution-ready 步骤但 GATE 在真 VM**：依赖 A1+A2 落地 + 需 IGVM 重建 + 真 Hyper-V VM（复用 POC harness）。其 underhill 接线镜像已 proven 的 Phase 2 + pcie_remote 模式。

---

## Task A1.1: device.rs — `Connecting` 等同 `Lost`（C-2 show-absent-until-Live）

**Files:**
- Modify: `vm/devices/pci/vfio_user_pci_device/src/device.rs:130-145`（cfg）+ `:148-226`（mmio 已是 Lost-gated，确认 Connecting 也 gated）
- Test: 同文件 `#[cfg(test)] mod tests`

- [ ] **Step 1: 写失败测试**（Connecting 时 cfg_read 返 Err，不泄露真 identity）

在 `device.rs` tests 模块加：
```rust
    #[test]
    fn connecting_cfg_read_returns_err_like_lost() {
        let mut dev = build_test_device(DeviceState::Connecting);
        let mut v = 0u32;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidRegister)),
            "Connecting 必须对 guest 呈不存在（C-2），got {r:?}"
        );
    }

    #[test]
    fn connecting_mmio_read_returns_err() {
        let mut dev = build_test_device(DeviceState::Connecting);
        let mut buf = [0u8; 4];
        let r = <VfioUserPciDevice as MmioIntercept>::mmio_read(&mut dev, 0x4000_0000, &mut buf);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p vfio_user_pci_device connecting_ -- --nocapture`
Expected: FAIL（当前 `Connecting` 走真 `cfg_space.read_u32` → 返 Ok，断言不满足）

- [ ] **Step 3: 改实现**——`pci_cfg_read` / `pci_cfg_write` 把 `Connecting` 并入 `Lost` 分支

`device.rs:130-144` 改为：
```rust
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        match self.state.load() {
            // C-2：Connecting 与 Lost 都对 guest 呈"不存在"（cfg 返 Err → VPCI fill !0），
            // 仅 Live 走真 cfg。防 guest 在 usnvmemu 真连上前绑驱动到死控制器（291d8645 类 hang）。
            DeviceState::Connecting | DeviceState::Lost => IoResult::Err(IoError::InvalidRegister),
            DeviceState::Live => self.cfg_space.read_u32(offset, value),
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match self.state.load() {
            DeviceState::Connecting | DeviceState::Lost => IoResult::Ok, // 静默丢弃
            DeviceState::Live => self.cfg_space.write_u32(offset, value),
        }
    }
```
MMIO：`mmio_read`/`mmio_write` 当前是 `if matches!(self.state.load(), DeviceState::Lost) { return Err(InvalidRegister) }`。改成同时拦 Connecting：
```rust
        if !matches!(self.state.load(), DeviceState::Live) {
            return IoResult::Err(IoError::InvalidRegister);
        }
```
（`mmio_read` 与 `mmio_write` 开头各一处。）

- [ ] **Step 4: 跑测试确认通过 + 不退化**

Run: `cargo test -p vfio_user_pci_device 2>&1 | tail -20`
Expected: 新 2 测 PASS；已有 device 测（`live_cfg_read_vendor_device` 等）仍 PASS（Live 路径不变）。

- [ ] **Step 5: 提交**（Phase 边界粗粒度——见末尾"提交策略"，本 task 先不单独 commit）

---

## Task A1.2: options.rs — CLI 加 `bar0`/`msix` 可选 override（决策 c）

**Files:**
- Modify: `openhcl/underhill_core/src/options.rs`（`VfioUserNvmeCliConfig` + `FromStr` + `parse_vfio_user_nvme_entries`）
- Test: 同文件 `#[cfg(test)]`（若无则加）

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn vfio_user_nvme_cfg_parses_path_and_optional_geometry() {
        // 仅 path
        let c: VfioUserNvmeCliConfig = "00000000-0000-0000-0000-000000000001:/tmp/a.sock".parse().unwrap();
        assert_eq!(c.unix_path, "/tmp/a.sock");
        assert_eq!(c.bar0_size, None);
        assert_eq!(c.msix_count, None);
        // path + override
        let c2: VfioUserNvmeCliConfig =
            "00000000-0000-0000-0000-000000000001:/tmp/a.sock,bar0=16384,msix=4".parse().unwrap();
        assert_eq!(c2.unix_path, "/tmp/a.sock");
        assert_eq!(c2.bar0_size, Some(16384));
        assert_eq!(c2.msix_count, Some(4));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p underhill_core vfio_user_nvme_cfg_parses -- --nocapture`
Expected: FAIL（字段 `bar0_size`/`msix_count` 不存在 → 编译错）

- [ ] **Step 3: 改实现**——加字段 + 解析（对齐 pcie_remote 的 `,key=val`：entry 先 `:` 分 guid / rest，rest 按第一个 `,` 分 path + kv 选项）

`VfioUserNvmeCliConfig` 加字段：
```rust
    pub bar0_size: Option<u64>,
    pub msix_count: Option<u16>,
```
`FromStr::from_str` 改为：
```rust
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (guid_s, rest) = s.split_once(':').context("expected <guid>:<unix_path>[,k=v]")?;
        let instance_id: Guid = guid_s.parse().with_context(|| format!("invalid guid {guid_s}"))?;
        // path 到第一个 ',' 为止；其后是逗号分隔的 k=v 选项。unix 路径含 ',' 罕见。
        let mut it = rest.splitn(2, ',');
        let unix_path = it.next().unwrap_or("").to_string();
        if unix_path.is_empty() {
            anyhow::bail!("empty unix_path");
        }
        let mut bar0_size = None;
        let mut msix_count = None;
        let mut handshake_timeout_ms = 5000u32;
        if let Some(opts) = it.next() {
            for kv in opts.split(',') {
                let (k, v) = kv.split_once('=').with_context(|| format!("expected k=v: {kv}"))?;
                match k {
                    "bar0" => bar0_size = Some(v.parse().with_context(|| format!("bad bar0 {v}"))?),
                    "msix" => msix_count = Some(v.parse().with_context(|| format!("bad msix {v}"))?),
                    "handshake_timeout_ms" => handshake_timeout_ms = v.parse().with_context(|| format!("bad timeout {v}"))?,
                    _ => anyhow::bail!("unknown key: {k}"),
                }
            }
        }
        Ok(Self { instance_id, unix_path, handshake_timeout_ms, bar0_size, msix_count })
    }
```
（`UnderhillEnvCfg.vfio_user_nvme` 字段类型不变；`lib.rs` bridge 不变。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p underhill_core vfio_user_nvme_cfg_parses -- --nocapture`
Expected: PASS

- [ ] **Step 5:** 不单独 commit（Phase 末提交）

---

## Task A1.3: identity.rs — 声明 geometry + `validate_identity`（决策 b）

**Files:**
- Create: `vm/devices/pci/vfio_user_pci_device/src/identity.rs`
- Modify: `vm/devices/pci/vfio_user_pci_device/src/lib.rs`（`pub mod identity;` + 导出）

- [ ] **Step 1: 写失败测试**（先建文件含测试）

`identity.rs`：
```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 声明 geometry（usnvmemu 实际固定几何）+ identity 校验（决策 b）。
//!
//! usnvmemu identity 实际固定，故声明默认 hardcode；CLI 可 override（决策 c）。
//! 连接器每次连上读真 identity 比对：vendor/class drift → log+继续 Live；
//! **actual BAR0/MSI-X > declared → 拒绝（stay-Lost）**，防 guest 拿到半映射控制器。

/// usnvmemu 的固定 PCI 几何默认值（与 firmware 一起维护）。
pub const DEFAULT_VENDOR_ID: u16 = 0x1414;
/// NVMe class：base 0x01 / sub 0x08 / prog_if 0x02。
pub const DEFAULT_CLASS_BASE: u8 = 0x01;
pub const DEFAULT_CLASS_SUB: u8 = 0x08;
pub const DEFAULT_CLASS_PROGIF: u8 = 0x02;
/// usnvmemu 实际 BAR0 NVMe 寄存器窗口（A1 校准：见末尾"校准"步）。
pub const DEFAULT_BAR0_SIZE: u64 = 8192;
/// usnvmemu 实际 MSI-X 向量数。
pub const DEFAULT_MSIX_COUNT: u16 = 8;

/// underhill 声明给 guest 的几何（默认 + 可被 CLI override）。
#[derive(Clone, Copy, Debug)]
pub struct DeclaredGeometry {
    pub bar0_size: u64,
    pub msix_count: u16,
}

impl DeclaredGeometry {
    /// 用 CLI override（None=用默认）构造。
    pub fn new(bar0_override: Option<u64>, msix_override: Option<u16>) -> Self {
        Self {
            bar0_size: bar0_override.unwrap_or(DEFAULT_BAR0_SIZE),
            msix_count: msix_override.unwrap_or(DEFAULT_MSIX_COUNT),
        }
    }
}

/// usnvmemu 真报的几何（连接器读 region/irq info 得到）。
#[derive(Clone, Copy, Debug)]
pub struct ActualGeometry {
    pub bar0_size: u64,
    pub msix_count: u16,
}

/// 校验结果。
#[derive(Debug, PartialEq, Eq)]
pub enum IdentityCheck {
    /// 可转 Live（actual ≤ declared）。
    Ok,
    /// actual 超 declared → 拒绝，stay-Lost（决策 b）。
    Exceeds(&'static str),
}

/// 决策 b：actual BAR0 size 或 MSI-X count 超 declared → Exceeds（拒绝）。actual ≤ declared → Ok。
pub fn validate_identity(declared: &DeclaredGeometry, actual: &ActualGeometry) -> IdentityCheck {
    if actual.bar0_size > declared.bar0_size {
        return IdentityCheck::Exceeds("BAR0 size actual > declared");
    }
    if actual.msix_count > declared.msix_count {
        return IdentityCheck::Exceeds("MSI-X count actual > declared");
    }
    IdentityCheck::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_declared_ok() {
        let d = DeclaredGeometry::new(None, None);
        let a = ActualGeometry { bar0_size: d.bar0_size, msix_count: d.msix_count };
        assert_eq!(validate_identity(&d, &a), IdentityCheck::Ok);
        let a2 = ActualGeometry { bar0_size: d.bar0_size - 1, msix_count: d.msix_count - 1 };
        assert_eq!(validate_identity(&d, &a2), IdentityCheck::Ok);
    }

    #[test]
    fn bar0_exceeds_rejected() {
        let d = DeclaredGeometry::new(Some(8192), Some(8));
        let a = ActualGeometry { bar0_size: 16384, msix_count: 8 };
        assert!(matches!(validate_identity(&d, &a), IdentityCheck::Exceeds(_)));
    }

    #[test]
    fn msix_exceeds_rejected() {
        let d = DeclaredGeometry::new(Some(8192), Some(8));
        let a = ActualGeometry { bar0_size: 8192, msix_count: 16 };
        assert!(matches!(validate_identity(&d, &a), IdentityCheck::Exceeds(_)));
    }

    #[test]
    fn cli_override_applies() {
        let d = DeclaredGeometry::new(Some(16384), Some(4));
        assert_eq!(d.bar0_size, 16384);
        assert_eq!(d.msix_count, 4);
    }
}
```
`lib.rs` 加：`pub mod identity;` + `pub use identity::{DeclaredGeometry, ActualGeometry, IdentityCheck, validate_identity};`。

- [ ] **Step 2: 跑测试确认（先失败再通过）**

Run: `cargo test -p vfio_user_pci_device identity:: -- --nocapture`（首次应因模块未接进 lib 而编译失败，接好后 PASS）
Expected: 接好 lib.rs 后 4 测 PASS。

- [ ] **Step 3: 校准 DEFAULT_BAR0_SIZE / DEFAULT_MSIX_COUNT 为 usnvmemu 真值**

读 usnvmemu 真报几何（POC RESULT 已知 `num_irqs=5`(PCI 标准) 但 MSI-X **vector count** 来自 MockDev/真 firmware；W5a client 见 `num_regions=9`）。**确认 usnvmemu 真 BAR0 size + 真 MSI-X count**：
Run: `grep -rn "bar0\|BAR0\|msix_count\|bar_len\|0x2000\|8192" usnvmemu/crates/nvme_firmware/src/ | grep -i "size\|len\|count\|msix" | head`
据真值改 `DEFAULT_BAR0_SIZE` / `DEFAULT_MSIX_COUNT`。**若拿不准，A3 真机首跑会因 validate Exceeds 暴露**（安全网）；务必校准。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p vfio_user_pci_device identity:: 2>&1 | tail`
Expected: PASS

- [ ] **Step 5:** 不单独 commit。

---

## Task A2.1: worker.rs — reconnect-capable worker（C-1 swap arm + 边沿 lost）

**Files:**
- Modify: `vm/devices/pci/vfio_user_pci_device/src/worker.rs`
- Test: A2.4 的 loopback（本 task 只改结构 + 编译）

- [ ] **Step 1: 改 Worker 结构**——`writer/reader` 改 `Option`，加 reconnect 通道

`worker.rs` 顶部加 enum：
```rust
/// 连接器 → worker：携新连接的全双工两半（C-1：必须一条消息携两半）。
pub enum ReconnectEvent {
    Connected {
        writer: vfio_user_device::VfioUserWriter,
        reader: vfio_user_device::VfioUserReader,
    },
}
```
`Worker` 字段改：
```rust
    // C-1：两半要么都在（Live）要么都无（未连/Lost）。首连前为 None；swap 时一条消息整体替换。
    writer: Option<vfio_user_device::VfioUserWriter>,
    reader: Option<vfio_user_device::VfioUserReader>,
    // ... next_msg_id / in_flight / state / from_device / _interrupts / _irq_tasks / stats 不变 ...
    /// 连接器 → worker：新连接（swap input）。
    reconnect_rx: mesh::Receiver<ReconnectEvent>,
    /// worker → 连接器：本连接已死（边沿，一次/次断），让连接器重连。
    lost_tx: mesh::Sender<()>,
```
`Worker::new` 改签名（去掉 writer/reader 参数，加 reconnect_rx/lost_tx；首连前 writer/reader=None，初态由 resolver 传入的 state 决定——通常 Connecting）：
```rust
    pub fn new(
        state: SharedState,
        from_device: Receiver<DeviceRequest>,
        interrupts: Vec<Interrupt>,
        irq_tasks: Vec<pal_async::task::Task<()>>,
        stats: SharedWorkerStats,
        reconnect_rx: mesh::Receiver<ReconnectEvent>,
        lost_tx: mesh::Sender<()>,
    ) -> Self {
        Self {
            writer: None, reader: None, next_msg_id: 1, in_flight: HashMap::new(),
            state, from_device, _interrupts: interrupts, _irq_tasks: irq_tasks, stats,
            reconnect_rx, lost_tx,
        }
    }
```

- [ ] **Step 2: 改 run() 主循环**——加 swap arm；send/recv 处理 None；go_lost 边沿

`run()` 的 `select_biased!` 改（关键增量）：
```rust
            let is_lost = !matches!(self.state.load(), DeviceState::Live);
            select_biased! {
                _ = shutdown.next().fuse() => { /* 同前 */ break; }

                // C-1：新连接。一条消息携两半，两赋值间无 .await。
                ev = self.reconnect_rx.next().fuse() => {
                    let Some(ReconnectEvent::Connected { writer, reader }) = ev else {
                        // 连接器 drop = 进程退出。
                        break;
                    };
                    self.drain_in_flight();          // sync：旧在途全 NoResponse 恰一次
                    self.writer = Some(writer);      // 两赋值之间【无 await】(C-1)
                    self.reader = Some(reader);
                    // next_msg_id 不重置（跨重连单调）。
                    self.state.store(DeviceState::Live);
                    tracing::info!(CVM_ALLOWED, "vfio_user_pci: reconnected, Live");
                }

                req = self.from_device.next().fuse() => {
                    let Some(req) = req else { break };
                    // 非 Live（无 writer）时 device shim 不会转 MMIO 来（C-2），但防御性兜底：
                    let Some(writer) = self.writer.as_mut() else {
                        if let ReqKind::MmioRead { token, .. } = req.kind {
                            token.complete_error(IoError::NoResponse);
                        }
                        continue;
                    };
                    match req.kind {
                        ReqKind::MmioRead { bar, offset, size, token } => {
                            if !matches!(size, 1 | 2 | 4 | 8) { token.complete_error(IoError::InvalidRegister); continue; }
                            let id = self.alloc_msg_id();
                            self.in_flight.insert(id, InFlightRead { token, region: bar, offset, count: size as u32, size });
                            // ... stats 同前 ...
                            if let Err(e) = writer.send_region_read(id, bar, offset, size as u32).await {
                                self.go_lost(lost_reason::WRITE_ERR, &e);
                            }
                        }
                        ReqKind::MmioWrite { bar, offset, data } => {
                            let id = self.alloc_msg_id();
                            if let Err(e) = writer.send_region_write(id, bar, offset, &data).await {
                                self.go_lost(lost_reason::WRITE_ERR, &e);
                            }
                        }
                    }
                }

                inbound = recv_reply_or_pending_opt(self.reader.as_mut(), is_lost).fuse() => {
                    match inbound {
                        Some(Ok(reply)) => self.dispatch_reply(reply),
                        Some(Err(e)) => self.go_lost(lost_reason::READ_ERR, &e),
                        None => { /* 无 reader 或 Lost：pending 化，不该到这；防御性 noop */ }
                    }
                }
            }
```
`recv_reply_or_pending` 改成接受 `Option<&mut reader>`（无 reader 或 Lost → `pending()`）：
```rust
async fn recv_reply_or_pending_opt(
    reader: Option<&mut vfio_user_device::VfioUserReader>,
    is_lost: bool,
) -> Option<anyhow::Result<vfio_user_wire::framing::WireMessage>> {
    match (reader, is_lost) {
        (Some(r), false) => Some(r.recv_reply().await),
        _ => std::future::pending().await, // 无连接 / Lost：永远 pending（不 busy-loop）
    }
}
```
（注意：`select_biased!` 的 fuse future 持 `self.reader.as_mut()` 的可变借用跨 `.await`；因 swap arm 也借 `self`，需把 reader 借用限制在该 arm。实现时若借用冲突，用 `take`/局部变量或把 `recv` 逻辑内联进 arm 并在 arm 内 `self.reader.as_mut()`。**借用细节由实现者按编译器调整，语义不变**：无 reader/Lost → pending。）

- [ ] **Step 3: go_lost 改边沿触发**

```rust
    fn go_lost(&mut self, reason_bit: u64, err: &anyhow::Error) {
        // 边沿：仅 Live/Connecting → Lost 的转变才通知连接器一次（防 level-triggered 双重连 flap）。
        let edge = self.state.try_transition(DeviceState::Live, DeviceState::Lost).is_ok()
            || self.state.try_transition(DeviceState::Connecting, DeviceState::Lost).is_ok();
        tracing::warn!(CVM_ALLOWED, error = %err, reason_bit, edge, "vfio_user_pci: going Lost");
        self.stats.last_lost_at_ms.store(now_unix_ms(), Ordering::Relaxed);
        self.stats.last_lost_reason.store(reason_bit, Ordering::Relaxed);
        // 连接已死：丢弃两半（旧 reader 不再被 poll；防 C-1 torn-half）。
        self.writer = None;
        self.reader = None;
        self.drain_in_flight();
        if edge {
            let _ = self.lost_tx.send(()); // fire-and-forget；连接器收到后重连
        }
    }
```

- [ ] **Step 4: 编译确认**

Run: `cargo build -p vfio_user_pci_device 2>&1 | tail -20`
Expected: 编译通过（resolver 还没改完会有 `Worker::new` 调用处不匹配 → A2.3 一并修；本步若 resolver 报错属预期，先确保 worker.rs 本身无误：`cargo build -p vfio_user_pci_device --lib 2>&1 | grep -A3 "worker.rs"` 应无 worker.rs 内错误）。

- [ ] **Step 5:** 不单独 commit。

---

## Task A2.2: reconnect.rs — 持久连接器 task（C-3 重发 set_irqs）

**Files:**
- Create: `vm/devices/pci/vfio_user_pci_device/src/reconnect.rs`
- Modify: `lib.rs`（`pub mod reconnect;` + 导出 `ReconnectChannels` / `spawn_reconnect`）

- [ ] **Step 1: 写连接器 + 通道结构**

`reconnect.rs`：
```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 持久 reconnect 连接器 task（operator 拥有 usnvmemu，underhill 主动持久重连）。
//!
//! 每实例一个 task：connect → handshake+identity（在自己的 serial client，round-trip
//! 安全，保 B-1）→ validate_identity（决策 b）→ **重发 set_irqs（C-3，持久 eventfd
//! owned clone）** → into_channel → 投 `Connected{writer,reader}`（C-1 一条消息）→
//! 阻塞等 worker 的 lost 信号 → 重连。ENOENT/ECONNREFUSED/握手失败/Exceeds 皆 backoff 重试。
#![forbid(unsafe_code)]

use crate::identity::{ActualGeometry, DeclaredGeometry, IdentityCheck, validate_identity};
use crate::worker::ReconnectEvent;
use cvm_tracing::CVM_ALLOWED;
use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use std::os::fd::AsFd;
use std::time::Duration;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::pci_irq;

const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

/// 连接器持有的端 + 重发 set_irqs 所需资源（resolver 装配时构造）。
pub struct ReconnectChannels {
    /// 连接器 → worker：新连接。
    pub reconnect_tx: mesh::Sender<ReconnectEvent>,
    /// worker → 连接器：本连接死。
    pub lost_rx: mesh::Receiver<()>,
}

/// 持久连接器 loop。
///
/// `eventfds`：C-3——`pal_event::Event` 的 **owned clone**（与 irq task 持的原件指同一内核
/// eventfd），每次重连 `as_fd()` 重发 set_irqs。`declared`：决策 b 校验基准。
pub async fn reconnect_loop(
    driver: impl Driver + Clone,
    unix_path: String,
    declared: DeclaredGeometry,
    eventfds: Vec<pal_event::Event>,
    mut ch: ReconnectChannels,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        // 1. connect（ENOENT/ECONNREFUSED/IO 皆重试）。
        let mut client = match VfioUserClient::connect(&driver, &unix_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(CVM_ALLOWED, error = %e, path = %unix_path, "vfio_user_pci: connect 失败,backoff");
                if !sleep_backoff(&driver, &mut backoff).await { return }
                continue;
            }
        };
        // 2. handshake + identity（serial client；round-trip 安全）。
        if let Err(e) = client.handshake().await {
            tracing::warn!(CVM_ALLOWED, error = %e, "vfio_user_pci: 握手失败,backoff");
            if !sleep_backoff(&driver, &mut backoff).await { return }
            continue;
        }
        let actual = match read_actual_geometry(&mut client).await {
            Ok(a) => a,
            Err(e) => { tracing::warn!(CVM_ALLOWED, error = %e, "vfio_user_pci: 读 identity 失败,backoff");
                        if !sleep_backoff(&driver, &mut backoff).await { return } continue; }
        };
        // 3. validate（决策 b：Exceeds → 拒绝,stay-Lost,重试）。
        if let IdentityCheck::Exceeds(why) = validate_identity(&declared, &actual) {
            tracing::error!(CVM_ALLOWED, why, declared_bar0 = declared.bar0_size, actual_bar0 = actual.bar0_size,
                declared_msix = declared.msix_count, actual_msix = actual.msix_count,
                "vfio_user_pci: usnvmemu 几何超声明,拒绝转 Live（决策 b）");
            if !sleep_backoff(&driver, &mut backoff).await { return }
            continue;
        }
        // 4. C-3：重发 set_irqs（持久 eventfd owned clone 的 as_fd）。
        let fds: Vec<_> = eventfds.iter().map(|e| e.as_fd()).collect();
        if !fds.is_empty() {
            if let Err(e) = client.set_irqs(pci_irq::MSIX, 0, &fds).await {
                tracing::warn!(CVM_ALLOWED, error = %e, "vfio_user_pci: 重发 set_irqs 失败,backoff");
                if !sleep_backoff(&driver, &mut backoff).await { return }
                continue;
            }
        }
        // 5. split → 投 Connected（C-1 一条消息）。
        let (writer, reader) = client.into_channel();
        if ch.reconnect_tx.send(ReconnectEvent::Connected { writer, reader }).is_err() {
            return; // worker 没了
        }
        tracing::info!(CVM_ALLOWED, "vfio_user_pci: 已投递新连接,等 lost");
        backoff = BACKOFF_MIN; // 成功连上,重置 backoff
        // 6. 阻塞至本连接死（worker 边沿 lost）。
        if ch.lost_rx.next().await.is_none() {
            return; // worker 没了
        }
        tracing::info!(CVM_ALLOWED, "vfio_user_pci: 收到 lost,重连");
    }
}

/// 读 usnvmemu 真几何（BAR0 size + MSI-X count）。
async fn read_actual_geometry(client: &mut VfioUserClient) -> anyhow::Result<ActualGeometry> {
    use vfio_user_wire::proto::{pci_irq, pci_region};
    let bar0 = client.get_region_info(pci_region::BAR0).await?;
    let bar0_size = { let s = bar0.size; s }; // packed copy
    let irq = client.get_irq_info(pci_irq::MSIX).await?;
    let msix_count = { let c = irq.count; c as u16 };
    Ok(ActualGeometry { bar0_size, msix_count })
}

/// backoff sleep（指数,上限 BACKOFF_MAX）。返 false 表示 driver 不可用（罕见）→ 退出。
async fn sleep_backoff(driver: &(impl Driver + Clone), backoff: &mut Duration) -> bool {
    PolledTimer::new(driver).sleep(*backoff).await;
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
    true
}
```
`std::future::StreamExt` for `.next()`: 用 `futures::StreamExt`（mesh::Receiver impl Stream）。`use futures::StreamExt as _;`。
`lib.rs` 加 `pub mod reconnect;` + `pub use reconnect::{ReconnectChannels, reconnect_loop};`。

- [ ] **Step 2: 编译确认**

Run: `cargo build -p vfio_user_pci_device 2>&1 | tail -20`
Expected: reconnect.rs 编译通过（resolver 调用处 A2.3 接）。确认 `client.set_irqs`/`get_region_info`/`get_irq_info`/`into_channel`/`connect`/`handshake` 签名匹配（A1.3/Phase 1 已存在）。

- [ ] **Step 3:** 不单独 commit。

---

## Task A2.3: resolver.rs — assemble-always + eventfd clone + 接线连接器（C-3 owner）

**Files:**
- Modify: `vm/devices/pci/vfio_user_pci_device/src/resolver.rs`

- [ ] **Step 1: 改 assemble_device**——总建设备（声明 geometry）+ eventfd clone + 两通道 + spawn 连接器 + 改 Worker::new 调用

关键改动（替换现 `assemble_device` 体的连接相关部分）：
```rust
    // 声明 geometry（默认 + CLI override；决策 c）。
    let declared = crate::identity::DeclaredGeometry::new(handle.bar0_size, handle.msix_count);
    let msix_count = declared.msix_count;

    // MSI-X emulator（声明 count）。
    let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, msix_count, params.msi_target);
    let interrupts: Vec<Interrupt> = (0..msix_count).map(|i| msix.interrupt(i).expect("i<count")).collect();

    // DeviceBars：BAR0 声明 size（M-1 自动 64-bit）+ BAR4 MSI-X。
    let bars = DeviceBars::new()
        .bar0(declared.bar0_size, BarMemoryKind::Intercept(params.register_mmio.new_io_region("bar0", declared.bar0_size)))
        .bar4(msix.bar_len(), BarMemoryKind::Intercept(params.register_mmio.new_io_region("msix", msix.bar_len())));
    let cfg_space = ConfigSpaceType0Emulator::new(
        crate::identity::declared_hardware_ids(), vec![Box::new(msix_cap)], Vec::new(), bars);

    // C-3：eventfd 先 clone 给连接器,原件移入 irq tasks。
    let driver = params.driver_source.simple();
    let events: Vec<pal_event::Event> = (0..msix_count).map(|_| pal_event::Event::new()).collect();
    let connector_events: Vec<pal_event::Event> = events.iter().map(Clone::clone).collect();

    // irq wait tasks（原 events 移入）。
    let mut irq_tasks = Vec::new();
    for (i, event) in events.into_iter().enumerate() {
        let waiter = pal_async::wait::PolledWait::new(&driver, event).expect("PolledWait eventfd");
        irq_tasks.push(driver.spawn(format!("vfio_user_irq_{instance_id}_{i}"), crate::irq::irq_wait_loop(waiter, interrupts[i].clone())));
    }

    // 两条通道（C-1 reconnect + 边沿 lost）。
    let (reconnect_tx, reconnect_rx) = mesh::channel::<crate::worker::ReconnectEvent>();
    let (lost_tx, lost_rx) = mesh::channel::<()>();

    let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();
    let stats: SharedWorkerStats = Arc::new(Default::default());
    let state = SharedState::new(DeviceState::Connecting); // C-2：初态 Connecting=对 guest 不存在
    let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    std::mem::forget(shutdown_tx);

    // worker（reconnect-capable；无初始连接）。
    let worker = crate::worker::Worker::new(
        state.clone(), worker_inbox, interrupts, irq_tasks, stats.clone(), reconnect_rx, lost_tx);
    let wtask = driver.spawn(format!("vfio_user_worker_{instance_id}"), worker.run(shutdown_rx));
    worker_tasks.lock().push(wtask);

    // 持久连接器 task。
    let ch = crate::reconnect::ReconnectChannels { reconnect_tx, lost_rx };
    let ctask = driver.spawn(format!("vfio_user_connect_{instance_id}"),
        crate::reconnect::reconnect_loop(driver.clone(), handle.unix_path.clone(), declared, connector_events, ch));
    worker_tasks.lock().push(ctask);

    VfioUserPciDevice::new(state, to_worker, cfg_space, msix, stats).into()
```
加 `crate::identity::declared_hardware_ids()`（在 identity.rs 加）：
```rust
pub fn declared_hardware_ids() -> pci_core::spec::hwid::HardwareIds {
    use pci_core::spec::hwid::*;
    HardwareIds {
        vendor_id: DEFAULT_VENDOR_ID, device_id: 0x00a9, revision_id: 1,
        prog_if: ProgrammingInterface::from(DEFAULT_CLASS_PROGIF),
        sub_class: Subclass::from(DEFAULT_CLASS_SUB),
        base_class: ClassCode::from(DEFAULT_CLASS_BASE),
        type0_sub_vendor_id: 0, type0_sub_system_id: 0,
    }
}
```
**resolve_one：去掉 prepared-missing→Absent 的早返**（assemble-always；AbsentPcieDevice 仅 set_irqs/PolledWait 真失败兜底——本设计 set_irqs 移到连接器,resolver 不再 set_irqs,故 assembly 几乎不失败；PolledWait::new 失败仍可兜底）。`handle` 改用 `VfioUserNvmeHandle`（带 bar0_size/msix_count override 字段——A1.2 已加；需在 resources crate 的 handle 也加这两 Option 字段并经 mesh 传递）。

> **resources handle 字段**：`vm/devices/pci/vfio_user_pci_resources/src/lib.rs` 的 `VfioUserNvmeHandle` 加 `pub bar0_size: Option<u64>, pub msix_count: Option<u16>`（MeshPayload 自动）。underhill `vtl2_settings_worker.rs` push 时从 CLI cfg 填这两字段。

- [ ] **Step 2: 编译 + 接 prepared/spawn 退役**——删/改 `spawn.rs` 的一次性 connect（被 reconnect 取代）+ `prepared.rs` 的 PreparedMap（不再用）

`spawn.rs`：`prepare_from_client` 保留（reconnect 复用其 identity 读取逻辑？——实际 reconnect.rs 自带 read_actual_geometry；若 prepare_from_client 仅此处用则可删 `spawn_vfio_user_connects`，保留 `derive_hardware_ids_from_cfg` 若别处用）。`PreparedMap` 在 underhill 接线（A3）一并退役。本步先让 `cargo build -p vfio_user_pci_device` 通过。

Run: `cargo build -p vfio_user_pci_device 2>&1 | tail -25`
Expected: 通过（清掉 prepared/一次性 connect 的悬挂引用）。

- [ ] **Step 3:** 不单独 commit。

---

## Task A2.4: standalone loopback 测（场景①②③④⑤）— reconnect 的核心验证

**Files:**
- Create: `vm/devices/pci/vfio_user_pci_device/tests/reconnect_loopback.rs`

- [ ] **Step 1: 写 loopback 测**（真 `vfio_user_transport` server + 可重启）

要点（对标 `tests/worker_loopback.rs` 的 MockDev+spawn_server，但 server 可停可重起）：
- 用一个**真 AF_UNIX listener path**（tempdir），server 线程 accept+`server_handshake`+`VfioUserSession::pump`，可被信号停掉再重起（或起两次 listener 同 path）。
- 起 resolver-like 装配的精简版：直接构造 worker（reconnect-capable）+ reconnect_loop（连 tempdir path）+ device 的 from_device。
- 断言场景：① 连上→state Live + MmioRead 经 token 返 MockDev 值；② server 停（drop listener+session）→ worker go_lost → state Lost + 在途 token NoResponse + lost_tx 边沿一次；③ server 重起 → reconnect_loop 重连 → swap → state Live + MmioRead 再工作；④ swap 时并发 2 个在途 MmioRead → 全 NoResponse（不串到新连接）+ 旧 reader 已 drop；⑤ 声明 geometry 设很小（bar0=512）使 actual(8192) Exceeds → 连接器拒绝,state 保持非 Live。
- 因涉真 socket path + 多次 accept，用 `pal_async::DefaultPool::run_with` + server 单独 std::thread。

（完整测试代码在执行时按 `tests/worker_loopback.rs` 模式落地——server 线程 + DefaultPool client；本计划给出断言清单与结构，实现者按现有 loopback 范式补全具体 setup。**这是 A2 的 SHIP 判据。**）

- [ ] **Step 2: 跑测试**

Run: `cargo test -p vfio_user_pci_device --test reconnect_loopback 2>&1 | tail -30`
Expected: 5 场景全 PASS。

- [ ] **Step 3: clippy + 全 crate 测**

Run: `cargo test -p vfio_user_pci_device 2>&1 | tail; cargo clippy -p vfio_user_pci_device --all-targets 2>&1 | tail -5`
Expected: 全 PASS + clippy 0。

- [ ] **Step 4: A2 段 review + 提交**

派 rust-reviewer 独立审 worker swap arm + reconnect_loop（C-1/C-2/C-3 + 边沿 lost + 借用安全 + 无 B-1 死锁）。过后**粗粒度 commit A1+A2**（语义单元：reconnect 引擎 standalone）：
```bash
git add vm/devices/pci/vfio_user_pci_device vm/devices/pci/vfio_user_pci_resources openhcl/underhill_core/src/options.rs Cargo.lock
git commit -m "feat(vfio_user_pci): W6b reconnect Layer A1+A2 — 持久重连引擎 + show-absent-until-Live（standalone）"
```
（多会话共享树：`git commit -- <pathspec>` 限定；勿 `git add -A`；勿碰 DECISIONS.md。）

---

## Task A3: underhill 集成 + 真机里程碑 ⚠️ GATE：需 A1+A2 落地 + IGVM 重建 + 真 Hyper-V VM

> **execution-ready 但 gated**：以下步骤就绪，但**只能在真 VM 验证**（复用 `usnvmemu/experiments/2026-06-12-w6b-realize-only/` + `-reconnect-poc/` harness 范式）。underhill 接线镜像已 proven 的 Phase 2（commit `0fb8e8b0`）+ pcie_remote。

- [ ] **Step 1: underhill worker.rs 接线**——连接器/通道 map 已在 resolver 内自建（A2.3），underhill 侧只需：boot grace poll **改判 `SharedState==Live`**（替 prepared.len，因 assemble-always 无 prepared 交接）。具体：`openhcl/underhill_core/src/worker.rs:2438-2446` 的 poll 条件改为查该实例 device state 达 Live 或超时。（device state 经 inspect 或一个共享 Arc 暴露给 boot 线程；若不便，简化为固定短 grace sleep——guest 枚举窗口由 grace 决定。）
- [ ] **Step 2: vtl2_settings_worker.rs**——push `VfioUserNvmeHandle` 时填 `bar0_size`/`msix_count`（从 CLI cfg 的 Option 透传）。`PreparedMap`/`spawn_vfio_user_connects` 退役（A2 已删 crate 侧；underhill 侧删对应注册块的 prepared 部分,保留 CVM gate + env gate）。
- [ ] **Step 3: `cargo check -p underhill_core` + IGVM 构建**
  Run: `cargo check -p underhill_core 2>&1 | tail; cargo xflowey build-igvm x64 2>&1 | tail -5`
  （IGVM 若 ICE：`rm -rf target/openvmm_hcl/x86_64-unknown-linux-musl/debug/incremental` 重跑——见 memory）。
- [ ] **Step 4: 真机里程碑**（复用 POC harness）：装 IGVM + `OPENHCL_VFIO_USER_NVME=<guid>:/tmp/vfio_nvme.sock` → boot → operator 经 ohcldiag-dev setsid 起 usnvmemu（POC-2 范式,**校验二进制 size 防截断**）于枚举窗口前 → guest 枚举 `OpenHCL Userspace NVMe` + 真 NVMe IO（host backing-file 独立 oracle）→ `pkill -x fw`（停）→ guest function 报错/Lost → 重起 usnvmemu → revive Live + IO 恢复。归档进 `usnvmemu/experiments/2026-06-12-w6b-reconnect-real-vm/RESULT.md`。
- [ ] **Step 5: A3 review + 粗粒度 commit**（underhill 集成 + 真机 harness/RESULT）。

---

## 提交策略（[[commit-granularity-coarse-not-per-step]]）
按语义单元粗粒度提交，不逐 task：
- **commit 1**：A1+A2（reconnect 引擎 + show-absent + identity + CLI，standalone 全测过）。
- **commit 2**：A3（underhill 集成 + 真机 harness/RESULT）。
多会话共享树：每次 `git status` 看清 index + `git commit -- <pathspec>` 限定 + 不 `git add -A` + 不碰 `usnvmemu/docs/DECISIONS.md`。

## 每段 review（[[review-not-optional-self-consistent-trap]]）
A1 quality review；A2 **独立 rust-reviewer 审 C-1/C-2/C-3 + 边沿 lost + 借用 + 无死锁**（非 happy-path）；A3 architect/真机 oracle。commit 前必过。

## Self-review（spec 覆盖核对）
- §4.1 assemble-always → A2.3 ✓；§4.2 show-absent → A1.1 ✓；§4.3 连接器+C-3 → A2.2+A2.3 ✓；§4.4 swap arm+边沿 lost → A2.1 ✓；§4.5 geometry+validate → A1.2+A1.3 ✓；§5 三 CRITICAL → A2.1(C-1)+A1.1(C-2)+A2.2/A2.3(C-3) ✓；§6 四场景 → A2.4 ①②③④⑤ ✓；§8 测试 → A2.4 + A3.4 ✓；§9 构建序 → A1/A2/A3 ✓；§3 决策 → a(A3 milestone 范畴)+b(A1.3 validate)+c(A1.2 CLI+A1.3 DeclaredGeometry) ✓。
- 类型一致：`ReconnectEvent::Connected{writer,reader}`（worker.rs 定义 / reconnect.rs+resolver.rs 用）；`ReconnectChannels{reconnect_tx,lost_rx}`；`DeclaredGeometry`/`ActualGeometry`/`validate_identity`（identity.rs ↔ reconnect.rs）；`VfioUserNvmeHandle{...,bar0_size,msix_count}`（resources ↔ options ↔ resolver）一致。
