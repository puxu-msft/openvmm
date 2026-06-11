# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""共享 POC helpers：build/spawn firmware server、SCM_RIGHTS fd 传递、DMA_MAP、
SET_IRQS、NVMe admin bring-up。被本目录各 pocN_*.py 复用（DRY）。

复用 interop_py 的 `vfio_proto` 作基础 wire（header/handshake/region_rw）；本文件
只补 interop_py 没有的、需要 fd 传递的命令（DMA_MAP / SET_IRQS）+ NVMe 驱动。
"""
from __future__ import annotations

import array
import mmap
import os
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
INTEROP = (HERE / ".." / ".." / "crates" / "vfio_user_transport" / "scripts" / "interop_py").resolve()
NVME_DIR = (HERE / ".." / ".." / "crates" / "nvme_firmware").resolve()
sys.path.insert(0, str(INTEROP))
import vfio_proto as v  # noqa: E402

# ── NVMe BAR0 寄存器 offset（regs.rs）+ 默认 "guest RAM" 布局 ────────────
R_CC, R_CSTS, R_AQA, R_ASQ, R_ACQ = 0x14, 0x1C, 0x24, 0x28, 0x30
SQ0TDBL = 0x1000
CC_VALUE = (4 << 20) | (6 << 16) | 1  # IOCQES=4, IOSQES=6, EN=1
CSTS_RDY = 1
RAM_SIZE = 256 * 1024
ASQ_GPA, ACQ_GPA, PRP1_GPA, QDEPTH = 0x1000, 0x2000, 0x3000, 2


def log(tag: str, msg: str) -> None:
    print(f"[{tag}] {msg}", flush=True)


def check(tag: str, cond: bool, what: str) -> None:
    if not cond:
        raise AssertionError(f"FAIL: {what}")
    print(f"[{tag}]   ✓ {what}", flush=True)


def build_server() -> Path:
    binary = NVME_DIR / "target" / "debug" / "nvme_firmware"
    # POC_SKIP_BUILD=1：用现成 binary（当并发会话正在改 nvme_firmware、build 暂时
    # 红时，仍能跑 POC，不触碰他人 WIP）。
    if os.environ.get("POC_SKIP_BUILD") == "1":
        if not binary.exists():
            raise RuntimeError(f"POC_SKIP_BUILD=1 but binary 不存在: {binary}")
        return binary
    r = subprocess.run(["cargo", "build", "--bin", "nvme_firmware"],
                       cwd=NVME_DIR, capture_output=True, text=True)
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        raise RuntimeError("cargo build failed")
    if not binary.exists():
        raise RuntimeError(f"binary not found: {binary}")
    return binary


def wait_for_socket(path: Path, proc: subprocess.Popen, timeout: float = 15.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            return
        if proc.poll() is not None:
            raise RuntimeError(f"server exited early (code {proc.returncode})")
        time.sleep(0.05)
    raise RuntimeError(f"socket {path} not created within {timeout}s")


def send_msg_with_fds(sock: socket.socket, msg_id: int, cmd: int, payload: bytes, fds: list[int]) -> None:
    """发一条 vfio-user 消息，附带 fds（SCM_RIGHTS ancillary）。"""
    msg_size = v.HEADER_LEN + len(payload)
    hdr = struct.pack(v.HEADER_FMT, msg_id, cmd, msg_size, v.F_TYPE_COMMAND, 0)
    anc = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds))]
    sock.sendmsg([hdr + payload], anc)


def dma_map_with_fd(sock: socket.socket, msg_id: int, addr: int, size: int, fd: int) -> None:
    """DMA_MAP [addr, addr+size) READABLE|WRITEABLE，附 memfd（DmaMapPayload=<IIQQQ）。"""
    payload = struct.pack("<IIQQQ", 32, 0x1 | 0x2, 0, addr, size)
    send_msg_with_fds(sock, msg_id, v.CMD_DMA_MAP, payload, [fd])
    v.expect_reply(v.recv_msg(sock), msg_id, v.CMD_DMA_MAP)


def set_irqs_eventfds(sock: socket.socket, msg_id: int, start: int, eventfds: list[int]) -> None:
    """SET_IRQS MSI-X DATA_EVENTFD：把 count 个 eventfd 经 SCM_RIGHTS 配给向量。

    IrqSetPayload=<IIIII : argsz, flags, index, start, count。
    """
    flags = 0x04 | 0x20  # irq_set::DATA_EVENTFD | ACTION_TRIGGER（assign 须二者并存）
    payload = struct.pack("<IIIII", 20, flags, v.IRQ_MSIX, start, len(eventfds))
    send_msg_with_fds(sock, msg_id, v.CMD_DEVICE_SET_IRQS, payload, eventfds)
    v.expect_reply(v.recv_msg(sock), msg_id, v.CMD_DEVICE_SET_IRQS)


def nvme_enable(sock: socket.socket, mid_gen, asq: int = ASQ_GPA, acq: int = ACQ_GPA,
                qdepth: int = QDEPTH) -> None:
    """写 AQA/ASQ/ACQ/CC.EN 并 poll CSTS.RDY。`mid_gen` 是 msg_id 生成器（callable）。"""
    aqa = ((qdepth - 1) << 16) | (qdepth - 1)
    v.region_write(sock, mid_gen(), v.REGION_BAR0, R_AQA, struct.pack("<I", aqa))
    v.region_write(sock, mid_gen(), v.REGION_BAR0, R_ASQ, struct.pack("<Q", asq))
    v.region_write(sock, mid_gen(), v.REGION_BAR0, R_ACQ, struct.pack("<Q", acq))
    v.region_write(sock, mid_gen(), v.REGION_BAR0, R_CC, struct.pack("<I", CC_VALUE))
    for _ in range(100):
        csts = struct.unpack("<I", v.region_read(sock, mid_gen(), v.REGION_BAR0, R_CSTS, 4))[0]
        if csts & CSTS_RDY:
            return
        time.sleep(0.02)
    raise AssertionError("CSTS.RDY never set")


def build_identify_sqe(cid: int, prp1: int) -> bytes:
    """64-byte Identify Controller SQE（opcode 0x06, CNS=0x01）。"""
    sqe = bytearray(64)
    sqe[0] = 0x06
    struct.pack_into("<H", sqe, 2, cid)
    struct.pack_into("<Q", sqe, 24, prp1)
    struct.pack_into("<I", sqe, 40, 0x01)  # CDW10 CNS=Identify Controller
    return bytes(sqe)


class GuestRam:
    """memfd-backed "guest RAM"：client 自己也 mmap，模拟 guest 物理内存。"""

    def __init__(self, size: int = RAM_SIZE):
        self.fd = os.memfd_create("poc-guest-ram", 0)
        os.ftruncate(self.fd, size)
        self.mem = mmap.mmap(self.fd, size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)

    def close(self) -> None:
        self.mem.close()
        os.close(self.fd)
