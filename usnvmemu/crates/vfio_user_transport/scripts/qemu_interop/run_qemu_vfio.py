#!/usr/bin/env python3
# 真 QEMU vfio-user e2e harness（差分 oracle，非自家两端）。
#
# 起一个我们的 nvme_firmware vfio-user server，让 **真 QEMU**（10.1+ 自带
# vfio-user-pci 客户端，本仓库用 11.0.1 验证过）以 `vfio-user-pci` 设备接管它，
# 然后通过 QMP `query-pci` 确认 guest 侧 PCI 总线真看到我们的 NVMe 控制器
# (vendor 0x1414 / device 0xc0de)。device realize 会跑完整
# VERSION / GET_INFO / GET_REGION_INFO / config 读 / DMA_MAP 握手——所以
# 这一条 e2e 等价于"独立第二实现"验证整个 vfio-user 服务端 wire 协议。
#
# 为什么需要它：自己写 server + 自己写 Python client 共享同一套错误假设两边都
# 不报错（LESSONS §20 self-consistent ≠ spec-conformant）。真 QEMU 是独立 C
# 实现，曾用它抓出 version-minor 协商、max_msg_fds≤16 两个握手 bug。
#
# 用法：
#   # 1) 先编译带 vfio-user feature 的 server（usnvmemu 各 crate 独立、非 workspace，
#   #    须在 crate 目录内编译；不能用 `-p nvme_firmware`）
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   # 2) 装一个 ≥10.1 的 QEMU（本仓库用 linuxbrew qemu 11）
#   #    eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   # 3) 跑
#   python3 run_qemu_vfio.py
#
# 可用环境变量覆盖默认：
#   QEMU_BIN   QEMU system 二进制（默认自动探测 linuxbrew / PATH）
#   NVME_BIN   nvme_firmware 二进制（默认 crate-local target/debug）
#   KEEP_LOGS  非空则保留 /tmp 日志不清理
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
# scripts/qemu_interop → crate root → crates/ → nvme_firmware/target/debug
CRATES = HERE.parent.parent.parent
DEFAULT_NVME = CRATES / "nvme_firmware" / "target" / "debug" / "nvme_firmware"

VENDOR_ID = 0x1414  # Microsoft（教学控制器借用）
DEVICE_ID = 0xC0DE


def find_qemu() -> str:
    if env := os.environ.get("QEMU_BIN"):
        return env
    for cand in (
        "/home/linuxbrew/.linuxbrew/bin/qemu-system-x86_64",
        shutil.which("qemu-system-x86_64"),
    ):
        if cand and Path(cand).exists():
            return cand
    sys.exit(
        "找不到 qemu-system-x86_64（需 ≥10.1 带 vfio-user-pci）。"
        "设 QEMU_BIN 或 `eval \"$(brew shellenv bash)\"` 后重试。"
    )


def qmp_cmd(s: socket.socket, execute: str, **args):
    obj = {"execute": execute}
    if args:
        obj["arguments"] = args
    s.sendall((json.dumps(obj) + "\r\n").encode())
    buf = b""
    s.settimeout(8)
    while True:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("QMP socket closed")
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if "return" in msg or "error" in msg:
                return msg
            # event → 继续读


def main() -> int:
    qemu_bin = find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(
            f"找不到 nvme_firmware 二进制：{nvme_bin}\n"
            "先 `cargo build -p nvme_firmware --features vfio-user`，"
            "或设 NVME_BIN。"
        )

    tmp = Path(tempfile.mkdtemp(prefix="qemu_vfio_"))
    sock = tmp / "nvme.sock"
    img = tmp / "nvme.img"
    qmp = tmp / "qmp.sock"
    srv_log_path = tmp / "server.log"
    qemu_log_path = tmp / "qemu.log"
    with open(img, "wb") as f:
        f.truncate(1024 * 1024)

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
        print("=== server socket up；启动真 QEMU vfio-user-pci 接管 ===", flush=True)
        # q35 + 共享内存（vfio-user DMA_MAP 需 share=on 的 memfd）+ 我们的设备
        # + QMP + -S 暂停 CPU（只验 realize/握手，不引导 guest OS）。
        qemu_log = open(qemu_log_path, "w")
        qemu = subprocess.Popen(
            [
                qemu_bin,
                "-machine", "q35,accel=kvm:tcg",
                "-m", "256M",
                "-object", "memory-backend-memfd,id=mem,size=256M,share=on",
                "-machine", "memory-backend=mem",
                "-device",
                json.dumps(
                    {"driver": "vfio-user-pci", "socket": {"path": str(sock), "type": "unix"}}
                ),
                "-display", "none",
                "-qmp", f"unix:{qmp},server,nowait",
                "-S",
            ],
            stdout=qemu_log,
            stderr=subprocess.STDOUT,
        )
        for _ in range(200):
            if qmp.exists() or qemu.poll() is not None:
                break
            time.sleep(0.05)
        time.sleep(0.5)  # 让 device realize 跑完整握手
        if qemu.poll() is not None:
            print(f"=== QEMU realize 失败 (exit {qemu.returncode}) ===")
            print(qemu_log_path.read_text()[-2000:])
            return 1

        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(8)
        s.connect(str(qmp))
        greeting = s.recv(65536)
        assert b"QMP" in greeting, "no QMP greeting"
        qmp_cmd(s, "qmp_capabilities")
        pci = qmp_cmd(s, "query-pci")
        qmp_cmd(s, "quit")
        s.close()

        found = []
        for bus in pci.get("return", []):
            for dev in bus.get("devices", []):
                idd = dev.get("id", {})
                v = idd.get("vendor")
                if v is not None:
                    found.append(
                        (v, idd.get("device"), dev.get("class_info", {}))
                    )
        print("=== QEMU query-pci 看到的设备 ===")
        ours = None
        for v, d, ci in found:
            mark = ""
            if v == VENDOR_ID and d == DEVICE_ID:
                ours = (v, d, ci)
                mark = "  <<< 我们的 NVMe vfio-user 设备!"
            print(f"  vendor={v:#06x} device={d:#06x} class={ci.get('desc','?')}{mark}")
        if ours is None:
            print("=== FAIL: QEMU 未在 PCI 总线看到我们的设备 ===")
            return 1
        print(
            f"=== PASS: 真 QEMU vfio-user-pci realize 成功 — guest 侧 PCI 总线看到"
            f" NVMe(vendor {VENDOR_ID:#06x} device {DEVICE_ID:#06x},"
            f" class={ours[2].get('desc','?')}) ==="
        )
        return 0
    except Exception as e:  # noqa: BLE001
        print(f"=== FAILED: {e} ===")
        print("--- qemu log ---")
        if qemu_log_path.exists():
            print(qemu_log_path.read_text()[-2000:])
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
        print("--- server log (尾 25 行) ---")
        if srv_log_path.exists():
            print("".join(srv_log_path.read_text().splitlines(keepends=True)[-25:]))
        if not os.environ.get("KEEP_LOGS"):
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
