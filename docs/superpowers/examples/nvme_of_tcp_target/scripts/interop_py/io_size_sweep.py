#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 IO size sweep — 探当前 MDTS=1 / V5_NLB_MAX=16 真实
上限。验证 Read + Write 在每个 LBA count 上 byte-equal，>16 LBA 应 reject。

跑法:
    uv run python io_size_sweep.py
"""
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 4420
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:sweep-host"
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
    psh = recv_exact(s, hlen - 8) if hlen > 8 else b""
    consumed = hlen
    if pdo > consumed:
        recv_exact(s, pdo - consumed)
        consumed = pdo
    data = recv_exact(s, plen - consumed) if plen > consumed else b""
    return pdu_type, psh, data


def write_pdu(s, pdu_type, psh, data=b"", pdo=0, flags=0):
    hlen = 8 + len(psh)
    if pdo == 0:
        plen = hlen + len(data)
        body = psh + data
    else:
        pad = pdo - hlen
        plen = pdo + len(data)
        body = psh + (b"\x00" * pad) + data
    hdr = struct.pack("<BBBBI", pdu_type, flags, hlen, pdo, plen)
    s.sendall(hdr + body)


def cqe_sc(psh):
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def parse_r2t_psh(psh):
    cccid = int.from_bytes(psh[0:2], "little")
    ttag = int.from_bytes(psh[2:4], "little")
    r2to = int.from_bytes(psh[4:8], "little")
    r2tl = int.from_bytes(psh[8:12], "little")
    return cccid, ttag, r2to, r2tl


def setup_admin_and_io():
    admin = socket.create_connection((HOST, PORT), timeout=10)
    write_pdu(admin, PDU_ICREQ, struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112))
    pt, _, _ = read_pdu(admin)
    assert pt == PDU_ICRESP

    def fabric_connect_sqe(cid, qid, kato):
        sqe = bytearray(64)
        sqe[0] = NVME_OPC_FABRIC
        sqe[2:4] = cid.to_bytes(2, "little")
        sqe[4] = FCTYPE_CONNECT
        sqe[40:64] = struct.pack(
            "<HHHBBI12s", 0, qid, 31, 0, 0, kato, b"\x00" * 12
        )
        return bytes(sqe)

    def connect_data(hn):
        buf = bytearray(1024)
        buf[16:18] = b"\xff\xff"
        sn = SUBNQN.encode("ascii")
        hnb = hn.encode("ascii")
        buf[256 : 256 + len(sn)] = sn
        buf[512 : 512 + len(hnb)] = hnb
        return bytes(buf)

    write_pdu(admin, PDU_CMD, fabric_connect_sqe(1, 0, 10000), data=connect_data(HOSTNQN), pdo=72)
    pt, psh, _ = read_pdu(admin)
    assert cqe_sc(psh) == 0
    # CC.EN
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = (2).to_bytes(2, "little")
    sqe[4] = FCTYPE_PROPERTY_SET
    sqe[40:64] = struct.pack("<B3sIQ8s", 0, b"\x00\x00\x00", PROP_CC, 0x46_0001, b"\x00" * 8)
    write_pdu(admin, PDU_CMD, bytes(sqe))
    pt, psh, _ = read_pdu(admin)
    assert cqe_sc(psh) == 0

    # IO conn qid=1
    io = socket.create_connection((HOST, PORT), timeout=10)
    write_pdu(io, PDU_ICREQ, struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112))
    pt, _, _ = read_pdu(io)
    assert pt == PDU_ICRESP
    write_pdu(io, PDU_CMD, fabric_connect_sqe(0x101, 1, 0), data=connect_data(HOSTNQN), pdo=72)
    pt, psh, _ = read_pdu(io)
    assert cqe_sc(psh) == 0, f"IO Connect SC={cqe_sc(psh):#x}"
    return admin, io


def io_write(s, cid, slba, payload):
    nlb = (len(payload) // 512) - 1
    sqe = bytearray(64)
    sqe[0] = 0x01
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb.to_bytes(4, "little")
    write_pdu(s, PDU_CMD, bytes(sqe))
    total = 0
    while total < len(payload):
        pt, psh, _ = read_pdu(s)
        if pt == PDU_RSP:
            return cqe_sc(psh)
        if pt != PDU_R2T:
            return -1
        _, ttag, r2to, r2tl = parse_r2t_psh(psh)
        chunk = payload[r2to : r2to + r2tl]
        h2c_psh = struct.pack("<HHII4s", cid, ttag, r2to, r2tl, b"\x00" * 4)
        write_pdu(s, PDU_H2C_DATA, h2c_psh, data=chunk, pdo=24, flags=0x04)
        total += r2tl
    pt, psh, _ = read_pdu(s)
    return cqe_sc(psh) if pt == PDU_RSP else -1


def io_read(s, cid, slba, byte_len):
    nlb = (byte_len // 512) - 1
    sqe = bytearray(64)
    sqe[0] = 0x02
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb.to_bytes(4, "little")
    write_pdu(s, PDU_CMD, bytes(sqe))
    pt1, _, data = read_pdu(s)
    if pt1 == PDU_RSP:
        return cqe_sc(_), b""  # noqa - error path
    if pt1 != PDU_C2H_DATA:
        return -1, b""
    pt2, psh, _ = read_pdu(s)
    if pt2 != PDU_RSP:
        return -1, b""
    return cqe_sc(psh), data


def main():
    print("=== V-interop-7 IO size sweep (V5_NLB_MAX=16, 8 KiB cap) ===")
    admin, io = setup_admin_and_io()

    # 测每个 nlb 1..=17
    cid = 0x500
    base_slba = 1000
    print("\nLBA  bytes  Write    Read     match")
    print("-" * 50)
    for nlb in range(1, 18):  # 1..17
        cid += 1
        payload = bytes(((b * 7 + nlb) & 0xFF) for b in range(nlb * 512))
        w_sc = io_write(io, cid, base_slba + nlb * 20, payload)
        cid += 1
        if w_sc == 0:
            r_sc, data = io_read(io, cid, base_slba + nlb * 20, nlb * 512)
            match = "✓" if data == payload else "✗"
            print(f"  {nlb:2d} {nlb * 512:5d}  SC={w_sc:#04x}  SC={r_sc:#04x}  {match}")
            if data != payload:
                fail(f"nlb={nlb} byte mismatch")
        else:
            # nlb > 16 expected SC=0x18 SGL_DATA_LENGTH_INVALID
            print(f"  {nlb:2d} {nlb * 512:5d}  SC={w_sc:#04x}  (skip)  -")
            if nlb <= 16:
                fail(f"nlb={nlb} 应通过 (V5_NLB_MAX=16) but got SC={w_sc:#x}")
            if nlb == 17 and w_sc != 0x18:
                fail(f"nlb=17 应 SC=0x18 (SGL_DATA_LENGTH_INVALID), got {w_sc:#x}")

    admin.close()
    io.close()
    print("\n✅ V-interop-7 IO size sweep: 1..16 LBA byte-equal, 17 LBA SC=0x18")
    print("    confirms V5_NLB_MAX=16 cap + V-followup-prp-list 未实现")


if __name__ == "__main__":
    main()
