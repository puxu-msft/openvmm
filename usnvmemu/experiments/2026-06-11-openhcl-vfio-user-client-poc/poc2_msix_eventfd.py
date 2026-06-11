# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""POC-2：MSI-X eventfd 中断信号（firmware → client，非-CVM，本地可跑）。

验 Spec A 承重假设：firmware server 完成 NVMe 命令后，经 client 通过 SET_IRQS
（DATA_EVENTFD，SCM_RIGHTS）配下的 eventfd **触发 MSI-X 中断信号**。这是
vfio_user_device 的中断路径的 firmware→client 半段（VTL0 注入半段是 OpenHCL 特有的
`Interrupt::deliver`，需真环境，不在本 POC）。

注：POC-1 日志里 admin CQ 已尝试 fire vector 0（"向量未配置 eventfd"）——本 POC 先
SET_IRQS 配好 eventfd，再驱动 Identify，验证 firmware 真把它 fire 了（client read
eventfd 得到 +1）。

用法：`python3 poc2_msix_eventfd.py`。详见同目录 README.md。
"""
from __future__ import annotations

import os
import select
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import poclib as P
import vfio_proto as v

TAG = "poc2-msix-eventfd"
NUM_VECTORS = 4  # 设备 describe MSI-X count


def run(sock: socket.socket, ram, memfd: int, evfds: list[int]) -> None:
    from poc1_dma_zerocopy import MsgId

    nxt = MsgId()

    v.version_handshake(sock, nxt())
    irq = v.get_irq_info(sock, nxt(), v.IRQ_MSIX)
    P.check(TAG, irq["count"] == NUM_VECTORS, f"GET_IRQ_INFO MSI-X count={NUM_VECTORS}")

    P.dma_map_with_fd(sock, nxt(), 0, P.RAM_SIZE, memfd)

    # 关键：先 SET_IRQS 把 NUM_VECTORS 个 eventfd 配给 MSI-X 向量（start=0）。
    P.set_irqs_eventfds(sock, nxt(), 0, evfds)
    P.log(TAG, f"SET_IRQS 配下 {len(evfds)} 个 eventfd（vector 0..{len(evfds) - 1}）")

    P.nvme_enable(sock, nxt)

    # 确认尚未触发（admin CQ 还没完成任何命令）。
    r, _, _ = select.select([evfds[0]], [], [], 0)
    P.check(TAG, evfds[0] not in r, "doorbell 前 vector-0 eventfd 未就绪（无杂散中断）")

    cid = 0x0043
    ram[P.ASQ_GPA:P.ASQ_GPA + 64] = P.build_identify_sqe(cid, P.PRP1_GPA)
    ram[P.ACQ_GPA:P.ACQ_GPA + 16] = b"\x00" * 16
    v.region_write(sock, nxt(), v.REGION_BAR0, P.SQ0TDBL, struct.pack("<I", 1))
    P.log(TAG, "rang SQ0 doorbell；等 firmware fire vector-0 eventfd ...")

    # 等 vector-0 eventfd 就绪（firmware 完成 Identify → post_cqe → fire_interrupt）。
    r, _, _ = select.select([evfds[0]], [], [], 5.0)
    P.check(TAG, evfds[0] in r, "vector-0 eventfd 就绪（firmware 真触发了 MSI-X 信号）")
    val = struct.unpack("<Q", os.read(evfds[0], 8))[0]
    P.check(TAG, val >= 1, f"eventfd 计数 = {val}（≥1 ⟹ 至少一次中断）")

    # 其它向量不应被误触发。
    r, _, _ = select.select(evfds[1:], [], [], 0)
    P.check(TAG, not r, "其它 MSI-X 向量未被误触发（中断路由到正确向量）")
    P.log(TAG, "中断路径 firmware→client 半段验证通过（eventfd 信号送达）")


def main() -> int:
    binary = P.build_server()
    tmp = Path(tempfile.mkdtemp(prefix="poc2_"))
    sock_path, img = tmp / "nvme.sock", tmp / "ns1.img"
    img.write_bytes(b"\x00" * (1024 * 1024))
    gr = P.GuestRam()
    evfds = [os.eventfd(0, os.EFD_NONBLOCK) for _ in range(NUM_VECTORS)]
    proc = subprocess.Popen(
        [str(binary), "--vfio-user-sock", str(sock_path), "--backing-file", str(img)],
        env=dict(os.environ, RUST_LOG="warn"),
    )
    try:
        P.wait_for_socket(sock_path, proc)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(8.0)
            s.connect(str(sock_path))
            run(s, gr.mem, gr.fd, evfds)
        P.log(TAG, "POC-2 PASSED ✓ — firmware 经 client 配下的 eventfd 触发 MSI-X 信号")
        return 0
    except Exception as e:  # noqa: BLE001
        P.log(TAG, f"POC-2 FAILED: {e}")
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
        gr.close()
        for fd in evfds:
            os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
