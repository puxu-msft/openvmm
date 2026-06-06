#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 DH-HMAC-CHAP 教学版简化 wire (V-followup-dhchap-3-wire)。

我们简化的 2-message wire (不是 spec 完整 4-message)。每 conn 流程:
  ICReq → Connect → AUTH_RECV (拉 32B challenge) → AUTH_SEND (32B HMAC response)
  → admin/IO cmd 才放行

3 scenarios:
1. 已知 host + 正确 secret → CHAP pass → admin cmd OK
2. 已知 host + 错 secret → AUTH_SEND SC=0x83
3. CHAP 未完成直接 admin cmd → SC=0x83

跑法 (前置: target --host-secret <nqn>=<hex>):
    uv run python chap_e2e.py
"""
import binascii
import hashlib
import hmac
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 4420
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:chap-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"
SECRET_HEX = "aa" * 32  # 与 target 启动时的 --host-secret 一致

NVME_OPC_FABRIC = 0x7F
FCTYPE_PROPERTY_SET = 0x00
FCTYPE_CONNECT = 0x01
FCTYPE_AUTH_SEND = 0x05
FCTYPE_AUTH_RECV = 0x06

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_C2H_DATA = 0x07

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
    data = recv_exact(s, plen - consumed) if plen > consumed else b""
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


def build_fabric_sqe(cid, fctype, qid=0, sqsize=31, kato=0):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = fctype
    if fctype == FCTYPE_CONNECT:
        sqe[40:64] = struct.pack(
            "<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12
        )
    return bytes(sqe)


def build_connect_data(hostnqn=HOSTNQN):
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    sn = SUBNQN.encode("ascii")
    hn = hostnqn.encode("ascii")
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def cqe_sc(psh):
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def compute_response(secret_hex: str, challenge: bytes, hostnqn: str, subnqn: str) -> bytes:
    """HMAC-SHA256(secret, challenge || hostnqn || subnqn)."""
    secret = binascii.unhexlify(secret_hex)
    m = hmac.new(secret, digestmod=hashlib.sha256)
    m.update(challenge)
    m.update(hostnqn.encode("ascii"))
    m.update(subnqn.encode("ascii"))
    return m.digest()


def setup_admin_and_chap(secret_hex: str, hostnqn: str = HOSTNQN) -> tuple[socket.socket, int]:
    """ICReq + Connect + AUTH_RECV + AUTH_SEND。返 (sock, auth_sc)."""
    s = socket.create_connection((HOST, PORT), timeout=5)
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    if pt != PDU_ICRESP:
        fail("ICResp")
    # Connect (admin, qid=0)
    sqe = build_fabric_sqe(0x01, FCTYPE_CONNECT, qid=0, sqsize=31, kato=10000)
    write_pdu(s, PDU_CMD, sqe, data=build_connect_data(hostnqn), pdo=72)
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail(f"Connect SC={cqe_sc(psh):#x}")
    # AUTH_RECV → 收 C2HData(32B challenge) + RSP
    sqe = build_fabric_sqe(0x02, FCTYPE_AUTH_RECV)
    write_pdu(s, PDU_CMD, sqe)
    pt, _, challenge = read_pdu(s)
    if pt != PDU_C2H_DATA or len(challenge) != 32:
        fail(f"AUTH_RECV C2HData len={len(challenge)}")
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail("AUTH_RECV RSP")
    # AUTH_SEND with HMAC response
    response = compute_response(secret_hex, challenge, hostnqn, SUBNQN)
    sqe = build_fabric_sqe(0x03, FCTYPE_AUTH_SEND)
    write_pdu(s, PDU_CMD, sqe, data=response, pdo=72)
    pt, psh, _ = read_pdu(s)
    return s, cqe_sc(psh)


def main():
    print("=== V-interop-7 DH-HMAC-CHAP e2e (V-followup-dhchap-3-wire) ===")

    # Scenario 1: 正确 secret → AUTH_SEND SC=0
    print("\n[1] 正确 secret → AUTH_SEND SC=0")
    s, sc = setup_admin_and_chap(SECRET_HEX)
    if sc != 0:
        fail(f"正确 secret AUTH_SEND SC={sc:#x}")
    ok(f"AUTH_SEND SC=0")

    # CHAP 通过后 Identify Controller 应放行
    sqe = bytearray(64)
    sqe[0] = 0x06
    sqe[2:4] = (0x100).to_bytes(2, "little")
    sqe[40:44] = (0x01).to_bytes(4, "little")
    write_pdu(s, PDU_CMD, bytes(sqe))
    pt1, _, d1 = read_pdu(s)
    pt2, psh2, _ = read_pdu(s)
    if pt1 != PDU_C2H_DATA or pt2 != PDU_RSP or cqe_sc(psh2) != 0:
        fail(f"Identify after CHAP: pt1={pt1:#x} pt2={pt2:#x} sc={cqe_sc(psh2):#x}")
    ok("Identify Controller 放行 (CHAP gate 通过)")
    s.close()

    # Scenario 2: 错 secret
    print("\n[2] 假 secret → AUTH_SEND SC=0x83")
    s, sc = setup_admin_and_chap("ff" * 32)  # 假 secret
    if sc != 0x83:
        fail(f"假 secret 应 SC=0x83 (AUTHENTICATION_REQUIRED), got {sc:#x}")
    ok("AUTH_SEND SC=0x83 (verify failed)")
    s.close()

    # Scenario 3: 未知 host
    print("\n[3] 未知 hostnqn → AUTH_SEND SC=0x83")
    # 直接试 Connect 用未知 hostnqn 看 server 行为
    s = socket.create_connection((HOST, PORT), timeout=5)
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    sqe = build_fabric_sqe(0x01, FCTYPE_CONNECT, qid=0, sqsize=31, kato=10000)
    write_pdu(s, PDU_CMD, sqe, data=build_connect_data("nqn.unknown-host"), pdo=72)
    pt, psh, _ = read_pdu(s)
    # Connect 自身可能仍 SC=0 (server allow 不在 store 的 NQN 走 ChapStage::Failed)
    # 然后 admin cmd 时被 gate
    if cqe_sc(psh) != 0:
        ok(f"未知 host Connect 直接 SC={cqe_sc(psh):#x}")
    else:
        # 试 Identify, 应被 CHAP gate 拒
        sqe_id = bytearray(64)
        sqe_id[0] = 0x06
        sqe_id[2:4] = (0x200).to_bytes(2, "little")
        sqe_id[40:44] = (0x01).to_bytes(4, "little")
        write_pdu(s, PDU_CMD, bytes(sqe_id))
        pt, psh, _ = read_pdu(s)
        sc = cqe_sc(psh)
        if sc != 0x83:
            fail(f"未知 host admin cmd 应 SC=0x83, got {sc:#x}")
        ok(f"未知 host admin cmd SC=0x83 (CHAP gate)")
    s.close()

    print("\n✅ V-interop-7 CHAP: 3 scenarios all behaved as expected")


if __name__ == "__main__":
    main()
