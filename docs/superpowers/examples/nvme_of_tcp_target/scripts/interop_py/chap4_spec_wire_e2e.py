#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-8 DH-HMAC-CHAP spec § 8.13.5 4-message wire 跨进程实证。

跑 Rust target 后用纯 stdlib Python 当 host 跑：
  ICReq → Connect → AUTH_SEND(NEGOTIATE) → AUTH_RECV(CHALLENGE wire) →
  AUTH_SEND(REPLY HMAC) → AUTH_RECV(SUCCESS1 wire) → admin cmd 放行

覆盖：
1. happy path — 完整 4-msg + Identify Controller 通过
2. NEGOTIATE 无 SHA-256 → FAILURE1 wire + SC=0x83
3. REPLY 用错 secret → FAILURE1 wire (rescode_exp=FAILED) + SC=0x83
4. NEGOTIATE napd>1 多 descriptor (V-followup-dhchap-4d) — DH-2048+NULL 第二
   位匹配应通过

前置: 启 target with `--host-secret nqn.<host>=<hex>`:
    cargo run -p nvme_of_tcp_target -- \\
        --listen-tcp 127.0.0.1:4420 \\
        --backing /tmp/img \\
        --host-secret nqn.2014-08.org.nvmexpress:uuid:dhchap4-host=aaaaaa...

跑法 (in scripts/interop_py/):
    uv run python chap4_spec_wire_e2e.py
"""
import binascii
import hashlib
import hmac
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 4420  # 与其他 interop_py 脚本默认 port 对齐；改写后 cross-script 同步换
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:dhchap4-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"
SECRET_HEX = "aa" * 32

NVME_OPC_FABRIC = 0x7F
FCTYPE_CONNECT = 0x01
FCTYPE_AUTH_SEND = 0x05
FCTYPE_AUTH_RECV = 0x06

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_C2H_DATA = 0x07

# DHCHAP wire constants (= nvme_of_tcp_target::dhchap::wire)
AUTH_TYPE_DHCHAP = 0x01
MSG_NEGOTIATE = 0x00
MSG_CHALLENGE = 0x01
MSG_REPLY = 0x02
MSG_SUCCESS1 = 0x03
MSG_FAILURE1 = 0xF1

AUTH_DHCHAP = 0x01
HASH_SHA256 = 0x01
HASH_SHA384 = 0x02
DHGROUP_NULL = 0x00

FAIL_EXP_FAILED = 0x01
FAIL_EXP_HASH_UNUSABLE = 0x04


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


def read_pdu(s: socket.socket) -> tuple[int, bytes, bytes]:
    hdr = recv_exact(s, 8)
    pdu_type, _flags, hlen, pdo, plen = struct.unpack("<BBBBI", hdr)
    psh = recv_exact(s, hlen - 8) if hlen > 8 else b""
    consumed = hlen
    if pdo > consumed:
        recv_exact(s, pdo - consumed)
        consumed = pdo
    data = recv_exact(s, plen - consumed) if plen > consumed else b""
    return pdu_type, psh, data


def write_pdu(
    s: socket.socket, pdu_type: int, psh: bytes, data: bytes = b"", pdo: int = 0
) -> None:
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


def build_icreq() -> bytes:
    return struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)


def build_fabric_sqe(cid: int, fctype: int, qid: int = 0, sqsize: int = 31, kato: int = 0) -> bytes:
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = fctype
    if fctype == FCTYPE_CONNECT:
        sqe[40:64] = struct.pack(
            "<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12
        )
    return bytes(sqe)


def build_connect_data() -> bytes:
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    sn = SUBNQN.encode("ascii")
    hn = HOSTNQN.encode("ascii")
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def cqe_sc(psh: bytes) -> int:
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def compute_response(secret_hex: str, challenge: bytes, hostnqn: str, subnqn: str) -> bytes:
    """HMAC-SHA256(secret, challenge || hostnqn || subnqn) — 与 Rust 端 compute_response 一致。"""
    secret = binascii.unhexlify(secret_hex)
    m = hmac.new(secret, digestmod=hashlib.sha256)
    m.update(challenge)
    m.update(hostnqn.encode("ascii"))
    m.update(subnqn.encode("ascii"))
    return m.digest()


# ============== DHCHAP-4 spec wire builders/parsers ==============

def build_negotiate(tid: int, hash_ids: list[int], dh_ids: list[int]) -> bytes:
    """spec § 8.13.5.1 NEGOTIATE，1 个 DHCHAP descriptor。"""
    out = bytearray([
        AUTH_TYPE_DHCHAP, MSG_NEGOTIATE, 0, 0,
        tid & 0xFF, (tid >> 8) & 0xFF,
        0,  # sc_c
        1,  # napd
    ])
    out.extend([AUTH_DHCHAP, 0, len(hash_ids), len(dh_ids)])
    out.extend(hash_ids)
    out.extend(dh_ids)
    return bytes(out)


def build_negotiate_multi(tid: int, descriptors: list[tuple[int, list[int], list[int]]]) -> bytes:
    """V-followup-dhchap-4d: 多 descriptor + 8B padding。"""
    out = bytearray([
        AUTH_TYPE_DHCHAP, MSG_NEGOTIATE, 0, 0,
        tid & 0xFF, (tid >> 8) & 0xFF,
        0, len(descriptors),
    ])
    for authid, hash_ids, dh_ids in descriptors:
        out.extend([authid, 0, len(hash_ids), len(dh_ids)])
        out.extend(hash_ids)
        out.extend(dh_ids)
        raw = 4 + len(hash_ids) + len(dh_ids)
        padded = ((raw + 7) // 8) * 8
        out.extend(b"\x00" * (padded - raw))
    return bytes(out)


def build_reply(tid: int, rval: bytes) -> bytes:
    """spec § 8.13.5.3 REPLY (host→target), rvalid=0 (unidirectional)。"""
    assert len(rval) == 32
    out = bytearray([AUTH_TYPE_DHCHAP, MSG_REPLY, 0, 0])
    out.extend(tid.to_bytes(2, "little"))
    out.append(32)  # hl
    out.append(0)   # rsvd2
    out.append(0)   # cvalid = 0
    out.append(0)   # rsvd3
    out.extend((0).to_bytes(2, "little"))  # dhvlen
    out.extend((1).to_bytes(4, "little"))  # seqnum
    out.extend(rval)
    return bytes(out)


def parse_challenge(wire: bytes) -> tuple[int, bytes]:
    """spec § 8.13.5.2 CHALLENGE (target→host), 返 (tid, 32B challenge)。"""
    if len(wire) < 16 + 32:
        fail(f"CHALLENGE too short: {len(wire)}")
    if wire[0] != AUTH_TYPE_DHCHAP or wire[1] != MSG_CHALLENGE:
        fail(f"CHALLENGE header bad: {wire[:2].hex()}")
    tid = int.from_bytes(wire[4:6], "little")
    if wire[8] != HASH_SHA256:
        fail(f"CHALLENGE hashid={wire[8]} 不是 SHA-256")
    if wire[9] != DHGROUP_NULL:
        fail(f"CHALLENGE dhgid={wire[9]} 不是 NULL")
    if wire[6] != 32:  # hl
        fail(f"CHALLENGE hl={wire[6]} 不是 32")
    return tid, wire[16:48]


def parse_success1(wire: bytes) -> int:
    if len(wire) < 16:
        fail(f"SUCCESS1 too short: {len(wire)}")
    if wire[1] != MSG_SUCCESS1:
        fail(f"SUCCESS1 msg_id={wire[1]:#x}")
    return int.from_bytes(wire[4:6], "little")


def parse_failure1(wire: bytes) -> tuple[int, int, int]:
    """返 (tid, rescode, rescode_exp)。"""
    if len(wire) < 16 or wire[1] != MSG_FAILURE1:
        fail(f"FAILURE1 不对: msg={wire[1] if len(wire) > 1 else '?':#x}")
    tid = int.from_bytes(wire[4:6], "little")
    return tid, wire[6], wire[7]


# ============== test scenarios ==============

def icreq_and_connect(s: socket.socket) -> None:
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    if pt != PDU_ICRESP:
        fail("ICResp 没收到")
    sqe = build_fabric_sqe(0x01, FCTYPE_CONNECT, qid=0, sqsize=31, kato=10000)
    write_pdu(s, PDU_CMD, sqe, data=build_connect_data(), pdo=72)
    pt, psh, _ = read_pdu(s)
    if cqe_sc(psh) != 0:
        fail(f"Connect SC={cqe_sc(psh):#x}")


def auth_send(s: socket.socket, cid: int, payload: bytes) -> tuple[bytes, int]:
    """发 AUTH_SEND，返 (capsule_psh, sc)。"""
    sqe = build_fabric_sqe(cid, FCTYPE_AUTH_SEND)
    write_pdu(s, PDU_CMD, sqe, data=payload, pdo=72 if payload else 0)
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP:
        # 可能先来 wire failure C2HData，再 RSP
        if pt == PDU_C2H_DATA:
            pt2, psh2, _ = read_pdu(s)
            if pt2 != PDU_RSP:
                fail(f"AUTH_SEND: 第二个 PDU type={pt2:#x}")
            return psh2, cqe_sc(psh2)
        fail(f"AUTH_SEND: 没收到 RSP, pt={pt:#x}")
    return psh, cqe_sc(psh)


def auth_send_expect_wire_failure(
    s: socket.socket, cid: int, payload: bytes
) -> tuple[int, int, int, int]:
    """发 AUTH_SEND，期望先收 FAILURE1 wire + RSP，返 (tid, rescode, rescode_exp, sc)。"""
    sqe = build_fabric_sqe(cid, FCTYPE_AUTH_SEND)
    write_pdu(s, PDU_CMD, sqe, data=payload, pdo=72 if payload else 0)
    pt, _, fw = read_pdu(s)
    if pt != PDU_C2H_DATA:
        fail(f"AUTH_SEND 期望 wire failure C2HData，得 pt={pt:#x}")
    tid, rc, exp = parse_failure1(fw)
    pt2, psh2, _ = read_pdu(s)
    if pt2 != PDU_RSP:
        fail(f"AUTH_SEND wire-failure 后期望 RSP，得 pt={pt2:#x}")
    return tid, rc, exp, cqe_sc(psh2)


def auth_recv_wire(s: socket.socket, cid: int) -> bytes:
    """发 AUTH_RECV 拉 wire C2HData payload，返 wire bytes。"""
    sqe = build_fabric_sqe(cid, FCTYPE_AUTH_RECV)
    write_pdu(s, PDU_CMD, sqe)
    pt, _, data = read_pdu(s)
    if pt != PDU_C2H_DATA:
        fail(f"AUTH_RECV 期望 C2HData, pt={pt:#x}")
    pt2, psh, _ = read_pdu(s)
    if pt2 != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"AUTH_RECV 后 RSP 不对")
    return data


def scenario_happy_path() -> None:
    print("\n[1] spec 4-msg 完整握手 → Identify 放行")
    s = socket.create_connection((HOST, PORT), timeout=5)
    icreq_and_connect(s)

    # NEGOTIATE
    tid = 0xABCD
    neg = build_negotiate(tid, [HASH_SHA256], [DHGROUP_NULL])
    _, sc = auth_send(s, 0x02, neg)
    if sc != 0:
        fail(f"NEGOTIATE SC={sc:#x}")
    ok("NEGOTIATE accepted")

    # AUTH_RECV → CHALLENGE wire
    fw = auth_recv_wire(s, 0x03)
    challenge_tid, challenge = parse_challenge(fw)
    if challenge_tid != tid:
        fail(f"CHALLENGE tid={challenge_tid:#x} != NEGOTIATE tid")
    ok(f"CHALLENGE: tid={challenge_tid:#x} challenge=32B")

    # REPLY with HMAC
    response = compute_response(SECRET_HEX, challenge, HOSTNQN, SUBNQN)
    reply = build_reply(tid, response)
    _, sc = auth_send(s, 0x04, reply)
    if sc != 0:
        fail(f"REPLY SC={sc:#x}")
    ok("REPLY HMAC verified")

    # AUTH_RECV → SUCCESS1
    fw = auth_recv_wire(s, 0x05)
    s1_tid = parse_success1(fw)
    if s1_tid != tid:
        fail(f"SUCCESS1 tid mismatch {s1_tid:#x}")
    if len(fw) != 16:
        fail(f"SUCCESS1 wire len={len(fw)} (期望 16)")
    ok("SUCCESS1 (unidirectional, 16B)")

    # Identify Controller 放行
    sqe = bytearray(64)
    sqe[0] = 0x06
    sqe[2:4] = (0x100).to_bytes(2, "little")
    sqe[40:44] = (0x01).to_bytes(4, "little")
    write_pdu(s, PDU_CMD, bytes(sqe))
    pt1, _, data = read_pdu(s)
    pt2, psh, _ = read_pdu(s)
    if pt1 != PDU_C2H_DATA or pt2 != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Identify after spec-4 CHAP: pt1={pt1:#x} pt2={pt2:#x}")
    if len(data) != 4096:
        fail(f"Identify data len={len(data)} (期望 4096)")
    ok("Identify Controller 4096 B (CHAP gate 通过)")

    s.close()


def scenario_negotiate_no_sha256() -> None:
    print("\n[2] NEGOTIATE 无 SHA-256 → FAILURE1 wire + SC=0x83")
    s = socket.create_connection((HOST, PORT), timeout=5)
    icreq_and_connect(s)
    tid = 0xBEEF
    neg = build_negotiate(tid, [HASH_SHA384], [DHGROUP_NULL])
    _, rc, exp, sc = auth_send_expect_wire_failure(s, 0x02, neg)
    if sc != 0x83:
        fail(f"NEGOTIATE no-SHA256 SC={sc:#x} 期望 0x83")
    if exp != FAIL_EXP_HASH_UNUSABLE:
        fail(f"rescode_exp={exp:#x} 期望 HASH_UNUSABLE (0x04)")
    ok(f"FAILURE1 rescode={rc:#x} exp=HASH_UNUSABLE (0x04) + RSP SC=0x83")
    s.close()


def scenario_reply_wrong_secret() -> None:
    print("\n[3] REPLY 用假 secret → FAILURE1 rescode_exp=FAILED + SC=0x83")
    s = socket.create_connection((HOST, PORT), timeout=5)
    icreq_and_connect(s)
    tid = 0xCAFE
    neg = build_negotiate(tid, [HASH_SHA256], [DHGROUP_NULL])
    _, sc = auth_send(s, 0x02, neg)
    if sc != 0:
        fail("NEGOTIATE 失败")
    fw = auth_recv_wire(s, 0x03)
    _, challenge = parse_challenge(fw)
    # 用假 secret 算 response
    bad = compute_response("ff" * 32, challenge, HOSTNQN, SUBNQN)
    reply = build_reply(tid, bad)
    _, _, exp, sc = auth_send_expect_wire_failure(s, 0x04, reply)
    if sc != 0x83:
        fail(f"假 secret SC={sc:#x}")
    if exp != FAIL_EXP_FAILED:
        fail(f"rescode_exp={exp:#x} 期望 FAILED (0x01)")
    ok(f"FAILURE1 exp=FAILED + RSP SC=0x83")
    s.close()


def scenario_multi_descriptor() -> None:
    print("\n[4] NEGOTIATE napd=2 (DH-2048 first, NULL second) → 接受第二个 (V-dhchap-4d)")
    s = socket.create_connection((HOST, PORT), timeout=5)
    icreq_and_connect(s)
    tid = 0xDEAD
    # 第一个 descriptor: SHA-384 + DH-2048 (我们不支持)
    # 第二个 descriptor: SHA-256 + DH-NULL (匹配)
    neg = build_negotiate_multi(tid, [
        (AUTH_DHCHAP, [HASH_SHA384], [0x01]),
        (AUTH_DHCHAP, [HASH_SHA256], [DHGROUP_NULL]),
    ])
    _, sc = auth_send(s, 0x02, neg)
    if sc != 0:
        fail(f"multi-descriptor NEGOTIATE SC={sc:#x}")
    ok("multi-descriptor NEGOTIATE accepted (第二个 descriptor 命中)")

    # 完成剩余握手验证
    fw = auth_recv_wire(s, 0x03)
    _, challenge = parse_challenge(fw)
    response = compute_response(SECRET_HEX, challenge, HOSTNQN, SUBNQN)
    reply = build_reply(tid, response)
    _, sc = auth_send(s, 0x04, reply)
    if sc != 0:
        fail(f"REPLY 在 multi-descriptor 后 SC={sc:#x}")
    ok("REPLY 通过 (multi-descriptor 全流程 e2e)")

    s.close()


def main() -> None:
    print("=== V-interop-8 DH-HMAC-CHAP spec § 8.13.5 4-message wire e2e ===")
    print(f"target {HOST}:{PORT}, host={HOSTNQN}, secret prefix {SECRET_HEX[:8]}...")
    try:
        scenario_happy_path()
        scenario_negotiate_no_sha256()
        scenario_reply_wrong_secret()
        scenario_multi_descriptor()
    except (ConnectionRefusedError, OSError) as e:
        fail(
            f"无法连 {HOST}:{PORT}: {e}\n"
            "先启 target:\n"
            f"  cargo run --bin nvme_of_tcp_target -- \\\n"
            f"    --listen-tcp {HOST}:{PORT} --backing /tmp/dhchap4.img \\\n"
            f"    --host-secret {HOSTNQN}={SECRET_HEX}"
        )
    print("\n✅ V-interop-8 全 4 scenarios 通过 (spec 4-msg wire + multi-descriptor)")


if __name__ == "__main__":
    main()
