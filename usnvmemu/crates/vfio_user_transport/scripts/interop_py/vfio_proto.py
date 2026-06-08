# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""vfio-user wire 协议 helpers（Python stdlib only）。

跨进程实证 harness 用：以真 vfio-user 客户端身份连入我们的 server（UNIX
socket），按真 wire 协议驱动，catch loopback 单测看不到的跨进程 wire bug
（见 LESSONS §7）。

权威 wire 参考：crates/vfio_user_transport/docs/specs/2026-06-04-vfio-user-wire-reference.md
"""
from __future__ import annotations

import socket
import struct

# ── 16-byte common header ──────────────────────────────────────────────
# msg_id u16 | cmd u16 | msg_size u32 | flags u32 | error_no u32
HEADER_FMT = "<HHIII"
HEADER_LEN = 16

# ── flags ──────────────────────────────────────────────────────────────
F_TYPE_COMMAND = 0x00
F_TYPE_REPLY = 0x01
F_TYPE_MASK = 0x0F
F_NO_REPLY = 0x10
F_ERROR = 0x20

# ── 命令号（include/vfio-user.h 权威）──────────────────────────────────
CMD_VERSION = 1
CMD_DMA_MAP = 2
CMD_DMA_UNMAP = 3
CMD_DEVICE_GET_INFO = 4
CMD_DEVICE_GET_REGION_INFO = 5
CMD_DEVICE_GET_REGION_IO_FDS = 6
CMD_DEVICE_GET_IRQ_INFO = 7
CMD_DEVICE_SET_IRQS = 8
CMD_REGION_READ = 9
CMD_REGION_WRITE = 10
CMD_DMA_READ = 11
CMD_DMA_WRITE = 12
CMD_DEVICE_RESET = 13

# ── PCI region / IRQ index ─────────────────────────────────────────────
REGION_BAR0 = 0
REGION_CONFIG = 7
IRQ_MSIX = 2

# ── region flags ───────────────────────────────────────────────────────
REGION_FLAG_READ = 0x1
REGION_FLAG_WRITE = 0x2
REGION_FLAG_MMAP = 0x4

PROTOCOL_MAJOR = 0
PROTOCOL_MINOR = 1


class WireError(Exception):
    """wire 层错误（reply error flag / 校验失败）。"""


def recv_exact(sock: socket.socket, n: int) -> bytes:
    """阻塞读恰好 n 字节；peer 提前关闭抛 WireError。"""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise WireError(f"peer closed; got {len(buf)}/{n} bytes")
        buf.extend(chunk)
    return bytes(buf)


def send_msg(
    sock: socket.socket,
    msg_id: int,
    cmd: int,
    payload: bytes = b"",
    flags: int = F_TYPE_COMMAND,
) -> None:
    """发一条 vfio-user 消息（header + payload）。"""
    msg_size = HEADER_LEN + len(payload)
    hdr = struct.pack(HEADER_FMT, msg_id, cmd, msg_size, flags, 0)
    sock.sendall(hdr + payload)


def recv_msg(sock: socket.socket) -> dict:
    """读一条消息，返 dict(msg_id, cmd, flags, error_no, payload)。"""
    hdr = recv_exact(sock, HEADER_LEN)
    msg_id, cmd, msg_size, flags, error_no = struct.unpack(HEADER_FMT, hdr)
    if msg_size < HEADER_LEN:
        raise WireError(f"msg_size {msg_size} < header {HEADER_LEN}")
    payload = recv_exact(sock, msg_size - HEADER_LEN)
    return {
        "msg_id": msg_id,
        "cmd": cmd,
        "flags": flags,
        "error_no": error_no,
        "payload": payload,
    }


def expect_reply(reply: dict, msg_id: int, cmd: int) -> bytes:
    """校验 reply 是 msg_id/cmd 对应的成功 reply；返 payload。"""
    if reply["msg_id"] != msg_id:
        raise WireError(f"reply msg_id {reply['msg_id']:#x} != {msg_id:#x}")
    if reply["flags"] & F_ERROR:
        raise WireError(f"reply error: errno={reply['error_no']}")
    if (reply["flags"] & F_TYPE_MASK) != F_TYPE_REPLY:
        raise WireError(f"not a REPLY: flags={reply['flags']:#x}")
    if reply["cmd"] != cmd:
        raise WireError(f"reply cmd {reply['cmd']} != {cmd}")
    return reply["payload"]


def version_handshake(sock: socket.socket, msg_id: int = 1) -> dict:
    """发 VERSION command（major/minor + JSON caps + NUL）+ 读 server reply。"""
    payload = struct.pack("<HH", PROTOCOL_MAJOR, PROTOCOL_MINOR) + b"{}\x00"
    send_msg(sock, msg_id, CMD_VERSION, payload)
    reply = recv_msg(sock)
    expect_reply(reply, msg_id, CMD_VERSION)
    return reply


def get_info(sock: socket.socket, msg_id: int) -> dict:
    """DEVICE_GET_INFO → dict(flags, num_regions, num_irqs)。"""
    req = struct.pack("<IIII", 16, 0, 0, 0)  # argsz, flags, num_regions, num_irqs
    send_msg(sock, msg_id, CMD_DEVICE_GET_INFO, req)
    pl = expect_reply(recv_msg(sock), msg_id, CMD_DEVICE_GET_INFO)
    argsz, flags, num_regions, num_irqs = struct.unpack("<IIII", pl[:16])
    return {"flags": flags, "num_regions": num_regions, "num_irqs": num_irqs}


def get_region_info(sock: socket.socket, msg_id: int, index: int) -> dict:
    """DEVICE_GET_REGION_INFO(index) → dict(flags, index, size)。"""
    # argsz, flags, index, cap_offset, size, offset
    req = struct.pack("<IIIIQQ", 32, 0, index, 0, 0, 0)
    send_msg(sock, msg_id, CMD_DEVICE_GET_REGION_INFO, req)
    pl = expect_reply(recv_msg(sock), msg_id, CMD_DEVICE_GET_REGION_INFO)
    argsz, flags, idx, cap_offset, size, offset = struct.unpack("<IIIIQQ", pl[:32])
    return {"flags": flags, "index": idx, "size": size, "cap_offset": cap_offset}


def get_irq_info(sock: socket.socket, msg_id: int, index: int) -> dict:
    """DEVICE_GET_IRQ_INFO(index) → dict(flags, index, count)。"""
    req = struct.pack("<IIII", 16, 0, index, 0)
    send_msg(sock, msg_id, CMD_DEVICE_GET_IRQ_INFO, req)
    pl = expect_reply(recv_msg(sock), msg_id, CMD_DEVICE_GET_IRQ_INFO)
    argsz, flags, idx, count = struct.unpack("<IIII", pl[:16])
    return {"flags": flags, "index": idx, "count": count}


def region_read(sock: socket.socket, msg_id: int, region: int, offset: int, count: int) -> bytes:
    """REGION_READ(region, offset, count) → 读出的 count 字节。"""
    req = struct.pack("<QII", offset, region, count)  # offset, region, count
    send_msg(sock, msg_id, CMD_REGION_READ, req)
    pl = expect_reply(recv_msg(sock), msg_id, CMD_REGION_READ)
    # reply = RegionAccessPayload echo(16) + count 字节
    return pl[16 : 16 + count]


def region_write(sock: socket.socket, msg_id: int, region: int, offset: int, value: bytes) -> None:
    """REGION_WRITE(region, offset, value)。"""
    req = struct.pack("<QII", offset, region, len(value)) + value
    send_msg(sock, msg_id, CMD_REGION_WRITE, req)
    expect_reply(recv_msg(sock), msg_id, CMD_REGION_WRITE)


def device_reset(sock: socket.socket, msg_id: int) -> None:
    """DEVICE_RESET（FLR）。"""
    send_msg(sock, msg_id, CMD_DEVICE_RESET)
    expect_reply(recv_msg(sock), msg_id, CMD_DEVICE_RESET)
