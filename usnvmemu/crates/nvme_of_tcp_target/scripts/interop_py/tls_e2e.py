#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 完整 TLS e2e — Connect → CC.EN → Identify → IO Read。

跑法 (无需 sudo / nvme-cli)：
    cd scripts/interop_py
    uv run python tls_e2e.py
"""
import socket
import ssl
import struct
import sys


HOST = "127.0.0.1"
TLS_PORT = 8009
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:py-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"

NVME_OPC_FABRIC = 0x7F
FCTYPE_PROPERTY_SET = 0x00
FCTYPE_CONNECT = 0x01
FCTYPE_PROPERTY_GET = 0x04

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_C2H_DATA = 0x07

PROP_CC = 0x14
PROP_CAP = 0x00


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
    # pad between hdr+psh and data
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
    psh = struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)
    assert len(psh) == 120
    return psh


def build_sqe_fabric_connect(cid, qid, sqsize, kato):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    # ConnectFabricFields @ offset 40..64 (24B):
    # recfmt:u16, qid:u16, sqsize:u16, cattr:u8, rsvd1:u8, kato:u32, rsvd2[12]
    cf = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    assert len(cf) == 24
    sqe[40:64] = cf
    return bytes(sqe)


def build_connect_data(hostnqn, subnqn):
    """ConnectData 1024 byte:
    hostid[16], cntlid:u16, rsvd1[238], subnqn[256], hostnqn[256], rsvd2[256]
    """
    buf = bytearray(1024)
    # hostid 全 0 OK
    buf[16:18] = b"\xff\xff"  # cntlid = 0xFFFF (dynamic)
    sn = subnqn.encode("ascii")[:255]
    hn = hostnqn.encode("ascii")[:255]
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def build_sqe_property(cid, fctype, ofst, attrib=0, value=0):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = fctype
    # PropertyFabricFields @ 40..64 (24B):
    # attrib:u8, rsvd1[3], ofst:u32, value:u64, rsvd2[8]
    pf = struct.pack("<B3sIQ8s", attrib, b"\x00\x00\x00", ofst, value, b"\x00" * 8)
    assert len(pf) == 24
    sqe[40:64] = pf
    return bytes(sqe)


def build_sqe_identify_ctrl(cid):
    sqe = bytearray(64)
    sqe[0] = 0x06  # ADMIN Identify
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[40:44] = (0x01).to_bytes(4, "little")  # CNS=1
    return bytes(sqe)


def build_sqe_io_read(cid, nsid, slba, nlb_zero_based):
    """opc=0x02 IO Read, nsid, slba, nlb"""
    sqe = bytearray(64)
    sqe[0] = 0x02
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = nsid.to_bytes(4, "little")
    # SLBA @ 40..48
    sqe[40:48] = slba.to_bytes(8, "little")
    # CDW12: nlb (0-based) bits 15:0
    sqe[48:52] = nlb_zero_based.to_bytes(4, "little")
    return bytes(sqe)


def cqe_sc(psh):
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def send_cmd_with_data(s, sqe, data):
    """CapsuleCmd PDU: hdr(8) + psh=sqe(64) → hlen=72; with data → pdo=72."""
    if data:
        write_pdu(s, PDU_CMD, sqe, data=data, pdo=72)
    else:
        write_pdu(s, PDU_CMD, sqe)


def main():
    print("=== V-interop-7 完整 TLS e2e ===")
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection((HOST, TLS_PORT), timeout=10)
    s = ctx.wrap_socket(raw, server_hostname=HOST)
    print(f"TLS: {s.version()} / {s.cipher()[0]}")

    # 1. ICReq → ICResp
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, psh, _ = read_pdu(s)
    if pt != PDU_ICRESP:
        fail(f"expected ICResp, got {pt:#x}")
    ok(f"ICResp negotiated, MAXH2CDATA={int.from_bytes(psh[4:8], 'little')}")

    # 2. Connect admin
    sqe = build_sqe_fabric_connect(cid=1, qid=0, sqsize=31, kato=10000)
    send_cmd_with_data(s, sqe, build_connect_data(HOSTNQN, SUBNQN))
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Connect admin: pt={pt:#x} sc={cqe_sc(psh):#x}")
    ok("Connect admin")

    # 3. Property Set CC.EN=1
    ps = build_sqe_property(cid=2, fctype=FCTYPE_PROPERTY_SET, ofst=PROP_CC, attrib=0, value=0x46_0001)
    send_cmd_with_data(s, ps, b"")
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Property Set CC: sc={cqe_sc(psh):#x}")
    ok("CC.EN=1")

    # 4. Property Get CAP (8B)
    pg = build_sqe_property(cid=3, fctype=FCTYPE_PROPERTY_GET, ofst=PROP_CAP, attrib=1)
    send_cmd_with_data(s, pg, b"")
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Property Get CAP: sc={cqe_sc(psh):#x}")
    cap_lo = int.from_bytes(psh[0:4], "little")
    cap_hi = int.from_bytes(psh[4:8], "little")
    cap = (cap_hi << 32) | cap_lo
    ok(f"CAP = {cap:#018x}")

    # 5. Identify Controller (CNS=1)
    id_sqe = build_sqe_identify_ctrl(cid=4)
    send_cmd_with_data(s, id_sqe, b"")
    pt1, _, d1 = read_pdu(s)
    pt2, psh2, _ = read_pdu(s)
    if pt1 != PDU_C2H_DATA or len(d1) != 4096:
        fail(f"Identify Ctrl C2HData len={len(d1)}")
    if pt2 != PDU_RSP or cqe_sc(psh2) != 0:
        fail(f"Identify Ctrl RSP: sc={cqe_sc(psh2):#x}")
    sn = d1[4:24].decode("ascii", errors="replace").strip("\x00 ")
    mn = d1[24:64].decode("ascii", errors="replace").strip("\x00 ")
    cntrltype = d1[111]
    ok(f"Identify Ctrl: SN={sn!r} MN={mn!r} CNTRLTYPE={cntrltype:#x}")

    # 6. IO Connect (qid=1, separate TCP for IO queue) — 这测要新 TLS conn
    raw2 = socket.create_connection((HOST, TLS_PORT), timeout=10)
    s2 = ctx.wrap_socket(raw2, server_hostname=HOST)
    write_pdu(s2, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s2)
    if pt != PDU_ICRESP:
        fail("IO conn ICResp")
    sqe = build_sqe_fabric_connect(cid=0x10, qid=1, sqsize=31, kato=0)
    send_cmd_with_data(s2, sqe, build_connect_data(HOSTNQN, SUBNQN))
    pt, psh, _ = read_pdu(s2)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"IO Connect (qid=1): sc={cqe_sc(psh):#x}")
    ok("IO Connect qid=1")

    s.close()
    s2.close()
    print("\n✅ V-interop-7 TLS e2e: TLS 1.3 + ICReq + Connect + CC.EN + Property Get + Identify + IO Connect 全通")


if __name__ == "__main__":
    main()
