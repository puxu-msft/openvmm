# OpenHCL + vfio-user NVMe (W6b/W6c) Hyper-V e2e harness

Drives the **real** OpenHCL `vfio_user_pci_device` (underhill client) + `usnvmemu`
(the `nvme_firmware` vfio-user server, running **inside VTL2**) against a **real**
Windows guest, and verifies the full zero-copy DMA data path end to end with two
independent oracles.

This is the **vfio-user-over-OpenHCL** counterpart to:

| harness | transport | server location |
|---|---|---|
| `nvme_firmware/scripts/hyperv_interop/` | pcie_remote (vsock) | host `nvme_firmware.exe` |
| `vfio_user_transport/scripts/qemu_interop/` | vfio-user | QEMU |
| **`hyperv_vfio_user_interop/` (this)** | **vfio-user** | **VTL2 `usnvmemu`** |

## What it proves

```
guest stornvme.sys -> VPCI -> underhill vfio_user_pci_device (client)
  -> REGION_RW (MMIO) + DMA_MAP(/dev/mshv_vtl_low fd via SCM_RIGHTS)
  -> usnvmemu (VTL2 server) zero-copy DMA of guest RAM -> backing file
```

- guest enumerates `PCI\VEN_1414&DEV_00A9` ("Standard NVM Express Controller", stornvme binds);
- an NVMe disk ("OpenHCL Userspace NVMe v2.0") appears;
- **oracle-1** (guest): 4 MiB write (embeds marker, > 8 KiB → PRP-list path) + flush + readback compare;
- **oracle-2** (host): independent raw byte-scan of the VTL2 backing file for the marker;
- **revive**: kill usnvmemu → device goes Lost → relaunch → reconnect → Live + DMA_MAP re-issued.

## Usage

```bash
# one-time: stage a *vpci* IGVM + guest.vhdx + ohcldiag-dev.exe + hyperv.psm1 in WIN_WORKDIR
#   cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci   # NB: vpci feature!
#   cp flowey-out/artifacts/build-igvm/debug/x64-custom/openhcl-x64-custom.bin <WIN_WORKDIR>/openhcl-vfio-user.bin

# run (reuses staged usnvmemu musl binary; BUILD_FW=1 to cross-build it first):
usnvmemu/crates/nvme_firmware/scripts/hyperv_vfio_user_interop/run_w6c_e2e.sh
```

Env knobs: `BUILD_FW`, `WIN_WORKDIR`, `VM_NAME`, `IGVM_NAME`, `INSTANCE`, `SOCK`,
`BACKING_MB`, `ADMIN_USER`/`ADMIN_PASS`, `SKIP_REVIVE`. PASS prints
`RESULT=PASS ... oracle2_offset=<byte>`.

## Hard-won correct methods (these are baked into the scripts — do NOT regress them)

Each of these cost a debugging round on real hardware. They are encoded in
`run_w6c_e2e.sh`; this section is the rationale so nobody "simplifies" them back.

1. **`ohcldiag-dev <vm> run -- sh -c '...'`** — the `--` is REQUIRED. Without it,
   ohcldiag's own clap parses `-c` as its flag → `error: unexpected argument '-c'`.
   Binary push = `base64 -w0 < bin | ohcldiag run -- sh -c 'base64 -d > /tmp/x ...'`.

2. **Liveness in VTL2: `ps | grep '[u]snvmemu'`**, never `pgrep`. busybox
   `pgrep -x usnvmemu` matches nothing (its `-x` quirk → false **DEAD**), and
   `pgrep -f "/tmp/usnvmemu"` matches the pgrep command's *own* cmdline → false
   **ALIVE**. Both bit us; only `ps` is trustworthy.

3. **oracle-2 backing scan: stream out with `ohcldiag-dev file -p | <host> grep`**,
   never `grep -a` on the binary **inside** VTL2. A newline-free 256 MB backing
   file is buffered by grep as one giant "line" → OOM in the 512 MB-RAM VTL2 →
   `oops=panic` → VTL2 reboots (this was "finding-5", a *harness* artifact, not a
   product bug — proven by differential: `file -p` succeeds on the same file that
   `grep` crashed). Streaming via `file -p` does the buffering on the host.

4. **Kill usnvmemu for the revive test: `pkill -f "/tmp/usnvmemu --vfio-user-sock"`**
   (by cmdline) or by pid — never `pkill -x usnvmemu` (busybox `-x` matches nothing,
   so the kill silently no-ops and you misread the device as "failing to go Lost").

5. **IGVM must be built WITH the vpci feature**:
   `cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci`. The default
   `build-igvm x64` omits it; once `OPENHCL_VFIO_USER_NVME` is set, underhill hits
   `bail!("built without vpci support")` and control_state sticks at "starting"
   (VTL2 diag still reachable + Hyper-V Running, so weak oracles falsely PASS —
   always check `ohcldiag-dev inspect control_state` / guest enumeration).

6. **No `OPENHCL_WAIT_FOR_START=1`** on this VM — it reliably trips a Hyper-V
   `VirtualizationException` at Start-VM (device-unrelated). We boot plain and bring
   usnvmemu Live early; the W6c C-2 fix makes the VPCI offer latch `DEV_00A9` at
   assemble regardless, and the disk comes up after a devnode disable/enable re-init.

7. **DMA `fd_offset` = bare guest GPA (mshv_vtl_low direct view)**, set by
   `underhill_mem` `GuestMemoryView::sharing()`. The guest's NVMe driver programs
   bare VTL0 GPAs in PRP/SGL (it has no knowledge of the VTL2 alias map), so the
   server maps `offset=gpa` directly — independent of whether the VTL0 alias map is
   active. (An earlier "alias-on → disable sharing" gate was wrong and produced
   0 DMA_MAP / no disk; the direct view works for alias on **and** off.)

## Known limitation

After repeated VTL2 reboots (e.g. from method-3 misuse), VTL2 can settle in a
degraded `control_state="started"` (not `"running"`). A clean `Stop-VM`/`Start-VM`
(which `run_w6c_e2e.sh` does on each run) restores a fresh VTL2.

## Result (2026-06-12)

PASS on `pcie-remote-exp`: DMA_MAP×2 `zero_copy=true` (bare gpa, = `memory_layout.ram()`
low/high segments, MMIO hole excluded), dma_read fail=0; guest disk Online; oracle-1
`markerMatch=True`; oracle-2 marker found at backing byte offset 22577152; revive
(2nd `reconnected, Live` + DMA_MAP re-issued). See
`usnvmemu/experiments/2026-06-12-w6c-dma-poc/RESULT.md`.
