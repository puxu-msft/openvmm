#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""2026-06-09 Fused Compare-and-Write over fabric e2e — 原子 CAS (无 sudo)。

验证 NVMe-oF TCP target 经 **fabric 路径**（async session handle_io_cmd_async →
handle_fused_io_async → controller dispatch_sqe 配对）真做原子 Compare-and-Write：

  fused 对 = 连续两 capsule：Compare(opc=0x05, fuse=01) + Write(opc=0x01, fuse=10)，
  nsid/slba/nlb 相同。wire 流：
    host → Compare capsule (fuse=01)          [target 暂存，无响应]
    host → Write capsule   (fuse=10)
    target → R2T(cccid=Compare CID)           host → H2CData(Compare 数据)
    (Compare PASS) target → R2T(cccid=Write CID)  host → H2CData(Write 数据)
    target → CapsuleResp(Compare CID) + CapsuleResp(Write CID)

  Compare FAIL 时：无 Write R2T，两条 CQE 均 COMPARE_FAILURE(0x85)，**Write 不写盘**。

测试核心（修前 fabric 走 dispatch_io 当两条独立命令 → Compare 失败 Write 仍写 →
非原子 → 本测试 case 3 会 FAIL）：
  1. 预写 LBA 5 = P0。
  2. 原子 CAS：Compare(P0) + Write(P1)，Compare 匹配 → Write 生效，读回 = P1，双 SC=0。
  3. 原子 CAS：Compare(P0) + Write(P2)，Compare(P0) vs backing(P1) 不匹配 →
     双 SC=0x85 + **读回仍 = P1（Write 未生效，原子性证明）**。

跑法（默认 plaintext target，无需 --allow-format）：
    uv run python fused_cw_e2e.py
"""
import socket
import struct
import sys
import os

HOST = "127.0.0.1"
PORT = int(os.environ.get("NVME_PORT", "4420"))
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:fused-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"

NVME_OPC_FABRIC = 0x7F
FCTYPE_PROPERTY_SET = 0x00
FCTYPE_CONNECT = 0x01

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_H2C_DATA = 0x06
PDU_C2H_DATA = 0x07
PDU_R2T = 0x09

PROP_CC = 0x14
SECTOR = 512
COMPARE_FAILURE = 0x85


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def ok(msg: str) -> None:
    print(f"OK: {msg}")


def recv_exact(s: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise RuntimeError(f"EOF after {len(buf)}/{n}")
        buf += chunk
    return buf


def read_pdu(s: socket.socket):
    hdr = recv_exact(s, 8)
    pdu_type, _flags, hlen, pdo, plen = struct.unpack("<BBBBI", hdr)
    psh = recv_exact(s, hlen - 8) if hlen > 8 else b""
    consumed = hlen
    if pdo > consumed:
        recv_exact(s, pdo - consumed)
        consumed = pdo
    data = recv_exact(s, plen - consumed) if plen > consumed else b""
    return pdu_type, psh, data


def write_pdu(s: socket.socket, pdu_type: int, psh: bytes, data: bytes = b"", pdo: int = 0) -> None:
    hlen = 8 + len(psh)
    if pdo == 0:
        plen = hlen + len(data)
        body = psh + data
    else:
        plen = pdo + len(data)
        body = psh + (b"\x00" * (pdo - hlen)) + data
    s.sendall(struct.pack("<BBBBI", pdu_type, 0, hlen, pdo, plen) + body)


def write_pdu_h2c(s: socket.socket, psh: bytes, data: bytes, pdo: int, flags: int) -> None:
    hlen = 8 + len(psh)
    plen = pdo + len(data)
    hdr = struct.pack("<BBBBI", PDU_H2C_DATA, flags, hlen, pdo, plen)
    s.sendall(hdr + psh + (b"\x00" * (pdo - hlen)) + data)


def build_icreq() -> bytes:
    return struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)


def build_connect_sqe(cid: int, qid: int, sqsize: int, kato: int) -> bytes:
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    sqe[40:64] = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    return bytes(sqe)


def build_connect_data() -> bytes:
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    buf[256 : 256 + len(SUBNQN)] = SUBNQN.encode("ascii")
    buf[512 : 512 + len(HOSTNQN)] = HOSTNQN.encode("ascii")
    return bytes(buf)


def build_cc_en() -> bytes:
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = (0x0002).to_bytes(2, "little")
    sqe[4] = FCTYPE_PROPERTY_SET
    sqe[40:64] = struct.pack("<B3sIQ8s", 0, b"\x00\x00\x00", PROP_CC, 0x46_0001, b"\x00" * 8)
    return bytes(sqe)


def build_io_sqe(opc: int, cid: int, slba: int, nlb_zb: int, fuse: int = 0) -> bytes:
    sqe = bytearray(64)
    sqe[0] = opc
    sqe[1] = fuse & 0x3  # cdw0 bits 9:8 = FUSE
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")  # nsid=1
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb_zb.to_bytes(4, "little")
    return bytes(sqe)


def cqe_sc(psh: bytes) -> int:
    return (int.from_bytes(psh[14:16], "little") >> 1) & 0xFF


def cqe_cid(psh: bytes) -> int:
    return int.from_bytes(psh[12:14], "little")


def parse_r2t(psh: bytes):
    return (
        int.from_bytes(psh[0:2], "little"),  # cccid
        int.from_bytes(psh[2:4], "little"),  # ttag
        int.from_bytes(psh[4:8], "little"),  # offset
        int.from_bytes(psh[8:12], "little"),  # length
    )


def setup_admin(s: socket.socket) -> None:
    write_pdu(s, PDU_ICREQ, build_icreq())
    assert read_pdu(s)[0] == PDU_ICRESP
    write_pdu(s, PDU_CMD, build_connect_sqe(1, 0, 31, 10000), data=build_connect_data(), pdo=72)
    if cqe_sc(read_pdu(s)[1]) != 0:
        fail("admin Connect")
    write_pdu(s, PDU_CMD, build_cc_en())
    if cqe_sc(read_pdu(s)[1]) != 0:
        fail("CC.EN")
    ok("admin online + CC.EN=1")


def setup_io(host: str, port: int) -> socket.socket:
    s = socket.create_connection((host, port), timeout=10)
    write_pdu(s, PDU_ICREQ, build_icreq())
    assert read_pdu(s)[0] == PDU_ICRESP
    write_pdu(s, PDU_CMD, build_connect_sqe(0x101, 1, 31, 0), data=build_connect_data(), pdo=72)
    if cqe_sc(read_pdu(s)[1]) != 0:
        fail("IO Connect")
    ok("IO queue 1 online")
    return s


def io_write(s: socket.socket, cid: int, slba: int, payload: bytes) -> None:
    """单/多 LBA 512B write（非 fused）。"""
    nlb = (len(payload) // SECTOR) - 1
    write_pdu(s, PDU_CMD, build_io_sqe(0x01, cid, slba, nlb))
    sent = 0
    while sent < len(payload):
        pt, psh, _ = read_pdu(s)
        if pt != PDU_R2T:
            fail(f"write expected R2T got {pt:#x}")
        _, ttag, r2to, r2tl = parse_r2t(psh)
        h2c = struct.pack("<HHII4s", cid, ttag, r2to, r2tl, b"\x00" * 4)
        write_pdu_h2c(s, h2c, payload[r2to : r2to + r2tl], pdo=24, flags=0x04)
        sent += r2tl
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"write RSP pt={pt:#x} sc={cqe_sc(psh):#x}")


def io_read(s: socket.socket, cid: int, slba: int, byte_len: int) -> bytes:
    nlb = (byte_len // SECTOR) - 1
    write_pdu(s, PDU_CMD, build_io_sqe(0x02, cid, slba, nlb))
    got = b""
    while len(got) < byte_len:
        pt, _, data = read_pdu(s)
        if pt != PDU_C2H_DATA:
            fail(f"read expected C2HData got {pt:#x}")
        got += data
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"read RSP pt={pt:#x} sc={cqe_sc(psh):#x}")
    return got


def fused_cas(
    s: socket.socket,
    compare_cid: int,
    write_cid: int,
    slba: int,
    compare_data: bytes,
    write_data: bytes,
) -> dict:
    """发 fused Compare(fuse=01)+Write(fuse=10)，cccid-aware 应答 R2T，收双 CQE。
    返回 {cid: sc}。"""
    assert len(compare_data) == len(write_data) == SECTOR
    # FIRST=Compare(fuse=01)：target 暂存，无响应。SECOND=Write(fuse=10)：触发。
    write_pdu(s, PDU_CMD, build_io_sqe(0x05, compare_cid, slba, 0, fuse=1))
    write_pdu(s, PDU_CMD, build_io_sqe(0x01, write_cid, slba, 0, fuse=2))
    resps: dict = {}
    while len(resps) < 2:
        pt, psh, _ = read_pdu(s)
        if pt == PDU_R2T:
            cccid, ttag, r2to, r2tl = parse_r2t(psh)
            # R2T cccid 决定该轮是 Compare 数据还是 Write 数据
            data = compare_data if cccid == compare_cid else write_data
            h2c = struct.pack("<HHII4s", cccid, ttag, r2to, r2tl, b"\x00" * 4)
            write_pdu_h2c(s, h2c, data[r2to : r2to + r2tl], pdo=24, flags=0x04)
        elif pt == PDU_RSP:
            resps[cqe_cid(psh)] = cqe_sc(psh)
        else:
            fail(f"fused: unexpected pt={pt:#x}")
    return resps


def drive_responses(s: socket.socket, data_by_cid: dict, expect_cids: set) -> dict:
    """驱动响应循环：R2T → 按 cccid 发 data_by_cid[cccid]；CapsuleResp → 记
    {cid: sc}。收齐 expect_cids 即返回。用于状态机/对抗场景（任意组合 capsule）。"""
    resps: dict = {}
    remaining = set(expect_cids)
    while remaining:
        pt, psh, _ = read_pdu(s)
        if pt == PDU_R2T:
            cccid, ttag, r2to, r2tl = parse_r2t(psh)
            data = data_by_cid[cccid]
            h2c = struct.pack("<HHII4s", cccid, ttag, r2to, r2tl, b"\x00" * 4)
            write_pdu_h2c(s, h2c, data[r2to : r2to + r2tl], pdo=24, flags=0x04)
        elif pt == PDU_RSP:
            c = cqe_cid(psh)
            resps[c] = cqe_sc(psh)
            remaining.discard(c)
        else:
            fail(f"drive: unexpected pt={pt:#x}")
    return resps


def main() -> None:
    print("=== Fused Compare-and-Write over fabric e2e (原子 CAS) ===")
    admin = socket.create_connection((HOST, PORT), timeout=10)
    setup_admin(admin)
    io = setup_io(HOST, PORT)

    slba = 5
    p0 = bytes([0xC0]) * SECTOR
    p1 = bytes([0xC1]) * SECTOR
    p2 = bytes([0xC2]) * SECTOR

    # [1] 预写 LBA 5 = P0
    print("\n[1] 预写 LBA 5 = P0(0xC0)")
    io_write(io, 0x200, slba, p0)
    if io_read(io, 0x201, slba, SECTOR) != p0:
        fail("预写 P0 校验失败")
    ok("LBA 5 = P0")

    # [2] 原子 CAS 成功：Compare(P0) 匹配 → Write(P1)
    print("\n[2] CAS：Compare(P0) 匹配 backing(P0) → Write(P1)")
    r = fused_cas(io, compare_cid=0x300, write_cid=0x301, slba=slba, compare_data=p0, write_data=p1)
    if r.get(0x300) != 0 or r.get(0x301) != 0:
        fail(f"CAS 成功对双 CQE 应 SC=0，实得 {r}")
    ok(f"双 CQE SC=0 (Compare={r[0x300]:#x} Write={r[0x301]:#x})")
    if io_read(io, 0x302, slba, SECTOR) != p1:
        fail("CAS 成功后 LBA 5 应 = P1")
    ok("[3] CAS 成功：LBA 5 = P1（Write 生效）")

    # [4] 原子 CAS 失败：Compare(P0) vs backing(P1) 不匹配 → Write(P2) 不写盘
    print("\n[4] CAS：Compare(P0) vs backing(P1) 不匹配 → 双 COMPARE_FAILURE，Write 不写")
    r = fused_cas(io, compare_cid=0x400, write_cid=0x401, slba=slba, compare_data=p0, write_data=p2)
    if r.get(0x400) != COMPARE_FAILURE or r.get(0x401) != COMPARE_FAILURE:
        fail(f"CAS 失败对双 CQE 应 SC=0x85，实得 {r}")
    ok(f"双 CQE SC=0x85 (Compare={r[0x400]:#x} Write={r[0x401]:#x})")
    back = io_read(io, 0x402, slba, SECTOR)
    if back == p2:
        fail("**原子性破坏**：Compare 失败但 Write(P2) 仍写盘了！")
    if back != p1:
        fail(f"CAS 失败后 LBA 5 应仍 = P1，实得首字节 {back[0]:#x}")
    ok("[5] CAS 失败：LBA 5 仍 = P1（Write **未** 生效 → 原子 CAS 成立）")

    # [6] 状态机：fuse=10(Write) 无前置 FIRST → INVALID_FIELD(0x02)
    print("\n[6] 状态机：SECOND 无 FIRST → INVALID_FIELD")
    write_pdu(io, PDU_CMD, build_io_sqe(0x01, 0x700, slba, 0, fuse=2))
    r = drive_responses(io, {}, {0x700})
    if r[0x700] != 0x02:
        fail(f"SECOND 无 FIRST 应 SC=0x02，实得 {r[0x700]:#x}")
    ok("SECOND 无 FIRST → SC=0x02")

    # [7] 状态机：fused 对 nsid/slba/nlb 不匹配 → 双 INVALID_FIELD
    print("\n[7] 状态机：Compare(slba=5) + Write(slba=6) 不匹配 → 双 INVALID_FIELD")
    write_pdu(io, PDU_CMD, build_io_sqe(0x05, 0x710, slba, 0, fuse=1))
    write_pdu(io, PDU_CMD, build_io_sqe(0x01, 0x711, slba + 1, 0, fuse=2))
    r = drive_responses(io, {}, {0x710, 0x711})
    if r[0x710] != 0x02 or r[0x711] != 0x02:
        fail(f"不匹配 fused 对应双 SC=0x02，实得 {r}")
    ok("不匹配 fused 对 → 双 SC=0x02")

    # [8] 状态机（reviewer HIGH-1）：连续两 FIRST → 旧 A 被打断 abort，新 B 正确
    # 配对（buffer 不串）。backing 当前 = P1（[2] 后），B 比 P1 匹配 → Write P2。
    print("\n[8] 状态机(HIGH-1)：Compare A + Compare B + Write → A abort + B(P1)+Write(P2)")
    write_pdu(io, PDU_CMD, build_io_sqe(0x05, 0x800, slba, 0, fuse=1))  # Compare A
    write_pdu(io, PDU_CMD, build_io_sqe(0x05, 0x801, slba, 0, fuse=1))  # Compare B（打断 A）
    write_pdu(io, PDU_CMD, build_io_sqe(0x01, 0x802, slba, 0, fuse=2))  # Write
    # B 的 compare 数据走 R2T(0x801)、Write 数据走 R2T(0x802)；A(0x800) abort 无 R2T。
    r = drive_responses(io, {0x801: p1, 0x802: p2}, {0x800, 0x801, 0x802})
    if r[0x800] != 0x02:
        fail(f"两 FIRST：A 应被打断 abort SC=0x02，实得 {r[0x800]:#x}")
    if r[0x801] != 0 or r[0x802] != 0:
        fail(f"两 FIRST：B+Write 应成功(B 比 P1 匹配)，实得 B={r[0x801]:#x} W={r[0x802]:#x}")
    ok("A 被打断 abort + B 正确配对（compare buffer 未串 cccid → HIGH-1 锁定）")
    if io_read(io, 0x803, slba, SECTOR) != p2:
        fail("两 FIRST 后 LBA 5 应 = P2（B 的 Write 生效）")
    ok("[9] LBA 5 = P2（B 的 Write 正确落盘）")

    admin.close()
    io.close()
    print(
        "\n✅ Fused C&W over fabric 通过：原子 CAS(匹配→写/不匹配→不写) + 状态机"
        "(SECOND-无-FIRST / 不匹配 / 两-FIRST 打断 各正确)"
    )


if __name__ == "__main__":
    main()
