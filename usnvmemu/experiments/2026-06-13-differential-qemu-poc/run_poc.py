#!/usr/bin/env python3
# 差分 POC harness —— 在一个 QEMU guest 内同时挂两个本地 PCIe NVMe controller：
#   nvme0/1 之一 = usnvmemu(vfio-user-pci)，另一 = QEMU 自带 -device nvme。
# 同一 guest 内核 nvme 驱动驱动两者 → 同一逻辑 passthru 命令翻译成**等价 SQE**到达
# 各自 controller（中和 architect #5「不同 wire」异议）。pt_diff 对两端发同一矩阵，
# 比对 status(SCT|SC)/result/errno，把分歧打成表。
#
# 验证承重假设：
#   (H1) 同命令→等价 SQE（同内核同 PCIe，按构造）；可由 usnvmemu server.log 旁证。
#   (H2) 畸形/边界命令能等价注入 + 内核 passthru 透传（ioctl_ret<0/errno 即不透传）。
#   价值信号：两端 status 分歧 = 「都 self-consistent 但语义不同」候选。
#
# 复用 run_qemu_vfio_guest.py 的 guest-boot 基建（缓存的 bzImage + busybox + nvme 模块）。
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/fetch_guest_kernel.sh   # 一次
#   python3 run_poc.py
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
# usnvmemu/experiments/<poc>/ → 回到 crates/nvme_firmware 的 server 二进制
USNVMEMU = HERE.parent.parent
DEFAULT_NVME = USNVMEMU / "crates" / "nvme_firmware" / "target" / "debug" / "nvme_firmware"
CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "usnvmemu_vfio_guest"


def find_qemu() -> str:
    if env := os.environ.get("QEMU_BIN"):
        return env
    for cand in ("/home/linuxbrew/.linuxbrew/bin/qemu-system-x86_64", shutil.which("qemu-system-x86_64")):
        if cand and Path(cand).exists():
            return cand
    sys.exit("找不到 qemu-system-x86_64（需 ≥10.1 带 vfio-user-pci）。设 QEMU_BIN。")


def build_injector(tmp: Path) -> Path:
    out = tmp / "pt_diff"
    cc = shutil.which("gcc") or shutil.which("cc")
    if not cc:
        sys.exit("缺 gcc/cc，无法编译静态 pt_diff。")
    subprocess.run([cc, "-static", "-O2", "-o", str(out), str(HERE / "pt_diff.c")], check=True)
    return out


def build_initramfs(tmp: Path, injector: Path) -> Path:
    bb = CACHE / "busybox"
    if not bb.exists():
        sys.exit(f"缺 busybox 缓存：{bb}（先跑 qemu_interop/build_initramfs.sh 或 fetch_guest_kernel.sh）")
    root = tmp / "initramfs_root"
    (root / "bin").mkdir(parents=True)
    (root / "modules").mkdir()
    for d in ("proc", "sys", "dev", "tmp"):
        (root / d).mkdir()
    shutil.copy(bb, root / "bin" / "busybox")
    os.chmod(root / "bin" / "busybox", 0o755)
    shutil.copy(injector, root / "bin" / "pt_diff")
    os.chmod(root / "bin" / "pt_diff", 0o755)
    shutil.copy(HERE / "poc_init.sh", root / "init")
    os.chmod(root / "init", 0o755)
    mods = sorted((CACHE / "modules").glob("*.ko"))
    for m in mods:
        shutil.copy(m, root / "modules" / m.name)
    out = tmp / "initramfs.cpio.gz"
    # 用 busybox 自带 cpio applet（host cpio 常缺）
    find = subprocess.Popen(["find", "."], cwd=root, stdout=subprocess.PIPE)
    cpio = subprocess.Popen([str(bb), "cpio", "-o", "-H", "newc"], cwd=root,
                            stdin=find.stdout, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    find.stdout.close()
    with open(out, "wb") as f:
        gz = subprocess.Popen(["gzip", "-9"], stdin=cpio.stdout, stdout=f)
        cpio.stdout.close()
        gz.wait()
    cpio.wait()
    find.wait()
    return out


def main() -> int:
    qemu_bin = find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(f"找不到 nvme_firmware：{nvme_bin}（先 cargo build --features vfio-user）")
    kernel = Path(os.environ.get("GUEST_KERNEL", CACHE / "vmlinuz"))
    if not kernel.exists():
        sys.exit(f"找不到 guest kernel：{kernel}（先跑 qemu_interop/fetch_guest_kernel.sh）")

    tmp = Path(tempfile.mkdtemp(prefix="poc_diff_"))
    injector = build_injector(tmp)
    initrd = build_initramfs(tmp, injector)

    sock = tmp / "nvme.sock"
    uimg = tmp / "usnvme.img"   # usnvmemu backing
    qimg = tmp / "qemunvme.img" # QEMU 自带 nvme backing
    for p in (uimg, qimg):
        with open(p, "wb") as f:
            f.truncate(64 * 1024 * 1024)
    srv_log_path = tmp / "server.log"
    serial_path = tmp / "serial.log"
    qemu_log_path = tmp / "qemu.log"

    srv_log = open(srv_log_path, "w")
    srv = subprocess.Popen(
        [str(nvme_bin), "--vfio-user-sock", str(sock), "--backing-file", str(uimg)],
        env=dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "info")),
        stdout=srv_log, stderr=subprocess.STDOUT,
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

        print("=== server socket up；引导 guest（双 NVMe：usnvmemu vfio-user + QEMU 自带）===", flush=True)
        qemu_log = open(qemu_log_path, "w")
        qemu = subprocess.Popen(
            [
                qemu_bin,
                "-machine", "q35,accel=kvm:tcg",
                "-cpu", "host",
                "-smp", "2",
                "-m", "512M",
                "-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                "-machine", "memory-backend=mem",
                "-kernel", str(kernel),
                "-initrd", str(initrd),
                "-append", "console=ttyS0 panic=-1 rdinit=/init",
                # usnvmemu：vfio-user-pci
                "-device", f'{{"driver":"vfio-user-pci","socket":{{"path":"{sock}","type":"unix"}}}}',
                # QEMU 自带 NVMe（差分对端，独立 C 实现）
                "-drive", f"file={qimg},if=none,id=qnvm,format=raw",
                "-device", "nvme,serial=qemupoc,drive=qnvm",
                "-serial", f"file:{serial_path}",
                "-display", "none",
                "-no-reboot",
            ],
            stdout=qemu_log, stderr=subprocess.STDOUT,
        )
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s 未 poweroff）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        # 打印 POC 串口（设备识别 + 矩阵）
        print("=== guest 串口（POC 行）===")
        for ln in serial.splitlines():
            if ln.startswith("POC") or ln.startswith("PTDIFF"):
                print(ln)

        # 解析 PTDIFF，出 diff 表
        rows = []
        for ln in serial.splitlines():
            if not ln.startswith("PTDIFF|"):
                continue
            parts = ln.split("|")
            name = parts[1]
            us = parts[2].replace("US ", "").strip()
            qe = parts[3].replace("QEMU ", "").strip()
            us_st = _field(us, "status")
            qe_st = _field(qe, "status")
            us_rc = _field(us, "ioctl_ret")
            qe_rc = _field(qe, "ioctl_ret")
            diverge = (us_st != qe_st)
            rows.append((name, us_st, qe_st, us_rc, qe_rc, diverge, us, qe))

        print("\n=== 差分表（status = SCT|SC，去 phase）===")
        print(f"{'command':<16} {'usnvmemu':<10} {'qemu':<10} {'us_ret':<7} {'qe_ret':<7} {'verdict'}")
        ndiv = 0
        for (name, us_st, qe_st, us_rc, qe_rc, diverge, us, qe) in rows:
            v = "↯ DIVERGE" if diverge else "= same"
            if diverge:
                ndiv += 1
            print(f"{name:<16} {us_st:<10} {qe_st:<10} {us_rc:<7} {qe_rc:<7} {v}")
        print(f"\n=== {len(rows)} 命令；{ndiv} 处 status 分歧（待人工对 spec 裁定谁对）===")

        if "POC_RESULT=DONE" not in serial:
            print("=== WARN: guest 未跑完矩阵（POC_RESULT!=DONE）===")
            return 1
        if not rows:
            print("=== FAIL: 没采到 PTDIFF 行 ===")
            return 1
        print("=== POC 跑通：双 controller 差分注入闭环成立 ===")
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
        if os.environ.get("KEEP_LOGS"):
            print(f"--- logs kept: {tmp} ---")
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def _field(line: str, key: str) -> str:
    for tok in line.split():
        if tok.startswith(key + "="):
            return tok[len(key) + 1:]
    return "?"


if __name__ == "__main__":
    sys.exit(main())
