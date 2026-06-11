# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""POC-1：非-QEMU vfio-user client 的 fd-passing 零拷贝 DMA（非-CVM，本地可跑）。

验 Spec A 承重假设：我们自己写的 client 经 AF_UNIX+SCM_RIGHTS 把 memfd "guest RAM"
交给 firmware server，server mmap 零拷贝双向 DMA，驱动 NVMe Identify 端到端通。

零拷贝自证：本 client **不服务任何 server-initiated DMA_READ/WRITE**。若 server 走
message-mediated，它取 SQE 时会发 DMA_READ 阻塞等 reply → 我们不回 → Identify 永不
完成。故 Identify 成功 ⟺ server 走了 mmap 零拷贝（数据直落进我们的 memfd）。

用法：`python3 poc1_dma_zerocopy.py`（自动 build+spawn）。详见同目录 README.md。
"""
from __future__ import annotations

import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import poclib as P
import vfio_proto as v

TAG = "poc1-zerocopy-dma"


class MsgId:
    """单调 msg_id 生成器。"""

    def __init__(self) -> None:
        self._n = 0

    def __call__(self) -> int:
        self._n += 1
        return self._n


def run(sock: socket.socket, ram, memfd: int) -> None:
    nxt = MsgId()

    v.version_handshake(sock, nxt())
    P.check(TAG, v.get_info(sock, nxt())["num_regions"] == 9, "VERSION + GET_INFO num_regions=9")

    P.dma_map_with_fd(sock, nxt(), 0, P.RAM_SIZE, memfd)
    P.log(TAG, "DMA_MAP with memfd OK (server 应 mmap 零拷贝)")

    P.nvme_enable(sock, nxt)
    P.check(TAG, True, "CSTS.RDY=1 (controller enabled)")

    cid = 0x0042
    ram[P.ASQ_GPA:P.ASQ_GPA + 64] = P.build_identify_sqe(cid, P.PRP1_GPA)
    ram[P.ACQ_GPA:P.ACQ_GPA + 16] = b"\x00" * 16
    ram[P.PRP1_GPA:P.PRP1_GPA + 4096] = b"\x00" * 4096
    v.region_write(sock, nxt(), v.REGION_BAR0, P.SQ0TDBL, struct.pack("<I", 1))
    P.log(TAG, "rang SQ0 doorbell（全程不服务 DMA_READ/WRITE — 零拷贝自证）")

    cqe = None
    for _ in range(100):
        cand = bytes(ram[P.ACQ_GPA:P.ACQ_GPA + 16])
        if struct.unpack("<I", cand[12:16])[0] & (1 << 16):
            cqe = cand
            break
        time.sleep(0.02)
    P.check(TAG, cqe is not None, "CQE 出现在我们的 memfd（server 零拷贝 dma_write 写回）")
    dw3 = struct.unpack("<I", cqe[12:16])[0]
    P.check(TAG, (dw3 & 0xFFFF) == cid, f"CQE CID={cid:#x}")
    P.check(TAG, ((dw3 >> 17) & 0x7FFF) == 0, "CQE status=0 (success)")

    mn = bytes(ram[P.PRP1_GPA + 24:P.PRP1_GPA + 64]).decode("ascii", "replace").strip()
    P.log(TAG, f"Identify MN = {mn!r}")
    P.check(TAG, "NVMe" in mn or "OpenHCL" in mn, "Identify 结果零拷贝落进我们的 memfd")
    P.log(TAG, "零拷贝自证成立：未服务任何 DMA_READ/WRITE，Identify 仍完成 ⟹ server 走 mmap")


def main() -> int:
    binary = P.build_server()
    tmp = Path(tempfile.mkdtemp(prefix="poc1_"))
    sock_path, img = tmp / "nvme.sock", tmp / "ns1.img"
    img.write_bytes(b"\x00" * (1024 * 1024))
    gr = P.GuestRam()
    proc = subprocess.Popen(
        [str(binary), "--vfio-user-sock", str(sock_path), "--backing-file", str(img)],
        env=dict(os.environ, RUST_LOG="warn"),
    )
    try:
        P.wait_for_socket(sock_path, proc)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(8.0)
            s.connect(str(sock_path))
            run(s, gr.mem, gr.fd)
        P.log(TAG, "POC-1 PASSED ✓ — 非-QEMU client fd-passing 零拷贝 DMA 端到端可行")
        return 0
    except Exception as e:  # noqa: BLE001 — harness 顶层捕获
        P.log(TAG, f"POC-1 FAILED: {e}")
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
        gr.close()


if __name__ == "__main__":
    sys.exit(main())
