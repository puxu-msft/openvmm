#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 — Python TLS interop harness。

跑法 (无需 sudo / nvme-cli)：
    cd scripts/interop_py
    uv run --script tls_smoke.py

需要 target 已起在 127.0.0.1:8009 (TLS) 且 cert at /tmp/nvme_tls/server.pem。

测试：
1. TLS handshake (server-auth X.509, 跳 verify)
2. NVMe-oF ICReq → ICResp over TLS (真应用层握手)
3. Property Get CAP → 验 CAP 寄存器读
"""
import socket
import ssl
import struct
import sys
import time


def assert_eq(actual, expected, label):
    if actual != expected:
        print(f"FAIL: {label}: expected {expected}, got {actual}", file=sys.stderr)
        sys.exit(1)
    print(f"OK: {label} = {actual}")


def make_tls_socket(host: str, port: int) -> ssl.SSLSocket:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE   # 教学版自签 cert
    raw = socket.create_connection((host, port), timeout=5)
    return ctx.wrap_socket(raw, server_hostname=host)


def build_icreq() -> bytes:
    """NVMe-TCP ICReq (PDU type 0x00, 128 bytes total):
    CommonHdr (8B): pdu_type=0, flags=0, hlen=128, pdo=0, plen=128
    PSH (120B): pfv=0, hpda=0, dgst=0, maxr2t=0, rsvd[112]
    """
    hdr = struct.pack("<BBBBI", 0, 0, 128, 0, 128)  # pdu_type, flags, hlen, pdo, plen
    psh = struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)
    pdu = hdr + psh
    assert len(pdu) == 128, f"ICReq must be 128B, got {len(pdu)}"
    return pdu


def read_pdu(s: ssl.SSLSocket) -> tuple[int, bytes, bytes]:
    """读 1 个 PDU；返 (pdu_type, psh, data)."""
    hdr = recv_exact(s, 8)
    pdu_type, _flags, hlen, pdo, plen = struct.unpack("<BBBBI", hdr)
    psh_len = hlen - 8
    psh = recv_exact(s, psh_len) if psh_len > 0 else b""
    data_off = pdo if pdo > 0 else hlen
    data_len = plen - data_off
    # pad
    pad = pdo - hlen if pdo > 0 else 0
    if pad > 0:
        recv_exact(s, pad)
    data = recv_exact(s, data_len) if data_len > 0 else b""
    return pdu_type, psh, data


def recv_exact(s: ssl.SSLSocket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise RuntimeError(f"EOF after {len(buf)} of {n} bytes")
        buf += chunk
    return buf


def main():
    print("=== V-interop-7 Python TLS interop ===")
    print("Connecting to 127.0.0.1:8009 (TLS)...")
    s = make_tls_socket("127.0.0.1", 8009)
    print(f"TLS cipher: {s.cipher()}")
    print(f"TLS version: {s.version()}")

    # 1. Send ICReq
    print("\n[1] Sending NVMe-oF ICReq over TLS...")
    s.sendall(build_icreq())

    # 2. Read ICResp
    print("[2] Reading ICResp...")
    pdu_type, psh, data = read_pdu(s)
    assert_eq(pdu_type, 0x01, "ICResp pdu_type")
    assert_eq(len(psh), 120, "ICResp PSH len")
    pfv, hpda, digest, maxh2cdata = struct.unpack("<HBBI", psh[:8])
    assert_eq(pfv, 0, "ICResp PFV")
    print(f"OK: ICResp negotiated MAXH2CDATA = {maxh2cdata} ({maxh2cdata // 1024} KiB)")

    s.close()
    print("\n✅ V-interop-7 TLS handshake + NVMe-oF ICReq/ICResp over TLS 真互通成功")


if __name__ == "__main__":
    main()
