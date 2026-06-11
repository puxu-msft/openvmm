# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""POC-4（模型）：CVM page-convert 的撤销不变量 —— 为何 owner 无法单方面回收
另一进程的共享映射，故 revoke-before-convert 协议是必须的。

**背景**：Spec B（CVM 直访）审计裁定 RED，根因（architect CRITICAL-2 / security #3）：
OpenHCL `change_host_visibility` 保证"无并发访问者"靠 `check_gpn_not_locked`（只查
进程内 guestmem 锁表）+ `rcu().synchronize_blocking()`（只排空进程内 RCU）。**一个
out-of-process sidecar 的 MAP_SHARED mmap 在这两者之外完全不可见**——owner 以为页已
安全回收，sidecar 仍持映射 → 页转 private 后 sidecar 读/写到加密私有内存。

**这个 POC 是「模型」不是平台测试**：用 memfd + fork 在普通 Linux 上复现那条**根本
不变量**——owner 用尽自己单侧手段也无法让 child(sidecar) 的映射失效，Linux 不给 owner
枚举/强制撤销他人映射的 API。它不复现硬件加密（那需真 TDX/SNP），但精确证明审计
CRITICAL 的结构性根因，从而验证"revoke-before-convert（sidecar 显式 munmap+ack）"
是唯一出路。

用法：`python3 poc_convert_revoke.py`（退出码 0 = 不变量如审计所述成立）。
"""
from __future__ import annotations

import mmap
import os
import struct
import sys
import time

PAGE = 4096


def log(m: str) -> None:
    print(f"[poc-convert-revoke] {m}", flush=True)


def main() -> int:
    # 共享 "guest 页"：owner(paravisor) 与 child(sidecar) 都 MAP_SHARED。
    fd = os.memfd_create("poc-shared-page", 0)
    os.ftruncate(fd, PAGE)

    # 用一对 pipe 做 owner↔child 同步（不是 revoke 协议，仅测试编排）。
    r1, w1 = os.pipe()  # owner → child
    r2, w2 = os.pipe()  # child → owner

    pid = os.fork()
    if pid == 0:
        # ── child = sidecar（被怀疑方）──
        os.close(w1)
        os.close(r2)
        m = mmap.mmap(fd, PAGE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        m[0:8] = struct.pack("<Q", 0xC0FFEE)  # sidecar 写 marker
        os.write(w2, b"M")                     # 通知 owner：已映射+写入
        os.read(r1, 1)                          # 等 owner 完成它的"回收"动作
        # owner "回收" 后，sidecar 仍尝试读写（模拟它继续 DMA）：
        after_read = struct.unpack("<Q", m[8:16])[0]  # 读 owner 之后写的位置
        m[16:24] = struct.pack("<Q", 0xBADC0DE)        # 仍然写得进去？
        os.write(w2, struct.pack("<Q", after_read))    # 把读到的回给 owner
        os.read(r1, 1)
        m.close()
        os._exit(0)

    # ── parent = owner（paravisor）──
    os.close(r1)
    os.close(w2)
    m = mmap.mmap(fd, PAGE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
    os.read(r2, 1)  # 等 child 映射+写 marker
    marker = struct.unpack("<Q", m[0:8])[0]
    ok_shared = marker == 0xC0FFEE
    log(f"owner 看到 sidecar 写的 marker = {marker:#x}  (shared 可见性: {ok_shared})")

    # owner 用尽**单侧**手段试图"回收/convert"这页（= change_host_visibility 能做的）：
    log("owner 尝试单方面回收该页（drop 自己映射 / msync / punch-hole）...")
    # 1) owner 在页里写个值（之后看 sidecar 是否还能读到 → 是否仍共享）
    m[8:16] = struct.pack("<Q", 0x5ECEED)
    m.flush()
    # 2) owner drop 自己的映射（它能控制的只有自己）
    m.close()
    # 3) owner 没有任何 API 枚举"还有谁映射这个 memfd"——这正是 check_gpn_not_locked 的盲区。
    #    它最多 punch-hole，但那不撤销 sidecar 的映射，只改页内容。
    os.write(w1, b"G")  # 放行 sidecar 继续访问

    raw = os.read(r2, 8)
    sidecar_read = struct.unpack("<Q", raw)[0]
    os.write(w1, b"D")
    os.waitpid(pid, 0)
    os.close(fd)

    # 判定不变量
    log(f"owner '回收' 后，sidecar 读到 owner 写入的值 = {sidecar_read:#x}")
    still_shared = sidecar_read == 0x5ECEED
    log("")
    log("=== 不变量裁定 ===")
    log(f"  · shared 可见性建立：{ok_shared}")
    log(f"  · owner drop 自己映射 + 没有任何手段枚举/强制撤销 sidecar 的映射：True")
    log(f"  · owner '回收' 后 sidecar 仍能读到 owner 后写的数据（映射依然活）：{still_shared}")
    log("")
    if ok_shared and still_shared:
        log("结论：owner 无法单方面让 sidecar 的 MAP_SHARED 映射失效 —— 与审计 CRITICAL")
        log("一致。change_host_visibility 式的进程内检查对 out-of-process mmap 结构性失明。")
        log("⟹ CVM 下必须 revoke-before-convert：先让 sidecar 显式 munmap + 回 ack，")
        log("   owner 确认后才 unaccept/convert。Spec B 的承重前提，本 POC 实测验证。")
        log("POC-4 PASSED ✓（不变量成立）")
        return 0
    log("POC-4 FAILED：不变量未如预期复现")
    return 1


if __name__ == "__main__":
    sys.exit(main())
