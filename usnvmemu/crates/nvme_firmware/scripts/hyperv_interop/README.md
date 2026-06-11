# Hyper-V / OpenHCL e2e harness (`hyperv_interop`)

The pcie_remote/OpenHCL counterpart to vfio-user's
[`qemu_interop`](../../../vfio_user_transport/scripts/qemu_interop/) and NVMe-oF's
[`interop_py`](../../../nvme_of_tcp_target/scripts/interop_py/). It drives the
**real, current** `nvme_firmware.exe` against a **real Hyper-V OpenHCL VM** and a
**real Windows guest**, exercising the full data path end to end and verifying it
with two independent oracles.

## What it verifies

```
nvme_firmware.exe (host, AF_HYPERV vsock client)
  → OpenHCL VTL2 pcie_remote worker → vpci publish to VTL0
  → guest nvme.sys → guest format + 4 MiB file IO (drives the controller PRP-list path)
  → NVMe WRITE back over the same path → host backing file
```

Two **independent** oracles must agree (per the project rule: never use a
self-consistent assumption as the wire/oracle criterion — use a genuinely
different code path):

1. **guest-side** — write 4 MiB embedding a unique `$MARKER`, `Write-VolumeCache`,
   read back, compare.
2. **host-side** — a raw byte-scan of the backing file for `$MARKER`, done by the
   ps1 *after stopping the controller* (its mmap holds the file lock, so a live
   reader gets IOException; the guest's NVMe FLUSH already made the bytes durable
   via `mmap.flush()`). A different code path than the guest readback (which could
   be served from the NTFS cache), so it confirms the bytes actually traversed the
   controller to the host file.

This is the **L3 (real Hyper-V vsock + guest)** verification. The committed
**L1 (TCP, Linux)** counterpart is the in-tree Rust harness
[`tests/openhcl_pcie_remote_e2e.rs`](../../tests/openhcl_pcie_remote_e2e.rs),
which plays the VTL2 side over TCP and drives `nvme_firmware --tcp-addr` through
handshake → Identify → 4K Format + IO + fused C&W without needing Windows.

### Verified milestone — 2026-06-10

Re-verified with the **current** binary (crate `nvme_firmware`, post-rename +
Phases F–S + Phase W). Guest enumerated `OpenHCL Userspace NVMe v2.0`, formatted
NTFS, wrote+read 4 MiB (`markerMatch=True`), the controller log showed the
PRP-list path (`NVM Write PRP-list … num_blocks=32 full_len=16384`), and the
host oracle found the marker in the raw backing file at byte offset **22,577,152**
(~9.2 MB non-zero in the 1 GiB image). See `usnvmemu/docs/pcie-remote-phase/`.

### Re-verified — 2026-06-11 (post-DBBUF)

Re-ran with the **current post-DBBUF** binary (shadow-doorbell feature `f35e5a71`
+ Phases through O5). Same dual-oracle **PASS**: guest enumerated `OpenHCL
Userspace NVMe v2.0`, format + 4 MiB write/readback (`markerMatch=True`), PRP-list
path (`num_blocks=256 full_len=131072`), host raw-backing oracle found the marker.
**Windows `nvme.sys` does not use DBBUF** — no `Doorbell Buffer Config` appears in
the controller log, only plain MMIO doorbells (offsets `0x1004`/`0x1008`/`0x100c`).
So this L3 run confirms the DBBUF changes did **not** regress the real-guest
non-DBBUF MMIO path. (DBBUF itself is exercised by the Linux-side e2e — vfio
`qemu_interop` + the OpenHCL `tests/openhcl_pcie_remote_e2e.rs` DBBUF tests — since
shadow doorbells are a Linux nvme-driver feature.) Reused the existing VM
(`pcie-remote-exp`); `BUILD=1` cross-rebuilt the firmware in 9.76s.

## Usage

One-time prerequisites (see [HYPERV_RUNBOOK.md](../../../../docs/pcie-remote-phase/HYPERV_RUNBOOK.md)):
- Windows 11 24H2+ with Hyper-V; the running account in **Hyper-V Administrators**.
- A provisioned `guest.vhdx` (Windows guest with the creds below) + an OpenHCL
  pcie_remote IGVM staged in the Windows working dir.
- Cross-build prereqs (see [`build-windows-cross.sh`](../../../../scripts/build-windows-cross.sh) header).

Run the whole thing from WSL:

```bash
# reuse the existing VM + already-staged exe:
usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/run_e2e.sh

# rebuild the current binary, stage it, recreate the VM from scratch:
BUILD=1 FORCE_DEPLOY=1 usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/run_e2e.sh
```

Exit 0 = both oracles agree. Env knobs: `BUILD`, `FORCE_DEPLOY`, `WIN_WORKDIR`,
`VM_NAME`, `ADMIN_USER`, `ADMIN_PASS` (see the script headers).

## Files

| File | Role |
|------|------|
| `run_e2e.sh` | WSL entry point: build+stage (opt) → drive ps1 → host byte oracle → verdict. |
| `run_hyperv_nvme_e2e.ps1` | Windows driver: deploy-or-reuse VM, start controller (`--retries 0`), guest IO oracle, controller log scan. |
| `build_and_stage.sh` | WSL: cross-build `nvme_firmware.exe` (`--features openhcl`) + copy to the Windows workdir. |

## Guest credentials (single source)

Creds come from one place (`-AdminUser`/`-AdminPass`, default
`Administrator`/`PcieRemote123!` — matching the baked `guest.vhdx`). The earlier
ad-hoc scripts split this (`Pass123!` vs `PcieRemote123!`), which silently
blocked PSDirect verification; the harness avoids that by threading a single
value through. Freshly provisioned guests should use `a`/`1`.

## Why `--retries 0` + restart

The OpenHCL VTL2 pcie_remote listener has a short handshake window (see RUNBOOK
K-19). The controller is started with `--retries 0` (infinite retry) **before**
the VM boots, so it is already waiting when the VTL2 listener appears. Reusing a
long-running VM requires a restart, because its listener times out shortly after
boot and the guest then has no device.

## Not yet automated

- A Windows+Hyper-V CI runner (same gap as `qemu_interop`).
- Advanced-feature guest assertions (multi-NS, PI-reject, shutdown) — the data
  path is covered; these are follow-ups now that creds are unified.
