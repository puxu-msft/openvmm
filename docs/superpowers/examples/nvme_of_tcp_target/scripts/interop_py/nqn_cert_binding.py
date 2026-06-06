#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 NQN<->cert binding (V-followup-auth-2)。

3 scenarios:
1. client cert SAN URI = hostnqn → Connect 通 (SC=0)
2. client cert CN = hostnqn (无 SAN URI) → Connect 通 (CN fallback)
3. client cert SAN URI / CN 都 ≠ hostnqn → Connect 拒 SC=0x84

跑法 (前置: target 起 --tls-bind-nqn-to-cert):
    uv run python nqn_cert_binding.py
"""
import os
import socket
import ssl
import struct
import subprocess
import sys
import tempfile

HOST = "127.0.0.1"
PORT = 8009
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"
CERT_DIR = "/tmp/nvme_tls"

NVME_OPC_FABRIC = 0x7F
FCTYPE_CONNECT = 0x01
PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05


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


def build_connect_sqe(cid, qid, sqsize, kato):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    sqe[40:64] = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    return bytes(sqe)


def build_connect_data(hostnqn):
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


def gen_client_cert_with(san_uri: str | None, cn: str) -> tuple[str, str]:
    """生成 client cert 由 client-ca 签，SAN URI + CN 可控。"""
    tmp = tempfile.mkdtemp(prefix="nvme_nqn_test_")
    cnf = os.path.join(tmp, "client.cnf")
    with open(cnf, "w") as f:
        f.write("[req]\n")
        f.write("distinguished_name = dn\n")
        f.write("req_extensions = ext\n")
        f.write("prompt = no\n")
        f.write("[dn]\n")
        f.write(f"CN = {cn}\n")
        f.write("[ext]\n")
        if san_uri:
            f.write(f"subjectAltName = URI:{san_uri}\n")
        else:
            f.write("subjectAltName = DNS:placeholder\n")
    csr = os.path.join(tmp, "client.csr")
    key = os.path.join(tmp, "client.key")
    pem = os.path.join(tmp, "client.pem")
    subprocess.check_call(
        ["openssl", "req", "-new", "-newkey", "ec",
         "-pkeyopt", "ec_paramgen_curve:prime256v1",
         "-keyout", key, "-out", csr, "-nodes", "-config", cnf],
        stderr=subprocess.DEVNULL,
    )
    subprocess.check_call(
        ["openssl", "x509", "-req", "-in", csr,
         "-CA", f"{CERT_DIR}/client-ca.pem", "-CAkey", f"{CERT_DIR}/client-ca.key",
         "-out", pem, "-days", "1", "-CAcreateserial",
         "-extfile", cnf, "-extensions", "ext"],
        stderr=subprocess.DEVNULL,
    )
    return pem, key


def try_connect_with_cert(cert_path: str, key_path: str, hostnqn: str) -> int:
    """Connect with client cert, send Fabric Connect with hostnqn. Return CQE SC."""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.load_cert_chain(certfile=cert_path, keyfile=key_path)
    raw = socket.create_connection((HOST, PORT), timeout=5)
    s = ctx.wrap_socket(raw, server_hostname=HOST)
    try:
        write_pdu(s, PDU_ICREQ, build_icreq())
        pt, _, _ = read_pdu(s)
        if pt != PDU_ICRESP:
            return -1
        sqe = build_connect_sqe(0x01, 0, 31, 10000)
        write_pdu(s, PDU_CMD, sqe, data=build_connect_data(hostnqn), pdo=72)
        pt, psh, _ = read_pdu(s)
        if pt != PDU_RSP:
            return -1
        return cqe_sc(psh)
    finally:
        try:
            s.close()
        except Exception:
            pass


def main():
    print("=== V-interop-7 NQN <-> TLS cert SAN/CN binding ===")

    # Scenario 1: SAN URI = hostnqn → 通
    print("\n[1] SAN URI = hostnqn (should pass)")
    cert, key = gen_client_cert_with(
        san_uri="nqn.2014-08.org.nvmexpress:uuid:scenario-1",
        cn="placeholder",
    )
    sc = try_connect_with_cert(
        cert, key, hostnqn="nqn.2014-08.org.nvmexpress:uuid:scenario-1"
    )
    if sc != 0:
        fail(f"SAN URI 命中应 pass, got SC={sc:#x}")
    ok(f"SAN URI = hostnqn → Connect SC=0")

    # Scenario 2: CN = hostnqn (无 SAN URI)
    print("\n[2] CN = hostnqn fallback (无 SAN URI, should pass)")
    cert, key = gen_client_cert_with(
        san_uri=None,
        cn="nqn.2014-08.org.nvmexpress:uuid:scenario-2",
    )
    sc = try_connect_with_cert(
        cert, key, hostnqn="nqn.2014-08.org.nvmexpress:uuid:scenario-2"
    )
    if sc != 0:
        fail(f"CN 命中应 pass, got SC={sc:#x}")
    ok(f"CN = hostnqn (无 SAN URI) → Connect SC=0")

    # Scenario 3: cert 完全不含 hostnqn → 拒
    print("\n[3] cert SAN/CN 都 != hostnqn (should reject with SC=0x84)")
    cert, key = gen_client_cert_with(
        san_uri="nqn.legit.host",
        cn="legit-host",
    )
    sc = try_connect_with_cert(
        cert, key, hostnqn="nqn.evil.impersonator"
    )
    if sc != 0x84:
        fail(f"cert 不含 hostnqn 应 SC=0x84 (CONNECT_INVALID_HOST), got {sc:#x}")
    ok(f"cert 不含 hostnqn → Connect SC=0x84 (CONNECT_INVALID_HOST)")

    print("\n✅ V-interop-7 NQN<->cert binding: 3 scenarios all behaved as expected")


if __name__ == "__main__":
    main()
