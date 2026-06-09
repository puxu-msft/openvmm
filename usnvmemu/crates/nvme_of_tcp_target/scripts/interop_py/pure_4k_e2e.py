#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""2026-06-09 纯-4K over fabric e2e — Format NS→4K 后真 wire 跑 4K IO (无 sudo)。

独立 Python client 验证 NVMe-oF TCP target 的纯-4K 扇区感知（commit 73429e28）：
  1. admin conn: Format NVM lbafl=2 → NS1 切纯-4K (lbads=12)。需 target
     `--allow-format`（否则 Format 被 block 返 SC=0x01）。
  2. IO conn（独立 TCP，共享同一 controller → 多 conn 场景）: 按 **4096B/sector**
     跑 IO：
       - nlb=1 (4096B) 写+读回 byte-equal（单 PRP）；
       - nlb=2 (8192B) 写+读回 byte-equal（dual PRP，prp2 sentinel 阈值在 4K 下
         nlb≥2 即触发）；
       - nlb=3 (12288B) → SC=0x18（4K 单次 dispatch 上限=2；旧 512B 硬编码 session
         会误收 nlb=3 → controller PRP-list path → 撕裂）。

跑法（target 必须带 --allow-format）：
    # 另一终端：
    #   cargo run --bin nvme_of_tcp_target -- --listen 127.0.0.1:4420 \
    #       --backing-file /tmp/ns_4k.img --allow-format
    uv run python pure_4k_e2e.py
"""
import socket
import struct
import sys
import os

HOST = "127.0.0.1"
PORT = int(os.environ.get("NVME_PORT", "4420"))
HOSTNQN = "nqn.2014-08.org.nvmexpress:uuid:pure4k-host"
SUBNQN = "nqn.2014-08.org.nvmexpress:teaching:disk"

NVME_OPC_FABRIC = 0x7F
FCTYPE_PROPERTY_SET = 0x00
FCTYPE_CONNECT = 0x01
ADMIN_OPC_FORMAT = 0x80

PDU_ICREQ = 0x00
PDU_ICRESP = 0x01
PDU_CMD = 0x04
PDU_RSP = 0x05
PDU_H2C_DATA = 0x06
PDU_C2H_DATA = 0x07
PDU_R2T = 0x09

PROP_CC = 0x14
SECTOR_4K = 4096


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


def read_pdu(s: socket.socket):
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


def write_pdu(s: socket.socket, pdu_type: int, psh: bytes, data: bytes = b"", pdo: int = 0) -> None:
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


def build_connect_sqe(cid: int, qid: int, sqsize: int, kato: int) -> bytes:
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4] = FCTYPE_CONNECT
    sqe[40:64] = struct.pack("<HHHBBI12s", 0, qid, sqsize, 0, 0, kato, b"\x00" * 12)
    return bytes(sqe)


def build_connect_data() -> bytes:
    buf = bytearray(1024)
    buf[16:18] = b"\xff\xff"
    sn = SUBNQN.encode("ascii")
    hn = HOSTNQN.encode("ascii")
    buf[256 : 256 + len(sn)] = sn
    buf[512 : 512 + len(hn)] = hn
    return bytes(buf)


def build_cc_en() -> bytes:
    sqe = bytearray(64)
    sqe[0] = NVME_OPC_FABRIC
    sqe[2:4] = (0x0002).to_bytes(2, "little")
    sqe[4] = FCTYPE_PROPERTY_SET
    sqe[40:64] = struct.pack("<B3sIQ8s", 0, b"\x00\x00\x00", PROP_CC, 0x46_0001, b"\x00" * 8)
    return bytes(sqe)


def build_format_sqe(cid: int, nsid: int, lbafl: int) -> bytes:
    """Format NVM (admin opc 0x80)。CDW10 bits 3:0 = LBAF lower (lbafl)；
    SES/PI/MSET 全 0（无 secure erase / 无 PI / 不切 meta）。lbafl=2 = 纯 4K。"""
    sqe = bytearray(64)
    sqe[0] = ADMIN_OPC_FORMAT
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = nsid.to_bytes(4, "little")
    sqe[40:44] = lbafl.to_bytes(4, "little")  # cdw10
    return bytes(sqe)


def build_io_rw(cid: int, opc: int, slba: int, nlb_zero_based: int) -> bytes:
    """opc=0x01 Write, 0x02 Read."""
    sqe = bytearray(64)
    sqe[0] = opc
    sqe[2:4] = cid.to_bytes(2, "little")
    sqe[4:8] = (1).to_bytes(4, "little")
    sqe[40:48] = slba.to_bytes(8, "little")
    sqe[48:52] = nlb_zero_based.to_bytes(4, "little")
    return bytes(sqe)


def cqe_sc(psh: bytes) -> int:
    status = int.from_bytes(psh[14:16], "little")
    return (status >> 1) & 0xFF


def parse_r2t_psh(psh: bytes):
    cccid = int.from_bytes(psh[0:2], "little")
    ttag = int.from_bytes(psh[2:4], "little")
    r2t_offset = int.from_bytes(psh[4:8], "little")
    r2t_length = int.from_bytes(psh[8:12], "little")
    return cccid, ttag, r2t_offset, r2t_length


def build_h2c_data_psh(cccid: int, ttag: int, datao: int, datal: int) -> bytes:
    psh = struct.pack("<HHII4s", cccid, ttag, datao, datal, b"\x00" * 4)
    assert len(psh) == 16
    return psh


def write_pdu_h2c(s: socket.socket, psh: bytes, data: bytes, pdo: int, flags: int) -> None:
    hlen = 8 + len(psh)
    pad = pdo - hlen
    plen = pdo + len(data)
    hdr = struct.pack("<BBBBI", PDU_H2C_DATA, flags, hlen, pdo, plen)
    s.sendall(hdr + psh + (b"\x00" * pad) + data)


def setup_admin(s: socket.socket) -> None:
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


def setup_io(host: str, port: int) -> socket.socket:
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


def io_write_4k(s: socket.socket, cid: int, slba: int, payload: bytes) -> None:
    """按 4096B/sector 写 payload（必须 4096 倍数，≤ 8 KiB = 2 LBA 单次上限）。"""
    assert len(payload) % SECTOR_4K == 0, "payload 必须是 4096 的倍数"
    nlb = (len(payload) // SECTOR_4K) - 1  # zero-based
    write_pdu(s, PDU_CMD, build_io_rw(cid, 0x01, slba, nlb))
    total_sent = 0
    while total_sent < len(payload):
        pt, psh, _ = read_pdu(s)
        if pt != PDU_R2T:
            fail(f"expected R2T (sent={total_sent}/{len(payload)}), got pt={pt:#x}")
        cccid, ttag, r2to, r2tl = parse_r2t_psh(psh)
        if cccid != cid:
            fail(f"R2T cccid={cccid:#x} != cmd cid={cid:#x}")
        chunk = payload[r2to : r2to + r2tl]
        if len(chunk) != r2tl:
            fail(f"R2T r2to={r2to} r2tl={r2tl} 超出 payload {len(payload)}")
        write_pdu_h2c(s, build_h2c_data_psh(cid, ttag, r2to, r2tl), chunk, pdo=24, flags=0x04)
        total_sent += r2tl
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP or cqe_sc(psh) != 0:
        fail(f"Write RSP: pt={pt:#x} sc={cqe_sc(psh):#x}")
    ok(f"  Write cid={cid:#x} slba={slba} len={len(payload)} ({len(payload)//SECTOR_4K} LBA @4K)")


def io_read_4k(s: socket.socket, cid: int, slba: int, byte_len: int) -> bytes:
    """按 4096B/sector 读 byte_len。收集所有 C2HData（dual PRP 可能多段）拼接。"""
    assert byte_len % SECTOR_4K == 0
    nlb = (byte_len // SECTOR_4K) - 1
    write_pdu(s, PDU_CMD, build_io_rw(cid, 0x02, slba, nlb))
    got = b""
    while True:
        pt, psh, data = read_pdu(s)
        if pt == PDU_C2H_DATA:
            got += data
        elif pt == PDU_RSP:
            sc = cqe_sc(psh)
            if sc != 0:
                fail(f"Read RSP SC={sc:#x} (got {len(got)}/{byte_len})")
            break
        else:
            fail(f"Read 期望 C2HData/RSP，got pt={pt:#x}")
    if len(got) != byte_len:
        fail(f"Read 总长 {len(got)} != 期望 {byte_len}（chunked 多/少发字节静默错位）")
    ok(f"  Read  cid={cid:#x} slba={slba} len={byte_len} ({byte_len//SECTOR_4K} LBA @4K)")
    return got


def io_read_expect_reject(s: socket.socket, cid: int, slba: int, nlb_zero_based: int, want_sc: int) -> None:
    """发一条 IO Read 期望被 reject（无 C2HData，直接 RSP want_sc）。"""
    write_pdu(s, PDU_CMD, build_io_rw(cid, 0x02, slba, nlb_zero_based))
    pt, psh, _ = read_pdu(s)
    if pt != PDU_RSP:
        fail(f"期望 reject RSP（nlb={nlb_zero_based + 1}），却收 pt={pt:#x}（target 误受理 → 撕裂风险）")
    sc = cqe_sc(psh)
    if sc != want_sc:
        fail(f"期望 SC={want_sc:#x}，实收 SC={sc:#x}")
    ok(f"  Read  cid={cid:#x} nlb={nlb_zero_based + 1} @4K 正确 reject SC={sc:#x}")


def lba_tagged_pattern(n_lba: int) -> bytes:
    """每 LBA 高 nibble = (LBA 序号+1) 当 marker，低 nibble 随 byte 变。让**整-LBA
    偏移可见**：`(i+const)&0xFF` period-256 + 4096%256==0，整-LBA shift 会静默
    byte-equal；per-LBA marker 让 shift 改变 marker → 被 catch。"""
    out = bytearray()
    for lba in range(n_lba):
        marker = ((lba + 1) << 4) & 0xF0
        out += bytes((marker | (i & 0x0F)) for i in range(SECTOR_4K))
    return bytes(out)


def main() -> None:
    print("=== 纯-4K over fabric e2e (Format→4K + 扇区感知 IO) ===")
    admin = socket.create_connection((HOST, PORT), timeout=10)
    setup_admin(admin)

    # [1] admin conn: Format NS1 → lbaf=2 (纯 4K)。需 target --allow-format。
    print("\n[1] Format NVM NS1 lbaf=2 (纯 4K)")
    write_pdu(admin, PDU_CMD, build_format_sqe(0x300, nsid=1, lbafl=2))
    pt, psh, _ = read_pdu(admin)
    sc = cqe_sc(psh)
    if sc == 0x01:
        fail("Format 返 SC=0x01 INVALID_OPCODE — target 是否带 --allow-format 启动？")
    if sc != 0:
        fail(f"Format NVM SC={sc:#x}")
    ok("NS1 已 Format 到 lbaf=2 (纯 4K, lbads=12)")

    # [2] IO conn（独立 TCP，共享 controller → 多 conn 场景）跑 4K IO
    io = setup_io(HOST, PORT)

    # 单 PRP：1 LBA = 4096B 写+读回
    pat1 = bytes(((b + 0x55) & 0xFF) for b in range(SECTOR_4K))
    print("\n[2] 单 PRP：Write 1 LBA (4096B) @ SLBA=10 → 读回 byte-equal")
    io_write_4k(io, cid=0x400, slba=10, payload=pat1)
    got1 = io_read_4k(io, cid=0x401, slba=10, byte_len=SECTOR_4K)
    if got1 != pat1:
        fail("单 PRP 4K byte-mismatch")
    ok("[3] 单 PRP 4K Write/Read byte-equal")

    # dual PRP：2 LBA = 8192B（4K 下 nlb≥2 即触发 prp2 sentinel）
    pat2 = bytes(((b + 0xA3) & 0xFF) for b in range(2 * SECTOR_4K))
    print("\n[4] dual PRP：Write 2 LBA (8192B) @ SLBA=20 → 读回 byte-equal")
    io_write_4k(io, cid=0x500, slba=20, payload=pat2)
    got2 = io_read_4k(io, cid=0x501, slba=20, byte_len=2 * SECTOR_4K)
    if got2 != pat2:
        fail("dual PRP 4K byte-mismatch")
    ok("[5] dual PRP 4K Write/Read byte-equal")

    # 扇区感知 chunking + 独立 oracle：nlb=3 (12288B = 3 页) > 单次 dual-PRP
    # 上限(2)。生产 async 路径**透明拆 chunk**（2+1 LBA，各 ≤ 2 页）。若 session
    # 仍按 512B 硬编码：dual_prp_max_lbas(9)=16 → nlb=3 不拆 → 单 dispatch prp2=0
    # 但 controller bytes=12288>8192 → PRP-list path → 撕裂/corruption。
    # **注**：chunked write+chunked read 同路径，symmetric 偏移 bug 会 round-trip
    # 静默通过；故 ① 用 per-LBA marker pattern（整-LBA shift 可见）+ ② 用**独立**
    # 单-LBA 读（非 chunked，直接 slba*sector 偏移）交叉验 chunked write 落对绝对 LBA。
    pat3 = lba_tagged_pattern(3)
    print("\n[6] 扇区感知 chunking：chunked Write 3 LBA (12288B, 拆 2+1) @SLBA=30 → chunked 读 byte-equal")
    io_write_4k(io, cid=0x600, slba=30, payload=pat3)
    got3 = io_read_4k(io, cid=0x601, slba=30, byte_len=3 * SECTOR_4K)
    if got3 != pat3:
        fail("3 LBA 4K (chunked) byte-mismatch — chunk 偏移/扇区错")
    ok("[7] chunked Write/Read byte-equal (per-LBA marker pattern)")

    print("[8] 独立 oracle：单-LBA (nlb=1, 非 chunked) 逐读 SLBA 30/31/32 验落对绝对 LBA")
    for k in range(3):
        lba_got = io_read_4k(io, cid=0x610 + k, slba=30 + k, byte_len=SECTOR_4K)
        want = pat3[k * SECTOR_4K : (k + 1) * SECTOR_4K]
        if lba_got != want:
            fail(f"LBA {30 + k} 单读 != chunked write 第 {k} LBA — chunk 偏移错位")
        if (lba_got[0] >> 4) != k + 1:
            fail(f"LBA {30 + k} marker={lba_got[0] >> 4} != {k + 1}（整-LBA 偏移未被 chunked 写对）")
    ok("[9] 独立单-LBA oracle 确认 chunked write 落对绝对 LBA 30/31/32")

    # 扇区感知 MDTS cap：4K 下 host_io_max_lbas = 128 KiB / 4096 = 32 LBA。
    # nlb=33 > 32 → reject SC=0x18。旧 512B 硬编码 cap=256 会误受理 nlb=33 →
    # 拆 16-LBA chunk（64 KiB=16 页）撞穿 controller dual-PRP → 撕裂。
    print("\n[10] 扇区感知 MDTS cap：Read nlb=33 @4K (>32) 必 reject SC=0x18")
    io_read_expect_reject(io, cid=0x700, slba=50, nlb_zero_based=32, want_sc=0x18)

    admin.close()
    io.close()
    print("\n✅ 纯-4K over fabric e2e 通过：Format→4K + 单/dual PRP + chunking byte-equal + MDTS cap=32 reject")


if __name__ == "__main__":
    main()
