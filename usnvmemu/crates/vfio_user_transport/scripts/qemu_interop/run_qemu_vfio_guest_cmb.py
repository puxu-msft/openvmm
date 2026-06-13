#!/usr/bin/env python3
# CMB L4 真机 e2e —— 在 run_qemu_vfio_guest 基础上让 firmware 起 `--cmb-mode trap|map`，
# 验证**真 Linux `nvme` 驱动真用 CMB**（in-process harness 测不出的真机层，见设计 §11 L4）。
#
# 数据流（trap）：guest 驱动默认 `use_cmb_sqes` → 把 IO SQ 放进 CMB BAR → guest 写 SQE
# 经 REGION_WRITE 落 firmware CMB backing → firmware 取 SQE 经 `guest_read(CBA+off)` 命中
# CMB → `cmb.rs::note_first_cmb_access` 打 `CMB-RESIDENT-ACCESS`。map：guest mmap region fd
# 直写 SQE（零拷贝），firmware 取 SQE 同样命中 CMB。
#
# 三独立 oracle（须全一致才 PASS）：
#   1. guest 侧：init 真 NVMe write/flush/read marker，串口 GUEST_RESULT=PASS。
#   2. host 侧：裸读 backing file 头部找同一 marker（与 guest→驱动→vfio-user 不同路径）。
#   3. CMB 侧（本 harness 新增）：firmware 日志含 `CMB-RESIDENT-ACCESS` —— 证驱动真把
#      SQ/data 放进 CMB、firmware 真从 CMB backing 服务（不是"CMB 广告了但没人用"）。
#
# 用法：
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/fetch_guest_kernel.sh   # 一次
#   CMB_MODE=trap python3 run_qemu_vfio_guest_cmb.py     # 或 CMB_MODE=map
#
# 环境变量：CMB_MODE(trap|map，默认 trap) / QEMU_BIN / NVME_BIN / GUEST_KERNEL / KEEP_LOGS
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import run_qemu_vfio_guest as base

CMB_SIZE = 2 * 1024 * 1024  # 2 MiB（2 的幂 + 4 KiB 倍数，满足 enable_cmb 校验）
CMB_BIR = 2


def main() -> int:
    mode = os.environ.get("CMB_MODE", "trap")
    if mode not in ("trap", "map"):
        sys.exit(f"CMB_MODE 须 trap|map，got {mode!r}")

    qemu_bin = base.find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", base.DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(f"找不到 nvme_firmware：{nvme_bin}（先 cargo build --features vfio-user）")
    kernel = base.find_kernel()
    initrd = base.ensure_initrd()
    marker = "CMB-L4-" + time.strftime("%Y%m%d%H%M%S")

    tmp = Path(tempfile.mkdtemp(prefix="qemu_vfio_cmb_"))
    sock = tmp / "nvme.sock"
    img = tmp / "nvme.img"
    srv_log_path = tmp / "server.log"
    serial_path = tmp / "serial.log"
    qemu_log_path = tmp / "qemu.log"
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)

    srv_log = open(srv_log_path, "w")
    srv = subprocess.Popen(
        [
            str(nvme_bin),
            "--vfio-user-sock", str(sock),
            "--backing-file", str(img),
            "--cmb-mode", mode,
            "--cmb-size", str(CMB_SIZE),
            "--cmb-bir", str(CMB_BIR),
        ],
        env=dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "info")),
        stdout=srv_log,
        stderr=subprocess.STDOUT,
    )
    qemu = None
    try:
        for _ in range(200):
            if sock.exists():
                break
            if srv.poll() is not None:
                print(f"=== server 早退 code={srv.returncode} ===")
                print(srv_log_path.read_text()[-2000:])
                return 1
            time.sleep(0.05)

        print(f"=== server up（--cmb-mode {mode}）；boot 真 guest（marker={marker}）===", flush=True)
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
                "-append", f"console=ttyS0 panic=-1 rdinit=/init gmarker={marker}",
                "-device", f'{{"driver":"vfio-user-pci","socket":{{"path":"{sock}","type":"unix"}}}}',
                "-serial", f"file:{serial_path}",
                "-display", "none",
                "-no-reboot",
            ],
            stdout=qemu_log,
            stderr=subprocess.STDOUT,
        )
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s 未 poweroff）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        srv_log.flush()
        server_log = srv_log_path.read_text(errors="replace") if srv_log_path.exists() else ""
        print("=== guest 串口尾部 ===")
        print("".join(serial.splitlines(keepends=True)[-25:]))

        guest_pass = "GUEST_RESULT=PASS" in serial
        host_found = marker.encode() in img.read_bytes()[:4096]
        cmb_used = "CMB-RESIDENT-ACCESS" in server_log
        print(
            f"=== oracle: guest IO PASS={guest_pass} ; host backing marker={host_found} ; "
            f"CMB-USED={cmb_used} ==="
        )

        if guest_pass and host_found and cmb_used:
            print(f"=== PASS (L4,{mode})：真 nvme 驱动真用 CMB + 真 IO，三 oracle 一致 ===")
            return 0
        if "GUEST_RESULT=NO_NVME" in serial:
            print("=== FAIL: guest 没枚举到 /dev/nvme0n1（vfio-user 设备未绑定）===")
        elif not cmb_used:
            print(
                "=== FAIL: firmware 日志无 CMB-RESIDENT-ACCESS —— guest 驱动没把 SQ/data 放进 "
                "CMB（查 use_cmb_sqes / VS 版本 / CMBSZ 阈值 / CMSE 是否被编程）==="
            )
        print("=== FAIL ===")
        return 1
    except Exception as e:  # noqa: BLE001
        print(f"=== FAILED: {e} ===")
        if qemu_log_path.exists():
            print(qemu_log_path.read_text()[-1500:])
        return 1
    finally:
        if qemu and qemu.poll() is None:
            qemu.terminate()
            try:
                qemu.wait(timeout=3)
            except subprocess.TimeoutExpired:
                qemu.kill()
        if srv.poll() is None:
            srv.terminate()
            try:
                srv.wait(timeout=3)
            except subprocess.TimeoutExpired:
                srv.kill()
        srv_log.close()
        print("--- server log（尾 30 行，含 CMBMSC 编程 / CMB 访问）---")
        if srv_log_path.exists():
            print("".join(srv_log_path.read_text().splitlines(keepends=True)[-30:]))
        if not os.environ.get("KEEP_LOGS"):
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
