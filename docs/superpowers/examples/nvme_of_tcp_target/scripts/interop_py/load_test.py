#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""V-followup-interop-7 load test — pure-Python 多线程 IO Read 压测。

无 sudo / nvme-cli / fio。多 TCP 连接并发跑 NVMe-oF IO Read 单 PRP1 4 KiB
循环 N 秒，测吞吐 + 稳定性 (无 CQE 丢失 / 无超时 / 无 TLS handshake 故障)。

跑法:
    uv run python load_test.py [--threads 4] [--seconds 5] [--tls]
"""
import argparse
import socket
import ssl
import struct
import sys
import threading
import time

HOST = "127.0.0.1"
PLAIN_PORT = 4420
TLS_PORT = 8009
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:load-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"

NVME_OPC_FABRIC = 0x7F
FCTYPE_PROPERTY_SET = 0x00
FCTYPE_CONNECT = 0x01

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_C2H_DATA = 0x07
PDU_R2T = 0x09

PROP_CC = 0x14


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
    cf = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    sqe[40:64] = cf
    return bytes(sqe)


def build_connect_data():
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    sn = SUBNQN.encode("ascii")
    hn = HOSTNQN.encode("ascii")
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def build_property_set_cc_en():
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = (0x0002).to_bytes(2, "little")
    sqe[4] = FCTYPE_PROPERTY_SET
    pf = struct.pack("<B3sIQ8s", 0, b"\x00\x00\x00", PROP_CC, 0x46_0001, b"\x00" * 8)
    sqe[40:64] = pf
    return bytes(sqe)


def build_io_read(cid, slba, nlb_zero_based):
    sqe = bytearray(64)
    sqe[0] = 0x02
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")  # nsid=1
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb_zero_based.to_bytes(4, "little")
    return bytes(sqe)


def cqe_sc(psh):
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def make_socket(use_tls: bool) -> socket.socket:
    port = TLS_PORT if use_tls else PLAIN_PORT
    raw = socket.create_connection((HOST, port), timeout=5)
    if use_tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        return ctx.wrap_socket(raw, server_hostname=HOST)
    return raw


def setup_admin(use_tls: bool):
    s = make_socket(use_tls)
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    assert pt == PDU_ICRESP
    sqe = build_connect_sqe(0x0001, 0, 31, 10000)
    write_pdu(s, PDU_CMD, sqe, data=build_connect_data(), pdo=72)
    pt, psh, _ = read_pdu(s)
    assert cqe_sc(psh) == 0, f"admin Connect SC={cqe_sc(psh):#x}"
    sqe = build_property_set_cc_en()
    write_pdu(s, PDU_CMD, sqe)
    pt, psh, _ = read_pdu(s)
    assert cqe_sc(psh) == 0
    return s


def setup_io_queue(use_tls: bool, qid: int):
    s = make_socket(use_tls)
    write_pdu(s, PDU_ICREQ, build_icreq())
    pt, _, _ = read_pdu(s)
    assert pt == PDU_ICRESP
    sqe = build_connect_sqe(cid=0x100 + qid, qid=qid, sqsize=31, kato=0)
    write_pdu(s, PDU_CMD, sqe, data=build_connect_data(), pdo=72)
    pt, psh, _ = read_pdu(s)
    assert cqe_sc(psh) == 0, f"IO Connect qid={qid} SC={cqe_sc(psh):#x}"
    return s


def io_worker(s, qid: int, end_time: float, stats: dict, lock: threading.Lock):
    local = {"reads": 0, "bytes": 0, "errs": 0}
    cid = 0x1000 + qid * 0x100
    slba = 0
    while time.monotonic() < end_time:
        cid = (cid + 1) & 0xFFFF
        if cid == 0:
            cid = 1
        sqe = build_io_read(cid, slba, 7)  # 8 LBA = 4 KiB
        write_pdu(s, PDU_CMD, sqe)
        # 收 C2HData (4 KiB) + RSP
        try:
            pt1, _, d1 = read_pdu(s)
            pt2, psh2, _ = read_pdu(s)
            if pt1 == PDU_C2H_DATA and pt2 == PDU_RSP and cqe_sc(psh2) == 0:
                local["reads"] += 1
                local["bytes"] += len(d1)
            else:
                local["errs"] += 1
        except Exception:
            local["errs"] += 1
            break
        slba = (slba + 8) % 131072  # backing 64 MiB / 512 = 131072 LBA
    with lock:
        for k, v in local.items():
            stats[k] = stats.get(k, 0) + v


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--threads", type=int, default=4)
    p.add_argument("--seconds", type=int, default=5)
    p.add_argument("--tls", action="store_true")
    args = p.parse_args()

    print(f"=== V-interop-7 load test: threads={args.threads} sec={args.seconds} tls={args.tls} ===")

    admin = setup_admin(args.tls)

    # 起 N 个 IO 队列 (独立 TCP, 4 worker thread)
    io_socks = []
    for qid in range(1, args.threads + 1):
        s = setup_io_queue(args.tls, qid)
        io_socks.append(s)
        print(f"  IO queue {qid} connected")

    stats = {"reads": 0, "bytes": 0, "errs": 0}
    lock = threading.Lock()
    end = time.monotonic() + args.seconds
    threads = []
    for i, s in enumerate(io_socks):
        t = threading.Thread(target=io_worker, args=(s, i + 1, end, stats, lock))
        t.start()
        threads.append(t)

    for t in threads:
        t.join()

    elapsed = args.seconds
    reads = stats["reads"]
    mib = stats["bytes"] / (1024 * 1024)
    iops = reads / elapsed
    mbps = mib / elapsed
    print(f"\n--- Results ({elapsed}s) ---")
    print(f"reads  = {reads}")
    print(f"errors = {stats['errs']}")
    print(f"IOPS   = {iops:.0f}")
    print(f"MiB/s  = {mbps:.1f}")
    if stats["errs"] > 0:
        print(f"FAIL: {stats['errs']} errors", file=sys.stderr)
        sys.exit(1)
    print("✅ all reads succeeded, no errors")


if __name__ == "__main__":
    main()
