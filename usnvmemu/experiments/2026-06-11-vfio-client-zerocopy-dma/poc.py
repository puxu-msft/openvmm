# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""POC：非-QEMU vfio-user client 的 fd-passing 零拷贝 DMA（非-CVM）。

验证 Spec A（vfio_user_device = OpenHCL VTL2 vfio-user client）的承重假设：我们
自己写的 client 经 AF_UNIX + SCM_RIGHTS 把一段 memfd-backed "guest RAM" 的 fd 交给
firmware server，server 对它 mmap 零拷贝 DMA，驱动一次 NVMe admin Identify 端到端通。

零拷贝自证：本 client **故意不服务任何 server-initiated DMA_READ/WRITE**。若 server
走 message-mediated（非 mmap），取 SQE 时会发 DMA_READ 阻塞等我们 reply → 我们不回 →
Identify 永不完成 → 超时失败。**故 Identify 成功 ⟺ server 走了 mmap 零拷贝。**

详见同目录 README.md。用法：`python3 poc.py`（自动 build + spawn server）。
"""
from __future__ import annotations

import array
import ctypes
import mmap
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
INTEROP = (HERE / ".." / ".." / "crates" / "vfio_user_transport" / "scripts" / "interop_py").resolve()
NVME_DIR = (HERE / ".." / ".." / "crates" / "nvme_firmware").resolve()
sys.path.insert(0, str(INTEROP))
import vfio_proto as v  # noqa: E402  （复用已验证的 wire helpers）

# ── "guest RAM" 布局（memfd 内 offset == GPA，DMA_MAP region addr=0）──────
RAM_SIZE = 256 * 1024
ASQ_GPA = 0x1000   # admin SQ base
ACQ_GPA = 0x2000   # admin CQ base
PRP1_GPA = 0x3000  # Identify 结果 4KiB buffer
QDEPTH = 2

# ── NVMe BAR0 寄存器 offset（regs.rs）────────────────────────────────────
R_CC = 0x14
R_CSTS = 0x1C
R_AQA = 0x24
R_ASQ = 0x28
R_ACQ = 0x30
SQ0TDBL = 0x1000  # admin SQ tail doorbell（dstrd=0 → 4B stride）

CC_EN = 1
CC_VALUE = (4 << 20) | (6 << 16) | CC_EN  # IOCQES=4, IOSQES=6, EN=1
CSTS_RDY = 1


def log(msg: str) -> None:
    print(f"[poc-zerocopy-dma] {msg}", flush=True)


def check(cond: bool, what: str) -> None:
    if not cond:
        raise AssertionError(f"FAIL: {what}")
    log(f"  ✓ {what}")


# ── SCM_RIGHTS fd 传递（vfio_proto 没有，POC 本地实现）──────────────────
def send_msg_with_fd(sock: socket.socket, msg_id: int, cmd: int, payload: bytes, fd: int) -> None:
    """发一条 vfio-user 消息，附带一个 fd（SCM_RIGHTS ancillary）。"""
    msg_size = v.HEADER_LEN + len(payload)
    hdr = struct.pack(v.HEADER_FMT, msg_id, cmd, msg_size, v.F_TYPE_COMMAND, 0)
    anc = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [fd]))]
    sock.sendmsg([hdr + payload], anc)


def dma_map_with_fd(sock: socket.socket, msg_id: int, addr: int, size: int, fd: int) -> None:
    """DMA_MAP region [addr, addr+size) READABLE|WRITEABLE，附 memfd（offset=0）。"""
    # DmaMapPayload = <IIQQQ : argsz, flags, offset, addr, size
    flags = 0x1 | 0x2  # READABLE | WRITEABLE
    payload = struct.pack("<IIQQQ", 32, flags, 0, addr, size)
    send_msg_with_fd(sock, msg_id, v.CMD_DMA_MAP, payload, fd)
    v.expect_reply(v.recv_msg(sock), msg_id, v.CMD_DMA_MAP)


def build_server() -> Path:
    log("building nvme_firmware (default features include vfio-user) ...")
    r = subprocess.run(
        ["cargo", "build", "--bin", "nvme_firmware"],
        cwd=NVME_DIR, capture_output=True, text=True,
    )
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        raise RuntimeError("cargo build failed")
    binary = NVME_DIR / "target" / "debug" / "nvme_firmware"
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


def build_identify_sqe(cid: int, prp1: int) -> bytes:
    """64-byte Identify Controller SQE（opcode 0x06, CNS=0x01）。"""
    sqe = bytearray(64)
    sqe[0] = 0x06              # opcode = Identify
    sqe[1] = 0x00             # fuse/psdt
    struct.pack_into("<H", sqe, 2, cid)   # CID
    struct.pack_into("<I", sqe, 4, 0)     # NSID = 0
    struct.pack_into("<Q", sqe, 24, prp1)  # PRP1
    struct.pack_into("<Q", sqe, 32, 0)     # PRP2
    struct.pack_into("<I", sqe, 40, 0x01)  # CDW10: CNS = 0x01 (Identify Controller)
    return bytes(sqe)


def run(sock: socket.socket, ram: mmap.mmap, memfd: int) -> None:
    mid = 1

    def nxt() -> int:
        nonlocal mid
        m = mid
        mid += 1
        return m

    # 1) 握手 + GET_INFO
    v.version_handshake(sock, nxt())
    log("VERSION handshake OK")
    info = v.get_info(sock, nxt())
    check(info["num_regions"] == 9, "GET_INFO num_regions=9")

    # 2) DMA_MAP：把 memfd 当 guest RAM [0, 256K) 交给 server（SCM_RIGHTS）
    dma_map_with_fd(sock, nxt(), 0, RAM_SIZE, memfd)
    log("DMA_MAP with memfd OK (server should mmap zero-copy)")

    # 3) NVMe admin bring-up（REGION_WRITE BAR0）
    aqa = ((QDEPTH - 1) << 16) | (QDEPTH - 1)
    v.region_write(sock, nxt(), v.REGION_BAR0, R_AQA, struct.pack("<I", aqa))
    v.region_write(sock, nxt(), v.REGION_BAR0, R_ASQ, struct.pack("<Q", ASQ_GPA))
    v.region_write(sock, nxt(), v.REGION_BAR0, R_ACQ, struct.pack("<Q", ACQ_GPA))
    v.region_write(sock, nxt(), v.REGION_BAR0, R_CC, struct.pack("<I", CC_VALUE))
    log("wrote AQA/ASQ/ACQ/CC.EN")

    # poll CSTS.RDY
    for _ in range(100):
        csts = struct.unpack("<I", v.region_read(sock, nxt(), v.REGION_BAR0, R_CSTS, 4))[0]
        if csts & CSTS_RDY:
            break
        time.sleep(0.02)
    check(csts & CSTS_RDY == CSTS_RDY, "CSTS.RDY=1 (controller enabled)")

    # 4) client 在自己的 memfd mmap 里放 Identify SQE 到 ASQ[0]
    cid = 0x0042
    ram[ASQ_GPA:ASQ_GPA + 64] = build_identify_sqe(cid, PRP1_GPA)
    # 清零 CQE/结果区，便于检测 server 写入
    ram[ACQ_GPA:ACQ_GPA + 16] = b"\x00" * 16
    ram[PRP1_GPA:PRP1_GPA + 4096] = b"\x00" * 4096
    log(f"placed Identify SQE at ASQ[0] (cid={cid:#x}, PRP1={PRP1_GPA:#x})")

    # 5) ring admin SQ tail doorbell = 1（**绝不服务 DMA_READ/WRITE — 零拷贝自证**）
    v.region_write(sock, nxt(), v.REGION_BAR0, SQ0TDBL, struct.pack("<I", 1))
    log("rang SQ0 tail doorbell = 1")

    # 6) 验 server 是否零拷贝写回我们的 memfd（poll CQE phase）
    cqe = None
    for _ in range(100):
        cand = bytes(ram[ACQ_GPA:ACQ_GPA + 16])
        dw3 = struct.unpack("<I", cand[12:16])[0]
        if dw3 & (1 << 16):  # phase bit set → CQE 已写
            cqe = cand
            break
        time.sleep(0.02)
    check(cqe is not None, "CQE 出现在我们的 memfd（server 零拷贝 dma_write 写回）")

    dw3 = struct.unpack("<I", cqe[12:16])[0]
    cqe_cid = dw3 & 0xFFFF
    status = (dw3 >> 17) & 0x7FFF
    check(cqe_cid == cid, f"CQE CID = {cid:#x}（匹配我们提交的命令）")
    check(status == 0, f"CQE status = 0 (success)，实际={status:#x}")

    # 7) 验 Identify 结果（MN 字段 bytes 24..64）零拷贝落进我们的 memfd
    ident = bytes(ram[PRP1_GPA:PRP1_GPA + 4096])
    mn = ident[24:64].decode("ascii", errors="replace").strip()
    log(f"Identify MN field = {mn!r}")
    check("NVMe" in mn or "OpenHCL" in mn, f"Identify MN 含预期厂商串（零拷贝 DMA 写回）：{mn!r}")

    log("零拷贝自证成立：全程未服务任何 DMA_READ/WRITE，Identify 仍完成 ⟹ server 走 mmap")


def main() -> int:
    binary = build_server()
    tmpdir = tempfile.mkdtemp(prefix="poc_zc_dma_")
    sock_path = Path(tmpdir) / "nvme.sock"
    img_path = Path(tmpdir) / "ns1.img"
    with open(img_path, "wb") as f:
        f.truncate(1024 * 1024)

    # memfd 当 guest RAM
    memfd = os.memfd_create("poc-guest-ram", 0)
    os.ftruncate(memfd, RAM_SIZE)
    ram = mmap.mmap(memfd, RAM_SIZE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)

    env = dict(os.environ, RUST_LOG="warn")
    log(f"spawning: {binary.name} --vfio-user-sock {sock_path}")
    proc = subprocess.Popen(
        [str(binary), "--vfio-user-sock", str(sock_path), "--backing-file", str(img_path)],
        env=env,
    )
    try:
        wait_for_socket(sock_path, proc)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(8.0)
            sock.connect(str(sock_path))
            run(sock, ram, memfd)
        log("POC PASSED ✓ — 非-QEMU client fd-passing 零拷贝 DMA 端到端可行")
        return 0
    except Exception as e:  # noqa: BLE001
        log(f"POC FAILED: {e}")
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
        ram.close()
        os.close(memfd)
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
