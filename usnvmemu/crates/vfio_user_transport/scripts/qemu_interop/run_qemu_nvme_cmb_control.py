#!/usr/bin/env python3
# CMB L4 S6 对照实验 —— QEMU 自带 emulated `nvme`(cmb_size_mb)同 guest 跑。
#
# 决定 fork-QEMU 对不对路：用 QEMU **自己的** emulated nvme（非 vfio-user）在同一 guest
# 跑 CMB，看真 Linux 驱动是否把 SQ/data 放进 CMB。
#   - QEMU trace `pci_nvme_map_addr_cmb` 触发 ⟹ emulated nvme 的 CMB 真被访问（SQ/data 落
#     CMB）⟹ guest+kernel CAN do it ⟹ 洞**专属我们的 vfio-user 路径**（fork/改 QEMU vfio-user
#     对路）。
#   - 不触发 ⟹ QEMU 模拟-BAR 的 p2pdma 在此 guest 也不工作 ⟹ **通病**（fork 自家 vfio-user
#     白搭，问题在 QEMU 模拟 BAR / 内核 / 拓扑更上游）。
#
# 用法：
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   python3 run_qemu_nvme_cmb_control.py            # 默认
#   GUEST_EXTRA_CMDLINE="nvme.use_cmb_sqes=1" python3 run_qemu_nvme_cmb_control.py
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import run_qemu_vfio_guest as base


def main() -> int:
    qemu_bin = base.find_qemu()
    kernel = base.find_kernel()
    initrd = base.ensure_initrd()
    marker = "CMB-CTRL-" + time.strftime("%Y%m%d%H%M%S")
    extra_cmdline = os.environ.get("GUEST_EXTRA_CMDLINE", "")

    tmp = Path(tempfile.mkdtemp(prefix="qemu_nvme_ctrl_"))
    img = tmp / "nvme.img"
    serial_path = tmp / "serial.log"
    qemu_log_path = tmp / "qemu.log"
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)

    qemu_log = open(qemu_log_path, "w")
    qemu = subprocess.Popen(
        [
            qemu_bin,
            "-machine", "q35,accel=kvm:tcg",
            "-cpu", "host",
            "-smp", "2",
            "-m", "512M",
            "-object", "memory-backend-memfd,id=mem,size=512M,share=on",
            "-machine", "memory-backend=mem",
            "-kernel", str(kernel),
            "-initrd", str(initrd),
            "-append", f"console=ttyS0 panic=-1 rdinit=/init gmarker={marker} {extra_cmdline}".strip(),
            # QEMU 自带 emulated nvme + 2 MiB CMB（与我们 firmware 同 BAR2 CMB 对照）。
            "-drive", f"id=nvmedrv,file={img},format=raw,if=none",
            "-device", "nvme,drive=nvmedrv,serial=cmbctrl,cmb_size_mb=2",
            # trace：map_addr_cmb 触发即"CMB 真被访问"；create_sq 看 SQ 地址。
            "-trace", "pci_nvme_map_addr_cmb",
            "-trace", "pci_nvme_create_sq",
            "-serial", f"file:{serial_path}",
            "-display", "none",
            "-no-reboot",
        ],
        stdout=qemu_log,
        stderr=subprocess.STDOUT,
    )
    try:
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        qemu_log.flush()
        qlog = qemu_log_path.read_text(errors="replace") if qemu_log_path.exists() else ""
        print("=== guest 串口尾部 ===")
        print("".join(serial.splitlines(keepends=True)[-20:]))

        # QEMU 早退（device 参数错）检测
        if "GUEST_RESULT" not in serial and ("error" in qlog.lower() or "qemu" in qlog.lower()):
            print("=== QEMU log 尾（可能设备参数/早退）===")
            print("".join(qlog.splitlines(keepends=True)[-25:]))

        guest_pass = "GUEST_RESULT=PASS" in serial
        p2p = "added peer-to-peer DMA memory" in serial
        sq_traces = [ln for ln in qlog.splitlines() if "pci_nvme_create_sq" in ln]
        # 真 oracle = create_sq 的 addr：IO SQ(sqid≥1，非 admin)落在高 MMIO/CMB 区(≥0x8000_0000，
        # 远高于 512 MiB host RAM 0x2000_0000)即"SQ 在 CMB"。`pci_nvme_map_addr_cmb` 是 DMA-data
        # 路径的 trace，SQ-fetch 不走它，故不可作 oracle（前版误用）。
        import re

        sq_in_cmb = False
        for ln in sq_traces:
            m_addr = re.search(r"addr=0x([0-9a-fA-F]+)", ln)
            m_sqid = re.search(r"sqid=(\d+)", ln)
            if m_addr and m_sqid and int(m_sqid.group(1)) >= 1:
                if int(m_addr.group(1), 16) >= 0x8000_0000:
                    sq_in_cmb = True
        print(
            f"=== oracle: guest IO PASS={guest_pass} ; p2p registered={p2p} ; "
            f"IO-SQ-in-CMB(create_sq addr≥0x80000000)={sq_in_cmb} ==="
        )
        if sq_traces:
            print("=== pci_nvme_create_sq traces（前 6）===")
            print("\n".join(sq_traces[:6]))

        print("=== 对照结论 ===")
        if sq_in_cmb:
            print(
                "emulated nvme 的 IO SQ 落在 CMB（create_sq addr≥0x80000000）⟹ guest+kernel CAN "
                "把 SQ 放进 CMB ⟹ 洞**专属 vfio-user 路径**（我们 firmware 的 SQ 落 host RAM）⟹ "
                "fork/改 QEMU vfio-user 的 BAR-as-DMA-target 暴露 **对路**。"
            )
            return 0
        print(
            "emulated nvme 的 IO SQ 也不在 CMB ⟹ QEMU 模拟-BAR p2pdma 在此 guest **通病** ⟹ "
            "fork 自家 vfio-user **白搭**（问题更上游）。"
        )
        return 1
    finally:
        if qemu.poll() is None:
            qemu.terminate()
            try:
                qemu.wait(timeout=3)
            except subprocess.TimeoutExpired:
                qemu.kill()
        qemu_log.close()
        if not os.environ.get("KEEP_LOGS"):
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
