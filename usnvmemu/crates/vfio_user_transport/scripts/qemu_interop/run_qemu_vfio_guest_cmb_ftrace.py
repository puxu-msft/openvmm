#!/usr/bin/env python3
# CMB SQ 回退点定位实验 harness —— run_qemu_vfio_guest_cmb.py 的 ftrace 变体。
#
# 目的：在真 QEMU guest 内用 ftrace/kretprobe 抓 Linux nvme 驱动 `nvme_alloc_sq_cmds`
# 的 CMB 回退判定点，区分回退原因 (a) pci_alloc_p2pmem 返 NULL / (b)
# pci_p2pmem_virt_to_bus 返 0 / (c) dev->cmb_use_sqes 本就 false。
#
# 与主 harness 的差异：用 guest_init_ftrace.sh 作 /init（在 insmod nvme 前布置
# kprobe），打一个**独立的** initramfs（不污染主 harness 缓存的 initramfs.cpio.gz）。
# firmware / QEMU 启动方式与 run_qemu_vfio_guest_cmb.py 完全一致（含 CMB_MODE、
# QEMU_IOMMU、GUEST_EXTRA_CMDLINE）。
#
# 用法：
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   CMB_MODE=trap python3 run_qemu_vfio_guest_cmb_ftrace.py
#
# 环境变量：CMB_MODE(trap|map) / QEMU_IOMMU / GUEST_EXTRA_CMDLINE / QEMU_BIN /
#           NVME_BIN / GUEST_KERNEL / KEEP_LOGS
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import run_qemu_vfio_guest as base

CMB_SIZE = 2 * 1024 * 1024
CMB_BIR = 2
HERE = Path(__file__).resolve().parent


def build_ftrace_initrd(workdir: Path) -> Path:
    """用 guest_init_ftrace.sh 作 /init 打一个独立 initramfs（复用主 harness 的
    busybox + nvme 模块缓存，但 init 换成带 ftrace 布置的版本）。"""
    cache = base.CACHE
    bb = cache / "busybox"
    if not bb.exists():
        # 触发主 harness 的 build_initramfs.sh 至少把 busybox 拉下来
        subprocess.run([str(HERE / "build_initramfs.sh")], check=True)
    root = workdir / "initroot"
    for d in ("bin", "proc", "sys", "dev", "tmp"):
        (root / d).mkdir(parents=True, exist_ok=True)
    shutil.copy(bb, root / "bin" / "busybox")
    os.chmod(root / "bin" / "busybox", 0o755)
    shutil.copy(HERE / "guest_init_ftrace.sh", root / "init")
    os.chmod(root / "init", 0o755)
    mods = cache / "modules"
    if mods.exists() and any(mods.glob("*.ko")):
        (root / "modules").mkdir(exist_ok=True)
        for ko in mods.glob("*.ko"):
            shutil.copy(ko, root / "modules" / ko.name)
    out = workdir / "initramfs_ftrace.cpio.gz"
    # 用 busybox 自带 cpio applet（与 build_initramfs.sh 一致）
    find = subprocess.Popen(["find", "."], cwd=root, stdout=subprocess.PIPE)
    cpio = subprocess.Popen(
        [str(bb), "cpio", "-o", "-H", "newc"],
        cwd=root, stdin=find.stdout, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    with open(out, "wb") as f:
        gz = subprocess.Popen(["gzip", "-9"], stdin=cpio.stdout, stdout=f)
        gz.communicate()
    find.wait(); cpio.wait()
    return out


def main() -> int:
    mode = os.environ.get("CMB_MODE", "trap")
    if mode not in ("trap", "map"):
        sys.exit(f"CMB_MODE 须 trap|map，got {mode!r}")

    qemu_bin = base.find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", base.DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(f"找不到 nvme_firmware：{nvme_bin}（先 cargo build --features vfio-user）")
    kernel = base.find_kernel()
    extra_cmdline = os.environ.get("GUEST_EXTRA_CMDLINE", "")
    marker = "CMB-FTRACE-" + time.strftime("%Y%m%d%H%M%S")

    tmp = Path(tempfile.mkdtemp(prefix="qemu_vfio_cmb_ftrace_"))
    initrd = build_ftrace_initrd(tmp)
    print(f"=== ftrace initramfs: {initrd} ({initrd.stat().st_size} bytes) ===", flush=True)

    sock = tmp / "nvme.sock"
    img = tmp / "nvme.img"
    srv_log_path = tmp / "server.log"
    serial_path = tmp / "serial.log"
    qemu_log_path = tmp / "qemu.log"
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)

    srv_log = open(srv_log_path, "w")
    srv = subprocess.Popen(
        [
            str(nvme_bin),
            "--vfio-user-sock", str(sock),
            "--backing-file", str(img),
            "--cmb-mode", mode,
            "--cmb-size", str(CMB_SIZE),
            "--cmb-bir", str(CMB_BIR),
        ],
        env=dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "info")),
        stdout=srv_log,
        stderr=subprocess.STDOUT,
    )
    qemu = None
    try:
        for _ in range(200):
            if sock.exists():
                break
            if srv.poll() is not None:
                print(f"=== server 早退 code={srv.returncode} ===")
                print(srv_log_path.read_text()[-2000:])
                return 1
            time.sleep(0.05)

        print(f"=== server up（--cmb-mode {mode}）；boot ftrace guest（marker={marker}）===", flush=True)
        qemu_log = open(qemu_log_path, "w")
        machine = "q35,accel=kvm:tcg"
        iommu_dev: list[str] = []
        if os.environ.get("QEMU_IOMMU"):
            machine += ",kernel-irqchip=split"
            iommu_dev = ["-device", "intel-iommu,intremap=on,caching-mode=on"]
        qemu = subprocess.Popen(
            [
                qemu_bin,
                "-machine", machine,
                "-cpu", "host",
                "-smp", "2",
                "-m", "512M",
                "-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                "-machine", "memory-backend=mem",
                *iommu_dev,
                "-kernel", str(kernel),
                "-initrd", str(initrd),
                "-append", f"console=ttyS0 panic=-1 rdinit=/init gmarker={marker} {extra_cmdline}".strip(),
                "-device", f'{{"driver":"vfio-user-pci","socket":{{"path":"{sock}","type":"unix"}}}}',
                "-serial", f"file:{serial_path}",
                "-display", "none",
                "-no-reboot",
            ],
            stdout=qemu_log,
            stderr=subprocess.STDOUT,
        )
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s 未 poweroff）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        srv_log.flush()
        server_log = srv_log_path.read_text(errors="replace") if srv_log_path.exists() else ""

        # 打印完整 guest 串口（含 FTRACE: 判定行）—— 这是本实验的核心产物
        print("=== guest 串口全文（含 FTRACE 判定）===")
        print(serial)

        cmb_used = "CMB-RESIDENT-ACCESS" in server_log
        print(f"=== firmware CMB-RESIDENT-ACCESS = {cmb_used} ===")
        # firmware 侧 IO SQ 落点（gpa）
        print("=== firmware 'Create IO SQ' 行 ===")
        for line in server_log.splitlines():
            if "Create IO SQ" in line or "added peer-to-peer" in line:
                print(line)
        return 0
    except Exception as e:  # noqa: BLE001
        print(f"=== FAILED: {e} ===")
        if qemu_log_path.exists():
            print(qemu_log_path.read_text()[-1500:])
        return 1
    finally:
        if qemu and qemu.poll() is None:
            qemu.terminate()
            try:
                qemu.wait(timeout=3)
            except subprocess.TimeoutExpired:
                qemu.kill()
        if srv.poll() is None:
            srv.terminate()
            try:
                srv.wait(timeout=3)
            except subprocess.TimeoutExpired:
                srv.kill()
        srv_log.close()
        print("--- server log（尾 40 行）---")
        if srv_log_path.exists():
            print("".join(srv_log_path.read_text().splitlines(keepends=True)[-40:]))
        if not os.environ.get("KEEP_LOGS"):
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
