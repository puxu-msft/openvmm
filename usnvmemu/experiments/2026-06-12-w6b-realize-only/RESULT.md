# W6b Phase 2 Task 2.4 — realize-only 真机验证 + boot-ordering 结论

**日期**：2026-06-12  **VM**：pcie-remote-exp（真 OpenHCL Hyper-V VM）
**IGVM**：本会话 `cargo xflowey build-igvm x64` 产物（含 W6b Phase 1 设备 crate + Phase 2
underhill_core 集成，commits `733459d0` + `0fb8e8b0`）。

## 跑
```bash
./run_realize_only.sh        # 装 IGVM + cmdline → 启动 → 验 VTL2 up + kmsg boot-safety markers
```

## 结果 — realize-only "boot 不挂" PASS ✓

设 `OPENHCL_VFIO_USER_NVME=<guid>:/tmp/vfio_nvme.sock` 但**不**起 firmware，真机 kmsg：

```
[0.738] vfio_user_pci_device::spawn: WARN vfio_user_pci: connect failed; backing off
        id=11111111-... error=connect vfio-user socket "/tmp/vfio_nvme.sock"
... (×32，每 ~100ms 一次 backoff 重试)
[3.957] vfio_user_pci_device::spawn: ERROR vfio_user_pci: per-instance connect attempt
        cap reached id=11111111-... attempts=0x20
[5.741] underhill_core::worker: INFO ...vfio_user_nvme: boot grace period done
        expected=0x1 got=0x0
```
+ VTL2 ohcldiag-dev 可达（~8s）+ `Get-VM` State=Running。

**证明**（Phase 1 + Phase 2 集成在真 OpenHCL VTL2 上 boot-safe）：
1. `options.rs` env 解析（cmdline `OPENHCL_VFIO_USER_NVME` → 解析出 instance + unix_path）✓
2. `worker.rs` connect spawner 在真 underhill 跑：retry + backoff + cap(32) ✓
3. `worker.rs` boot grace poll 完成（got=0，firmware 缺席）✓
4. **AbsentPcieDevice 兜底 → underhill boot 完成不挂**（boot grace done 这条 INFO 在 grace 之后打出 + VTL2 可达 + VM Running）✓

即 plan Phase 2 Task 2.4 Step 2「先 realize-only 确认 boot 不挂」达成。env-gated + CVM-gated +
AbsentPcieDevice 兜底的 boot 安全性有了真机证据。

## boot-ordering 结论 — guest 枚举（Step 3）需 VTL2-init/underhill supervisor 启 firmware

Step 3（guest `lspci` 枚举设备）要求 **firmware 在 underhill connect spawner 跑之前就
listening**。本会话验证了外挂 firmware 的两条路都**不行**：

- **首 boot 注入**：underhill connect 在 init 早期跑（~0.7–3.9s），此时 VTL2 diag server
  还没起（ohcldiag-dev `run` 要 ~6–8s 才可达）→ 无法在 connect 窗口内从外部起 firmware。
- **ohcldiag-dev run 起 firmware + `ohcldiag-dev restart`**：firmware 进程能起（listening
  on /tmp/vfio_nvme.sock），但 underhill worker `restart` **重置 VTL2 /tmp**（restart 后
  socket 消失、fw.log 清空，firmware 进程虽存活但 socket 在旧 namespace）→ 重启后的
  underhill connect 仍看不到 socket。

**根因**：firmware 与 underhill 必须在**同一 VTL2 namespace + 同一生命周期**，且 firmware
必须先于 underhill 设备注册 listening。外部 ohcldiag-dev 注入做不到。

**正确做法（下一子阶段，W5b/W6-class）**：firmware 由 **VTL2 init（initrd 内）或 underhill
自身（`Command::new` supervisor）** 启动 —— 把 firmware ELF 打进 IGVM/initrd，underhill
在 connect 前 spawn 它指向同一 socket。这保证同 namespace + 先启动，connect 才能命中。
（这也是 plan 里 reconnect/supervisor W6d 与 [[transport-maturity-imbalance]] OpenHCL 主战场
落地的自然延伸。）

## 备注
- 本验证复用并改造 W5a 部署机制（ohcldiag-dev + base64 stdin）：`build_and_stage` 思路同，
  但 W6b 的 client 是 **underhill_core 自身**（非 W5a 的外挂 test client）。
- VM `pcie-remote-exp` 的 FirmwareFile/cmdline 已被本验证改为 W6b IGVM；如需复跑 pcie_remote
  e2e，按 hyperv_interop harness 的 `FORCE_DEPLOY=1` 重新部署即可。

---

## ⚠️ 更正（2026-06-12，reconnect 真机调试时发现）

**本 realize-only "PASS" 是 false-PASS。** 它只验了 ① VTL2 可达（ohcldiag-dev run）+ ② Hyper-V State=Running，**没验 underhill `control_state==running` 或 guest 枚举**。

真相：`cargo xflowey build-igvm x64`（默认）**不带 vpci feature**（只 `--features gdb,tpm`）。一旦配了 `OPENHCL_VFIO_USER_NVME`，vtl2_settings 会把 handle push 进 vpci_devices，而 underhill `worker.rs` 在 `#[cfg(not(feature="vpci"))]` 下 `bail!("built without vpci support")` → worker_new ERROR → **control_state 卡 "starting"**。但 VTL2 diag server 仍起来（run 可达）+ Hyper-V 仍 Running，所以本 harness 的两个判据都"真"，漏报了 worker init 失败。

kmsg 实证：`underhill_core::worker: ERROR ... failed to start VM error=built without vpci support`。

**正确做法**：① IGVM 必须 `cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci`；② 真机判据必须查 `ohcldiag-dev inspect control_state`（应 `running`，非 `starting`）或 guest 枚举，**不能只查 VTL2 可达**。详见 `../2026-06-12-w6b-reconnect-real-vm/RESULT.md`。

（realize-only 验的"connect retry→cap→AbsentPcieDevice 兜底→boot 不挂"逻辑本身没错；但当时设备其实因缺 vpci feature 根本没装配，且 worker init 已 ERROR——只是没被这套判据发现。）
