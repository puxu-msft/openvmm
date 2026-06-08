# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""vfio-user enumerate smoke — 真跨进程验证 server 的 PCI 枚举 + W1 cfg-space。

自包含：build + spawn nvme_firmware vfio-user server（UNIX socket）→ 以真
vfio-user 客户端身份连入 → 跑 VERSION/GET_INFO/GET_REGION_INFO/GET_IRQ_INFO/
REGION_READ 等真 wire 命令 → 校验 → 退出码 0 = 全通过。

验证重点（loopback Rust 单测看不到的跨进程 wire 正确性）：
- **W1 cfg-space 真路由**：guest（本 client）REGION_READ CONFIG region 拿到正确
  vendor/device/class（此前 bug：CONFIG 被误当 BAR MMIO，读不到 identity）。
- **描述模型统一**：GET_REGION_INFO / GET_IRQ_INFO 从 describe() 派生的 BAR0
  size / MSI-X count。
- **BAR size-probe + FLR**：写全 1 回读 size mask；DEVICE_RESET 后 config 复位。

用法：`uv run python enumerate_smoke.py`（自动 build + spawn server）。
"""
from __future__ import annotations

import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import vfio_proto as v

HERE = Path(__file__).resolve().parent
NVME_DIR = (HERE / ".." / ".." / ".." / "nvme_firmware").resolve()

# NvmeController 默认 describe() 值（main.rs 默认 --vid 0x1414；describe device_id
# 0xc0de / class 01.08.02 / BAR0 8 KiB / MSI-X 4）。
EXP_VENDOR = 0x1414
EXP_DEVICE = 0xC0DE
EXP_BAR0_SIZE = 8192
EXP_MSIX = 4
EXP_NUM_REGIONS = 9
EXP_NUM_IRQS = 5


def log(msg: str) -> None:
    print(f"[enumerate_smoke] {msg}", flush=True)


def check(cond: bool, what: str) -> None:
    if not cond:
        raise AssertionError(f"FAIL: {what}")
    log(f"  ✓ {what}")


def build_server() -> Path:
    log("building nvme_firmware (vfio-user) ...")
    r = subprocess.run(
        ["cargo", "build", "--bin", "nvme_firmware"],
        cwd=NVME_DIR,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        raise RuntimeError("cargo build failed")
    binary = NVME_DIR / "target" / "debug" / "nvme_firmware"
    if not binary.exists():
        raise RuntimeError(f"binary not found: {binary}")
    return binary


def wait_for_socket(path: Path, proc: subprocess.Popen, timeout: float = 10.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            return
        if proc.poll() is not None:
            raise RuntimeError(f"server exited early (code {proc.returncode})")
        time.sleep(0.05)
    raise RuntimeError(f"socket {path} not created within {timeout}s")


def run_scenarios(sock: socket.socket) -> None:
    mid = 1

    # 1) VERSION 握手
    v.version_handshake(sock, mid)
    log("VERSION handshake OK")
    mid += 1

    # 2) DEVICE_GET_INFO
    info = v.get_info(sock, mid)
    check(info["num_regions"] == EXP_NUM_REGIONS, f"num_regions={EXP_NUM_REGIONS}")
    check(info["num_irqs"] == EXP_NUM_IRQS, f"num_irqs={EXP_NUM_IRQS}")
    mid += 1

    # 3) GET_REGION_INFO — CONFIG (size 4096, R|W)
    cfg = v.get_region_info(sock, mid, v.REGION_CONFIG)
    check(cfg["size"] == 4096, "CONFIG region size = 4096")
    check(cfg["flags"] & v.REGION_FLAG_READ != 0, "CONFIG region READable")
    mid += 1

    # 4) GET_REGION_INFO — BAR0（describe() 派生 size = 8 KiB）
    bar0 = v.get_region_info(sock, mid, v.REGION_BAR0)
    check(bar0["size"] == EXP_BAR0_SIZE, f"BAR0 size = {EXP_BAR0_SIZE} (describe-derived)")
    mid += 1

    # 5) REGION_READ CONFIG @0x00 4B → vendor/device（**W1 cfg-space 真路由**）
    ident = v.region_read(sock, mid, v.REGION_CONFIG, 0x00, 4)
    vendor, device = struct.unpack("<HH", ident)
    check(vendor == EXP_VENDOR, f"CONFIG vendor_id = {EXP_VENDOR:#06x}")
    check(device == EXP_DEVICE, f"CONFIG device_id = {EXP_DEVICE:#06x}")
    mid += 1

    # 6) REGION_READ CONFIG @0x08 4B → revision + class code
    #    (REGION read count 须 ∈ {1,2,4,8}；4B 一次拿 rev/prog-if/sub/base)
    cc = v.region_read(sock, mid, v.REGION_CONFIG, 0x08, 4)
    check(cc[0] == 0x01, "CONFIG revision = 1")
    check(
        cc[1] == 0x02 and cc[2] == 0x08 and cc[3] == 0x01,
        "CONFIG class code = 01.08.02 (NVMe)",
    )
    mid += 1

    # 7) GET_IRQ_INFO — MSI-X count（describe() 派生）
    irq = v.get_irq_info(sock, mid, v.IRQ_MSIX)
    check(irq["count"] == EXP_MSIX, f"MSI-X count = {EXP_MSIX} (describe-derived)")
    mid += 1

    # 8) BAR0 size-probe：写全 1 → 回读 size mask（8 KiB → 0xFFFFE000）
    v.region_write(sock, mid, v.REGION_CONFIG, 0x10, struct.pack("<I", 0xFFFFFFFF))
    mid += 1
    bar_mask = struct.unpack("<I", v.region_read(sock, mid, v.REGION_CONFIG, 0x10, 4))[0]
    check(bar_mask == 0xFFFFE000, "BAR0 size-probe mask = 0xFFFFE000 (8 KiB)")
    mid += 1

    # 9) DEVICE_RESET (FLR) → config 复位：BAR0 回 0
    v.device_reset(sock, mid)
    mid += 1
    bar_after = struct.unpack("<I", v.region_read(sock, mid, v.REGION_CONFIG, 0x10, 4))[0]
    check(bar_after == 0x0, "FLR 后 BAR0 base 复位为 0")
    mid += 1


def main() -> int:
    binary = build_server()
    tmpdir = tempfile.mkdtemp(prefix="vfio_enum_")
    sock_path = Path(tmpdir) / "nvme.sock"
    img_path = Path(tmpdir) / "ns1.img"
    with open(img_path, "wb") as f:
        f.truncate(1024 * 1024)  # 1 MiB backing

    env = dict(os.environ, RUST_LOG="warn")
    log(f"spawning server: {binary.name} --vfio-user-sock {sock_path}")
    proc = subprocess.Popen(
        [str(binary), "--vfio-user-sock", str(sock_path), "--backing-file", str(img_path)],
        env=env,
    )
    try:
        wait_for_socket(sock_path, proc)
        log("server socket up; connecting as vfio-user client")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(5.0)
            sock.connect(str(sock_path))
            run_scenarios(sock)
        log("ALL SCENARIOS PASSED ✓")
        return 0
    except Exception as e:  # noqa: BLE001 — harness 顶层捕获，打印 + 非 0 退出
        log(f"FAILED: {e}")
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
        # best-effort cleanup
        for p in (sock_path, img_path):
            try:
                p.unlink()
            except OSError:
                pass
        try:
            os.rmdir(tmpdir)
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
