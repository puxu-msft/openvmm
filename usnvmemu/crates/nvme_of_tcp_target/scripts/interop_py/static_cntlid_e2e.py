#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""static controller model e2e — 驱动**真 bin** 跨进程验 Connect CNTLID 校验 +
SCT + IPO + discovery static-model 广告（无 sudo，raw socket）。

覆盖 in-process probe / lib test 之外的真实面：
  - 真 `nvme_of_tcp_target` bin 进程（非 lib），真 TCP，独立 Python 解析器（独立 oracle）。
  - bin CLI `--discovery-static-cntlid` flag wiring 端到端。

跑法:
    target/release/nvme_of_tcp_target 已 build 后：
    uv run python static_cntlid_e2e.py            # 或 python3
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

BIN = os.environ.get(
    "USNVMEMU_BIN",
    os.path.join(
        os.path.dirname(__file__), "..", "..", "target", "release", "nvme_of_tcp_target"
    ),
)
HOST = "127.0.0.1"
IO_PORT = 14420
DISC_PORT = 14421
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:static-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"
DISCOVERY_NQN = "nqn.2014-08.org.nvmexpress.discovery"

NVME_OPC_FABRIC = 0x7F
FCTYPE_CONNECT = 0x01
PDU_ICREQ, PDU_ICRESP, PDU_CMD, PDU_RSP, PDU_C2H_DATA = 0x00, 0x01, 0x04, 0x05, 0x07

fails = []


def check(cond, msg):
    if cond:
        print(f"  ✅ {msg}")
    else:
        print(f"  ❌ FAIL: {msg}")
        fails.append(msg)


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise RuntimeError(f"EOF after {len(buf)}/{n}")
        buf += chunk
    return buf


def read_pdu(s):
    pdu_type, _f, hlen, pdo, plen = struct.unpack("<BBBBI", recv_exact(s, 8))
    psh = recv_exact(s, hlen - 8) if hlen > 8 else b""
    consumed = hlen
    if pdo > consumed:
        recv_exact(s, pdo - consumed)
        consumed = pdo
    data = recv_exact(s, plen - consumed) if plen > consumed else b""
    return pdu_type, psh, data


def write_pdu(s, pdu_type, psh, data=b"", pdo=0):
    hlen = 8 + len(psh)
    if pdo == 0:
        plen, body = hlen + len(data), psh + data
    else:
        plen, body = pdo + len(data), psh + (b"\x00" * (pdo - hlen)) + data
    s.sendall(struct.pack("<BBBBI", pdu_type, 0, hlen, pdo, plen) + body)


def build_icreq():
    return struct.pack("<HBBI112s", 0, 0, 0, 0, b"\x00" * 112)


def build_connect_sqe(cid, qid=0, kato=10000):
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    sqe[40:64] = struct.pack("<HHHBBI12s", 0, qid, 31, 0, 0, kato, b"\x00" * 12)
    return bytes(sqe)


def build_connect_data(cntlid, subnqn=SUBNQN):
    buf = bytearray(1024)
    buf[16:18] = cntlid.to_bytes(2, "little")  # ← 本测试的核心变量
    sn, hn = subnqn.encode(), HOSTNQN.encode()
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def cqe_sc(psh):
    return (int.from_bytes(psh[14:16], "little") >> 1) & 0xFF


def cqe_sct(psh):
    return (int.from_bytes(psh[14:16], "little") >> 9) & 0x07


def cqe_result_dw0(psh):
    return int.from_bytes(psh[0:4], "little")


def _listen_port(args):
    return int(args[args.index("--listen") + 1].split(":")[1])


def spawn_bin(args):
    p = subprocess.Popen(
        [BIN, *args],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={**os.environ, "RUST_LOG": "warn"},
    )
    # 等监听就绪
    for _ in range(50):
        try:
            socket.create_connection((HOST, _listen_port(args)), timeout=0.3).close()
            return p
        except OSError:
            time.sleep(0.1)
    p.kill()
    p.wait()
    raise RuntimeError(f"bin 未在超时内监听: {args}")


def connect_with_cntlid(port, cntlid, subnqn=SUBNQN):
    s = socket.create_connection((HOST, port), timeout=10)
    try:
        write_pdu(s, PDU_ICREQ, build_icreq())
        assert read_pdu(s)[0] == PDU_ICRESP
        write_pdu(s, PDU_CMD, build_connect_sqe(0x42), data=build_connect_data(cntlid, subnqn), pdo=72)
        _, psh, _ = read_pdu(s)
    finally:
        s.close()
    return cqe_sc(psh), cqe_sct(psh), cqe_result_dw0(psh)


def get_discovery_log_entry_cntlid(port):
    s = socket.create_connection((HOST, port), timeout=10)
    try:
        write_pdu(s, PDU_ICREQ, build_icreq())
        assert read_pdu(s)[0] == PDU_ICRESP
        write_pdu(s, PDU_CMD, build_connect_sqe(0x1), data=build_connect_data(0xFFFF, DISCOVERY_NQN), pdo=72)
        sc = cqe_sc(read_pdu(s)[1])
        if sc != 0:
            raise RuntimeError(f"discovery Connect SC={sc:#x}")
        # Get Log Page LID=0x70, 2048B = header(1024) + 1 entry(1024)
        bytes_req = 2048
        numd = bytes_req // 4 - 1
        cdw10 = 0x70 | ((numd & 0xFFFF) << 16)
        sqe = bytearray(64)
        sqe[0] = 0x02  # GET_LOG_PAGE
        sqe[2:4] = (0x70).to_bytes(2, "little")
        sqe[40:44] = cdw10.to_bytes(4, "little")
        sqe[44:48] = ((numd >> 16) & 0xFFFF).to_bytes(4, "little")
        write_pdu(s, PDU_CMD, bytes(sqe))
        log = b""
        while len(log) < bytes_req:
            pt, psh, data = read_pdu(s)
            if pt == PDU_C2H_DATA:
                log += data
            elif pt == PDU_RSP:
                break
    finally:
        s.close()
    # entry[0] @ 1024；CNTLID @ entry+6 = 1030（u16 LE）。
    return int.from_bytes(log[1030:1032], "little")


def main():
    if not os.path.exists(BIN):
        print(f"FAIL: bin 不存在 {BIN}（先 cargo build --release --bin nvme_of_tcp_target）")
        sys.exit(1)
    print("=== static controller model e2e (真 bin 跨进程) ===")
    backing = tempfile.NamedTemporaryFile(suffix=".img", delete=False)
    backing.truncate(16 * 1024 * 1024)
    backing.close()
    try:
        # ── Part 1: Connect CNTLID 矩阵（真 IO bin）─────────────────────
        print(f"\n[Part 1] Connect CNTLID 矩阵 — 真 bin :{IO_PORT}")
        io_bin = spawn_bin(["--listen", f"{HOST}:{IO_PORT}", "--backing-file", backing.name])
        try:
            sc, _, dw0 = connect_with_cntlid(IO_PORT, 0xFFFF)
            check(sc == 0 and dw0 & 0xFFFF == 1, f"dynamic(0xFFFF) → SC=0 cntlid=1 (got sc={sc:#x} dw0={dw0:#x})")

            sc, sct, dw0 = connect_with_cntlid(IO_PORT, 5)
            check(sc == 0x82, f"static mismatch(5) → SC=0x82 (got {sc:#x})")
            check(sct == 0x01, f"  └ SCT=0x01 Command Specific (got {sct:#x}) — 非 0x07 Vendor")
            check(dw0 == 0x0001_0004, f"  └ result DW0 = IPO/IATTR 0x0001_0004 (got {dw0:#x})")

            sc, _, dw0 = connect_with_cntlid(IO_PORT, 1)
            check(sc == 0 and dw0 & 0xFFFF == 1, f"static match(1) → SC=0 cntlid=1 (got sc={sc:#x} dw0={dw0:#x})")

            sc, _, dw0 = connect_with_cntlid(IO_PORT, 0xFFFE)
            check(sc == 0 and dw0 & 0xFFFF == 1, f"static-any(0xFFFE) → SC=0 cntlid=1 (got sc={sc:#x} dw0={dw0:#x})")
        finally:
            io_bin.kill()
            io_bin.wait()

        # ── Part 2: discovery static-model 广告（真 bin + flag）──────────
        print(f"\n[Part 2] discovery static-model 广告 — 真 bin --discovery-static-cntlid")

        def disc_args(port):
            return [
                "--listen", f"{HOST}:{port}", "--backing-file", backing.name,
                "--discovery-mode",
                "--discovery-target-nqn", SUBNQN,
                "--discovery-target-addr", f"{HOST}:{IO_PORT}",
            ]

        # static：--discovery-static-cntlid 1（独立端口避免 kill 后端口重用竞态）
        disc_bin = spawn_bin(disc_args(DISC_PORT) + ["--discovery-static-cntlid", "1"])
        try:
            c = get_discovery_log_entry_cntlid(DISC_PORT)
            check(c == 1, f"--discovery-static-cntlid 1 → Discovery Log entry CNTLID=1 (got {c:#x})")
        finally:
            disc_bin.kill()
            disc_bin.wait()
        # dynamic：默认（无 flag），用另一端口
        disc_bin = spawn_bin(disc_args(DISC_PORT + 1))
        try:
            c = get_discovery_log_entry_cntlid(DISC_PORT + 1)
            check(c == 0xFFFF, f"默认（无 flag）→ Discovery Log entry CNTLID=0xFFFF dynamic (got {c:#x})")
        finally:
            disc_bin.kill()
            disc_bin.wait()
    finally:
        if os.path.exists(backing.name):
            os.unlink(backing.name)

    print()
    if fails:
        print(f"❌ {len(fails)} 个断言失败")
        sys.exit(1)
    print("✅ static controller model e2e 全通过（真 bin 跨进程 + 独立 Python oracle）")


if __name__ == "__main__":
    main()
