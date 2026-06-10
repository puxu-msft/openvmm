#!/usr/bin/env python3
# 真 QEMU vfio-user GUEST-BOOT e2e harness.
#
# run_qemu_vfio.py 只验 realize（QEMU PCI 总线看到设备）。这个进一步**引导一个真
# guest kernel**（默认从 Ubuntu 包提取的 bzImage + busybox initramfs），让 guest 的
# nvme 驱动绑定本 crate 的 vfio-user 服务端并做真 NVMe IO（write/flush/read）。
#
# 双独立 oracle（须一致才 PASS）：
#   1. guest 侧：init 写 marker 到 /dev/nvme0n1 LBA0，flush，回读比对，串口打印
#      GUEST_RESULT=PASS。
#   2. host 侧：直接读 backing file 头部找同一 marker（裸文件读，与 guest 经
#      nvme 驱动→vfio-user→firmware 的回读不同代码路径）。
#
# 用法：
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"   # QEMU ≥10.1
#   # 取 guest kernel + nvme 模块（一次，缓存）：
#   usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/fetch_guest_kernel.sh
#   python3 run_qemu_vfio_guest.py
#
# 环境变量：QEMU_BIN / NVME_BIN / GUEST_KERNEL / GUEST_INITRD / KEEP_LOGS
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATES = HERE.parent.parent.parent
DEFAULT_NVME = CRATES / "nvme_firmware" / "target" / "debug" / "nvme_firmware"
CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "usnvmemu_vfio_guest"
# per-run unique marker (kernel cmdline can't contain spaces/'='; keep it [A-Za-z0-9-])
MARKER = "VFIO-USER-GUEST-" + time.strftime("%Y%m%d%H%M%S")


def find_qemu() -> str:
    if env := os.environ.get("QEMU_BIN"):
        return env
    for cand in ("/home/linuxbrew/.linuxbrew/bin/qemu-system-x86_64", shutil.which("qemu-system-x86_64")):
        if cand and Path(cand).exists():
            return cand
    sys.exit("找不到 qemu-system-x86_64（需 ≥10.1 带 vfio-user-pci）。设 QEMU_BIN。")


def ensure_initrd() -> Path:
    if env := os.environ.get("GUEST_INITRD"):
        return Path(env)
    initrd = CACHE / "initramfs.cpio.gz"
    if not initrd.exists():
        subprocess.run([str(HERE / "build_initramfs.sh")], check=True)
    return initrd


def find_kernel() -> Path:
    if env := os.environ.get("GUEST_KERNEL"):
        return Path(env)
    k = CACHE / "vmlinuz"
    if not k.exists():
        sys.exit(f"找不到 guest kernel：{k}\n先跑 fetch_guest_kernel.sh（取 bzImage + nvme 模块）。")
    return k


def main() -> int:
    qemu_bin = find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(f"找不到 nvme_firmware：{nvme_bin}（先 cargo build --features vfio-user）")
    kernel = find_kernel()
    initrd = ensure_initrd()

    tmp = Path(tempfile.mkdtemp(prefix="qemu_vfio_guest_"))
    sock = tmp / "nvme.sock"
    img = tmp / "nvme.img"
    srv_log_path = tmp / "server.log"
    serial_path = tmp / "serial.log"
    qemu_log_path = tmp / "qemu.log"
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)  # 64 MiB backing, freshly zeroed

    srv_log = open(srv_log_path, "w")
    srv = subprocess.Popen(
        [str(nvme_bin), "--vfio-user-sock", str(sock), "--backing-file", str(img)],
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

        print(f"=== server socket up；引导真 guest（marker={MARKER}）===", flush=True)
        qemu_log = open(qemu_log_path, "w")
        qemu = subprocess.Popen(
            [
                qemu_bin,
                "-machine", "q35,accel=kvm:tcg",
                "-cpu", "host",
                "-m", "512M",
                "-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                "-machine", "memory-backend=mem",
                "-kernel", str(kernel),
                "-initrd", str(initrd),
                "-append", f"console=ttyS0 panic=-1 rdinit=/init gmarker={MARKER}",
                "-device", f'{{"driver":"vfio-user-pci","socket":{{"path":"{sock}","type":"unix"}}}}',
                "-serial", f"file:{serial_path}",
                "-display", "none",
                "-no-reboot",
            ],
            stdout=qemu_log,
            stderr=subprocess.STDOUT,
        )
        # guest boots, init does NVMe IO, then poweroff → QEMU exits (-no-reboot)
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s 未 poweroff）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        print("=== guest 串口尾部 ===")
        print("".join(serial.splitlines(keepends=True)[-30:]))

        guest_pass = "GUEST_RESULT=PASS" in serial
        # host-side independent oracle: raw-read the backing file head for the marker
        head = img.read_bytes()[:4096]
        host_found = MARKER.encode() in head
        print(f"=== GUEST_RESULT=PASS: {guest_pass} ; HOST ORACLE marker in backing: {host_found} ===")

        if guest_pass and host_found:
            print("=== PASS: 真 guest 经 vfio-user 对 NVMe 做了真 IO，host backing 独立核到 ===")
            return 0
        if "GUEST_RESULT=NO_NVME" in serial:
            print("=== FAIL: guest 没枚举到 /dev/nvme0n1（vfio-user 设备未被 nvme 驱动绑定）===")
        print("=== FAIL ===")
        return 1
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
        print("--- server log (尾 20 行) ---")
        if srv_log_path.exists():
            print("".join(srv_log_path.read_text().splitlines(keepends=True)[-20:]))
        if not os.environ.get("KEEP_LOGS"):
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
