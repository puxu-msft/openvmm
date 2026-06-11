# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""POC-3（env-gated）：真 guest RAM fd 导出 + sidecar 自映射探针。

验证 Spec A 的另一条承重假设（调研指出、需真环境）：在 **OpenHCL VTL2** 里，一个
独立用户态 sidecar 进程能否拿到 VTL0 guest RAM 的 fd 并 mmap 之（线性 GPA→file_offset）。
调研结论：`/dev/mshv_vtl_low` 是 VTL2 内核设备，open + 按 memory_layout `map_file` 线性
映射（`openhcl/underhill_mem/src/mapping.rs` 即如此），GPA→file_offset 线性。

本探针**不复现整条路径**（无真 Hyper-V/OpenHCL 内核就跑不全），它做两件事：
1. 本地（WSL2 等）跑：探测 `/dev/mshv_vtl_low` / `/dev/mshv` 是否存在 → 预期**不存在**，
   确认这条路径确属 OpenHCL-VTL2-only（不是我们漏配）。
2. 在真 OpenHCL VTL2 里跑（设 `POC_ALLOW_MSHV=1`）：open 设备、按 `POC_GPA`/`POC_LEN`
   线性 mmap、读首 16 字节、写回一个 marker、再读验证 → 证明 sidecar 自映射 guest RAM 可行。

用法：
  python3 poc_mshv_probe.py                 # 本地：仅探测存在性
  POC_ALLOW_MSHV=1 POC_GPA=0x100000 POC_LEN=0x1000 python3 poc_mshv_probe.py   # 真 VTL2
"""
from __future__ import annotations

import mmap
import os
import struct
import sys

CANDIDATES = ["/dev/mshv_vtl_low", "/dev/mshv", "/dev/mshv_vtl"]


def log(m: str) -> None:
    print(f"[poc-mshv-probe] {m}", flush=True)


def main() -> int:
    present = {p: os.path.exists(p) for p in CANDIDATES}
    for p, ok in present.items():
        log(f"  {p}: {'present' if ok else 'absent'}")

    allow = os.environ.get("POC_ALLOW_MSHV") == "1"
    dev = "/dev/mshv_vtl_low"

    if not present.get(dev):
        log("")
        log(f"{dev} 不存在 → 当前非 OpenHCL VTL2 环境（WSL2/普通 Linux 预期如此）。")
        log("这确认了：真 guest RAM fd 导出路径是 OpenHCL-VTL2-only 的承重假设，")
        log("**未在本地证伪也未证成**——它是 env-gated 研究门，须在真 OpenHCL VTL2 内跑本探针。")
        log("POC-3 状态：ENV-GATED（本地只确认设备缺席，符合预期）")
        # 退出码 0：本地探测本身成功完成（不是失败，只是 env 受限）。
        return 0

    if not allow:
        log(f"{dev} 存在但未设 POC_ALLOW_MSHV=1 —— 拒绝在未明确授权下 mmap VTL0 内存。")
        return 0

    # ── 真 OpenHCL VTL2 路径 ──
    gpa = int(os.environ.get("POC_GPA", "0x100000"), 0)
    length = int(os.environ.get("POC_LEN", "0x1000"), 0)
    log(f"opening {dev}, 线性 mmap GPA={gpa:#x} len={length:#x} ...")
    fd = os.open(dev, os.O_RDWR)
    try:
        # 注：真实现 file_offset 可能需带 SHARED_MEMORY_FLAG(bit63) 取 shared 视图；
        # 这里用裸 GPA 作 file_offset（非-CVM 线性映射）。CVM 见 Spec B。
        m = mmap.mmap(fd, length, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE, offset=gpa)
        head = struct.unpack("<Q", m[0:8])[0]
        log(f"  读 GPA[{gpa:#x}] 首 8 字节 = {head:#x}")
        marker = 0x5A5A_0000_0000_5A5A
        m[0:8] = struct.pack("<Q", marker)
        back = struct.unpack("<Q", m[0:8])[0]
        m.close()
        if back == marker:
            log("  写回 marker + 重读一致 ⟹ sidecar 自映射 guest RAM 读写可行。")
            log("POC-3 PASSED ✓（真 OpenHCL VTL2 路径验证）")
            return 0
        log("POC-3 FAILED：写回不一致")
        return 1
    finally:
        os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
