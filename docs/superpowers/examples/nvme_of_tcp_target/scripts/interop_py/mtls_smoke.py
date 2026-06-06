#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 mTLS — 强制 client cert chain (V-followup-mtls)。

3 个测试:
1. 合法 client cert (CA-signed) → handshake + ICReq 通
2. 无 client cert → handshake fail
3. unrelated CA 签的 client cert → handshake fail

跑法:
    uv run python mtls_smoke.py
"""
import os
import socket
import ssl
import struct
import subprocess
import sys
import tempfile


HOST = "127.0.0.1"
TLS_PORT = 8009
CERT_DIR = "/tmp/nvme_tls"


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


def make_ctx_with_client_cert(cert_path: str, key_path: str) -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.load_cert_chain(certfile=cert_path, keyfile=key_path)
    return ctx


def make_ctx_no_client_cert() -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx


def try_handshake_and_icreq(ctx: ssl.SSLContext) -> str:
    """返 'ok' / 'handshake-fail' / 'icreq-fail'."""
    try:
        raw = socket.create_connection((HOST, TLS_PORT), timeout=5)
        s = ctx.wrap_socket(raw, server_hostname=HOST)
    except (ssl.SSLError, OSError) as e:
        return f"handshake-fail: {e}"
    try:
        # 发 ICReq, 期 ICResp
        hdr = struct.pack("<BBBBI", 0, 0, 128, 0, 128)
        psh = struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)
        s.sendall(hdr + psh)
        hdr = recv_exact(s, 8)
        pdu_type = hdr[0]
        if pdu_type != 0x01:
            return f"icreq-fail: pdu_type={pdu_type:#x}"
        recv_exact(s, 120)  # PSH
        return "ok"
    except (ssl.SSLError, OSError, RuntimeError) as e:
        return f"icreq-fail: {e}"
    finally:
        try:
            s.close()
        except Exception:
            pass


def gen_unknown_ca_client_cert() -> tuple[str, str]:
    """生成 unrelated CA + client cert，返 (cert_path, key_path)."""
    tmp = tempfile.mkdtemp(prefix="nvme_mtls_test_")
    ca_key = os.path.join(tmp, "evil-ca.key")
    ca_pem = os.path.join(tmp, "evil-ca.pem")
    c_key = os.path.join(tmp, "evil.key")
    c_csr = os.path.join(tmp, "evil.csr")
    c_pem = os.path.join(tmp, "evil.pem")
    subprocess.check_call(
        [
            "openssl", "req", "-x509", "-newkey", "ec",
            "-pkeyopt", "ec_paramgen_curve:prime256v1",
            "-keyout", ca_key, "-out", ca_pem, "-days", "1", "-nodes",
            "-subj", "/CN=evil-ca",
        ],
        stderr=subprocess.DEVNULL,
    )
    subprocess.check_call(
        [
            "openssl", "req", "-new", "-newkey", "ec",
            "-pkeyopt", "ec_paramgen_curve:prime256v1",
            "-keyout", c_key, "-out", c_csr, "-nodes",
            "-subj", "/CN=evil",
        ],
        stderr=subprocess.DEVNULL,
    )
    subprocess.check_call(
        [
            "openssl", "x509", "-req", "-in", c_csr,
            "-CA", ca_pem, "-CAkey", ca_key, "-out", c_pem,
            "-days", "1", "-CAcreateserial",
        ],
        stderr=subprocess.DEVNULL,
    )
    return c_pem, c_key


def main():
    print("=== V-interop-7 mTLS interop ===")

    # 1. 合法 client cert
    print("\n[1] CA-signed client cert (should succeed)")
    ctx = make_ctx_with_client_cert(
        f"{CERT_DIR}/client.pem", f"{CERT_DIR}/client.key"
    )
    r = try_handshake_and_icreq(ctx)
    if r != "ok":
        fail(f"legit cert should succeed, got: {r}")
    ok("legit client cert → handshake + ICReq 通")

    # 2. 无 client cert
    print("\n[2] No client cert (should fail)")
    ctx = make_ctx_no_client_cert()
    r = try_handshake_and_icreq(ctx)
    if r == "ok":
        fail("no-cert client should fail but succeeded")
    ok(f"no-cert client → rejected: {r[:80]}...")

    # 3. unrelated CA signed client cert
    print("\n[3] Unrelated CA client cert (should fail)")
    evil_cert, evil_key = gen_unknown_ca_client_cert()
    ctx = make_ctx_with_client_cert(evil_cert, evil_key)
    r = try_handshake_and_icreq(ctx)
    if r == "ok":
        fail("evil cert should fail but succeeded")
    ok(f"evil cert → rejected: {r[:80]}...")

    print("\n✅ V-interop-7 mTLS: 3 scenarios all behaved as expected")


if __name__ == "__main__":
    main()
