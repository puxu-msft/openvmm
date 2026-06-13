#!/usr/bin/env python3
# 从 run_qemu_vfio_guest_capture.py 产的 JSON transcript 抽取「干净枚举/寄存器前缀」golden，
# 机械化 ADR-013 档2 frozen-vector 的 refresh（消除 JSON→txt 的人工断点 / 边界手算）。
#
# 边界判据（与 vfio_user_guest_replay_e2e.rs 模块 doc 一致）：首个 **BAR0 doorbell write**
# （c2s REGION_WRITE，region==0 且 offset>=0x1000）之前——该点起 server 读 mmap'd guest 内存，
# 0-fd replay 会分叉。前缀纯寄存器/枚举，响应与内存无关、确定。
#
# 用法（refresh 时，持 QEMU11+guest 资产者）：
#   python3 run_qemu_vfio_guest_capture.py /tmp/t.json
#   python3 extract_wire_prefix.py /tmp/t.json \
#       ../../../nvme_firmware/tests/data/vfio_guest_wire_prefix.txt
import json
import struct
import sys
from pathlib import Path

CMD_REGION_WRITE = 10


def first_doorbell_index(frames) -> int:
    """首个 BAR0(region 0) offset>=0x1000 的 c2s REGION_WRITE 帧序号；无则返 len。"""
    for i, f in enumerate(frames):
        if f["dir"] == "c2s" and f["cmd"] == CMD_REGION_WRITE:
            p = bytes.fromhex(f["payload_hex"])
            if len(p) >= 16:
                off, region, _cnt = struct.unpack_from("<QII", p, 0)
                if region == 0 and off >= 0x1000:
                    return i
    return len(frames)


def main() -> int:
    if len(sys.argv) < 2:
        sys.exit("用法: extract_wire_prefix.py <transcript.json> [out.txt]")
    src = Path(sys.argv[1])
    d = json.loads(src.read_text())
    frames = d["frames"]
    prov = d.get("provenance", {})
    boundary = first_doorbell_index(frames)
    pref = frames[:boundary]

    n_c2s = sum(1 for f in pref if f["dir"] == "c2s")
    n_s2c = len(pref) - n_c2s
    lines = [
        "# vfio-user 真 guest-kernel wire transcript — 干净枚举/寄存器前缀(ADR-013 档2 frozen-vector)",
        "# 由 run_qemu_vfio_guest_capture.py 抓 + extract_wire_prefix.py 机械抽取(勿手改)。",
        f"# provenance: qemu={prov.get('qemu', '?')} ; kernel={prov.get('guest_kernel', '?')}"
        f" ; marker={prov.get('captured_marker', '?')}",
        f"# 边界=首个 BAR0 doorbell write(帧 {boundary})之前——纯寄存器/枚举,响应与 guest 内存无关、确定。",
        "# refresh(目标 kernel/QEMU major bump 时,持 QEMU 环境者负责):",
        "#   python3 run_qemu_vfio_guest_capture.py /tmp/t.json",
        "#   python3 extract_wire_prefix.py /tmp/t.json ../../../nvme_firmware/tests/data/vfio_guest_wire_prefix.txt",
        '# 格式:每行 "<c2s|s2c> <frame_hex>";# 注释。replay 不传 fd(DMA_MAP message-mode 回同 golden)。',
    ]
    for f in pref:
        lines.append(f"{f['dir']} {f['header_hex']}{f['payload_hex']}")
    lines.append(f"# 统计: {len(pref)} 帧(c2s {n_c2s} / s2c {n_s2c})")

    out = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    text = "\n".join(lines) + "\n"
    if out:
        out.write_text(text)
        # NO_REPLY = c2s 帧 flags(header 字节 8..12,u32 LE)bit 0x10。
        no_reply = sum(
            1
            for f in pref
            if f["dir"] == "c2s"
            and (int.from_bytes(bytes.fromhex(f["header_hex"])[8:12], "little") & 0x10)
        )
        print(f"已写 {out}: {len(pref)} 帧(c2s {n_c2s} / s2c {n_s2c}),边界帧 {boundary}")
        print(
            f"⚠ replay test 锚定 sent={n_c2s} / checked={n_c2s - no_reply}(NO_REPLY {no_reply} 个)"
            " —— 若 refresh 后这些数变,同步改 vfio_user_guest_replay_e2e.rs 的锚定 ensure。"
        )
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
