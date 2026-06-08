#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 IO Write e2e — R2T + H2CData 完整流程 (无 sudo)。

NVMe-oF IO Write 流程:
  host → target: CapsuleCmd (opc=0x01 Write, SLBA, NLB)
  target → host: R2T (request 4KB)
  host → target: H2CData (LBADS=9 → 8 LBA = 4 KiB write data)
  target → host: CapsuleResp SC=0

测试: 写 + 读回 byte-equal 确认 V5c IO Write 路径真互通。

跑法:
    uv run python io_write_e2e.py
"""
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 4420
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:write-host"
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


def fail(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def ok(msg):
    print(f"OK: {msg}")


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise RuntimeError(f"EOF after {len(buf)}/{n}")
        buf += chunk
    return buf


def read_pdu(s):
    hdr = recv_exact(s, 8)
    pdu_type, _flags, hlen, pdo, plen = struct.unpack("<BBBBI", hdr)
    psh_len = hlen - 8
    psh = recv_exact(s, psh_len) if psh_len > 0 else b""
    consumed = hlen
    if pdo > consumed:
        recv_exact(s, pdo - consumed)
        consumed = pdo
    data_len = plen - consumed
    data = recv_exact(s, data_len) if data_len > 0 else b""
    return pdu_type, psh, data


def write_pdu(s, pdu_type, psh, data=b"", pdo=0):
    hlen = 8 + len(psh)
    if pdo == 0:
        plen = hlen + len(data)
        body = psh + data
    else:
        pad = pdo - hlen
        plen = pdo + len(data)
        body = psh + (b"\x00" * pad) + data
    hdr = struct.pack("<BBBBI", pdu_type, 0, hlen, pdo, plen)
    s.sendall(hdr + body)


def build_icreq():
    return struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)


def build_connect_sqe(cid, qid, sqsize, kato):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    sqe[40:64] = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    return bytes(sqe)


def build_connect_data():
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    sn = SUBNQN.encode("ascii")
    hn = HOSTNQN.encode("ascii")
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def build_cc_en():
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = (0x0002).to_bytes(2, "little")
    sqe[4] = FCTYPE_PROPERTY_SET
    sqe[40:64] = struct.pack("<B3sIQ8s", 0, b"\x00\x00\x00", PROP_CC, 0x46_0001, b"\x00" * 8)
    return bytes(sqe)


def build_io_rw(cid, opc, slba, nlb_zero_based):
    """opc=0x01 Write, 0x02 Read."""
    sqe = bytearray(64)
    sqe[0] = opc
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb_zero_based.to_bytes(4, "little")
    return bytes(sqe)


def cqe_sc(psh):
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def parse_r2t_psh(psh):
    """R2T PSH (16B):
    cccid: u16 (echoed from cmd CID)
    ttag:  u16 (controller-assigned tag, host must echo in H2CData)
    r2t_offset: u32 (offset into cmd data buffer)
    r2t_length: u32 (bytes host should send now)
    rsvd[4]
    """
    cccid = int.from_bytes(psh[0:2], "little")
    ttag = int.from_bytes(psh[2:4], "little")
    r2t_offset = int.from_bytes(psh[4:8], "little")
    r2t_length = int.from_bytes(psh[8:12], "little")
    return cccid, ttag, r2t_offset, r2t_length


def build_h2c_data_psh(cccid, ttag, datao, datal):
    """H2CData PSH (16B):
    cccid: u16 (echo SQE.cid)
    ttag:  u16 (echo from R2T)
    data_offset: u32 (offset in cmd data buffer)
    data_length: u32 (this PDU's data byte count)
    rsvd[4]
    """
    psh = struct.pack("<HHII4s", cccid, ttag, datao, datal, b"\x00" * 4)
    assert len(psh) == 16
    return psh


def setup_admin(s):
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    assert pt == PDU_ICRESP
    write_pdu(s, PDU_CMD, build_connect_sqe(1, 0, 31, 10000), data=build_connect_data(), pdo=72)
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail(f"admin Connect SC={cqe_sc(psh):#x}")
    write_pdu(s, PDU_CMD, build_cc_en())
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail("CC.EN")
    ok("admin online + CC.EN=1")


def setup_io(host, port):
    s = socket.create_connection((host, port), timeout=10)
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    assert pt == PDU_ICRESP
    write_pdu(
        s,
        PDU_CMD,
        build_connect_sqe(cid=0x101, qid=1, sqsize=31, kato=0),
        data=build_connect_data(),
        pdo=72,
    )
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail(f"IO Connect SC={cqe_sc(psh):#x}")
    ok("IO queue 1 online")
    return s


def io_write(s, cid, slba, payload):
    """Write payload (must be multiple of 512B, ≤ 8 KiB / 16 LBA).

    Loop R2T → H2CData until r2t_offset+r2t_length == payload len (V-followup-v4c
    MAXH2CDATA 分片，target 可能一次 R2T 仅 4 KiB)。
    """
    nlb = (len(payload) // 512) - 1  # zero-based
    sqe = build_io_rw(cid, 0x01, slba, nlb)
    write_pdu(s, PDU_CMD, sqe)
    total_sent = 0
    while total_sent < len(payload):
        # 收 R2T
        pt, psh, _ = read_pdu(s)
        if pt != PDU_R2T:
            fail(f"expected R2T (sent={total_sent}/{len(payload)}), got pt={pt:#x}")
        cccid, ttag, r2to, r2tl = parse_r2t_psh(psh)
        if cccid != cid:
            fail(f"R2T cccid={cccid:#x} != cmd cid={cid:#x}")
        chunk = payload[r2to : r2to + r2tl]
        if len(chunk) != r2tl:
            fail(f"R2T r2to={r2to} r2tl={r2tl} 超出 payload {len(payload)}")
        # 发 H2CData (LAST_DATA flag = bit 2 in PDU flags = 0x04)
        h2c_psh = build_h2c_data_psh(cid, ttag, r2to, r2tl)
        write_pdu_h2c(s, h2c_psh, chunk, pdo=24, flags=0x04)
        total_sent += r2tl
    # 收 RSP
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Write RSP: pt={pt:#x} sc={cqe_sc(psh):#x}")
    ok(f"  Write cid={cid:#x} slba={slba} len={len(payload)}")


def write_pdu_h2c(s, psh, data, pdo, flags):
    hlen = 8 + len(psh)
    pad = pdo - hlen
    plen = pdo + len(data)
    hdr = struct.pack("<BBBBI", PDU_H2C_DATA, flags, hlen, pdo, plen)
    s.sendall(hdr + psh + (b"\x00" * pad) + data)


def io_read(s, cid, slba, byte_len):
    nlb = (byte_len // 512) - 1
    sqe = build_io_rw(cid, 0x02, slba, nlb)
    write_pdu(s, PDU_CMD, sqe)
    pt1, _, data = read_pdu(s)
    if pt1 != PDU_C2H_DATA or len(data) != byte_len:
        fail(f"Read C2HData: pt={pt1:#x} len={len(data)}")
    pt2, psh, _ = read_pdu(s)
    if pt2 != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Read RSP: pt={pt2:#x} sc={cqe_sc(psh):#x}")
    ok(f"  Read  cid={cid:#x} slba={slba} len={byte_len}")
    return data


def main():
    print("=== V-interop-7 IO Write e2e (R2T + H2CData) ===")
    admin = socket.create_connection((HOST, PORT), timeout=10)
    setup_admin(admin)
    io = setup_io(HOST, PORT)

    # 写 4 KiB pattern → 读回 → byte-equal
    pattern = bytes(((b + 0x55) & 0xFF) for b in range(4096))
    print("\n[1] Write 4 KiB pattern @ SLBA=100")
    io_write(io, cid=0x500, slba=100, payload=pattern)

    print("\n[2] Read back 4 KiB @ SLBA=100")
    got = io_read(io, cid=0x501, slba=100, byte_len=4096)

    if got != pattern:
        # 找前 16 不同的 byte
        diffs = [
            (i, hex(pattern[i]), hex(got[i]))
            for i in range(min(len(pattern), len(got)))
            if pattern[i] != got[i]
        ][:16]
        fail(f"data mismatch — first diffs: {diffs}")
    ok("[3] Read data == written pattern (byte-equal)")

    # 多 LBA Write 8 KiB (16 LBA = V5_NLB_MAX 上限)
    pat2 = bytes(((b + 0xAA) & 0xFF) for b in range(8192))
    print("\n[4] Write 8 KiB (16 LBA) @ SLBA=200")
    io_write(io, cid=0x600, slba=200, payload=pat2)
    print("[5] Read back 8 KiB")
    got2 = io_read(io, cid=0x601, slba=200, byte_len=8192)
    if got2 != pat2:
        fail("8 KiB byte-mismatch")
    ok("[6] 8 KiB byte-equal")

    admin.close()
    io.close()
    print("\n✅ V-interop-7 IO Write + Read 端到端 byte-equal (4 KiB + 8 KiB)")


if __name__ == "__main__":
    main()
