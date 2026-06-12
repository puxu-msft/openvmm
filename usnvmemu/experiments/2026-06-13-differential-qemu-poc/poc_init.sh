#!/bin/busybox sh
# 差分 POC guest init —— 在真 QEMU guest 内跑，同一内核 nvme 驱动同时绑定两个本地
# PCIe NVMe：usnvmemu(vfio-user-pci) 与 QEMU 自带 -device nvme。对两者经 pt_diff 发
# **同一条** passthru 命令，打印 status/result/errno，host 侧 diff。
#
# 这验证 reviewer 建议的承重假设：同一命令流喂两个独立 controller 实现，能否抓到
# "两边各自 self-consistent 但语义分歧"的差异（anchor 常量抓不到）。
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null

if [ -d /modules ]; then
    for m in nvme-keyring nvme-auth nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
    for m in nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
fi

# 等两个 controller 都枚举
i=0
while [ $i -lt 150 ]; do
    n=$(ls -d /sys/class/nvme/nvme* 2>/dev/null | wc -l)
    [ "$n" -ge 2 ] && break
    sleep 0.1; i=$((i + 1))
done

echo "POC: nvme controllers:"; ls -ld /sys/class/nvme/nvme* 2>/dev/null
UDEV=""; QDEV=""
for c in /sys/class/nvme/nvme*; do
    [ -e "$c/model" ] || continue
    m=$(cat "$c/model" 2>/dev/null)
    d=/dev/$(basename "$c")
    echo "POC: $d model=[$m]"
    case "$m" in
        *QEMU*) QDEV=$d ;;
        *)      UDEV=$d ;;
    esac
done
echo "POC: UDEV(usnvmemu)=$UDEV  QDEV(qemu)=$QDEV"
if [ -z "$UDEV" ] || [ -z "$QDEV" ]; then
    echo "POC_RESULT=MISSING_DEVICE udev=$UDEV qdev=$QDEV"
    poweroff -f
fi

# run NAME ADMIN|IO OPCODE NSID C10 C11 C12 C13 C14 C15 DATALEN DIR
run() {
    name=$1; shift
    u=$(/bin/pt_diff "$UDEV" "$@")
    q=$(/bin/pt_diff "$QDEV" "$@")
    echo "PTDIFF|$name|US $u|QEMU $q"
}

echo "POC: ==== command matrix begin ===="
# --- baseline sanity（两端应一致成功 status=0x0000）---
run id_ctrl       admin 0x06 0x00000001 0x00000001 0 0 0 0 0 4096 r   # Identify CNS=1 (ctrl)
run id_ns         admin 0x06 0x00000001 0x00000000 0 0 0 0 0 4096 r   # Identify CNS=0 (ns), nsid=1
run getlog_err    admin 0x02 0x00000000 0x000f0001 0 0 0 0 0 64   r   # Get Log LID=1 Error, 16 dw
run getfeat_arb   admin 0x0a 0x00000000 0x00000001 0 0 0 0 0 0    n   # Get Features FID=1 Arbitration

# --- 语义分歧候选（admin）---
run id_cns_resvd  admin 0x06 0x00000001 0x0000001f 0 0 0 0 0 4096 r   # Identify CNS=0x1f 保留/未知
run id_ns_bcast   admin 0x06 0xffffffff 0x00000000 0 0 0 0 0 4096 r   # Identify CNS=0 nsid=广播(非法)
run getlog_unk    admin 0x02 0x00000000 0x000f00ff 0 0 0 0 0 64   r   # Get Log LID=0xff 未知
run getfeat_resvd admin 0x0a 0x00000000 0x0000007f 0 0 0 0 0 0    n   # Get Features FID=0x7f 保留
run admin_badopc  admin 0xfe 0x00000000 0 0 0 0 0 0 0 n               # 未支持 admin opcode

# --- 语义分歧候选（io）---
run io_read_ok    io 0x02 0x00000001 0 0 0 0 0 0 512 r                # Read slba=0 nlb=0(1 块)
run io_read_oob   io 0x02 0x00000001 0xffffffff 0x0000ffff 0 0 0 0 512 r  # Read slba 越界 → LBA OOR
run io_read_ns0   io 0x02 0x00000000 0 0 0 0 0 0 512 r                # Read nsid=0 非法
run io_badopc     io 0xff 0x00000001 0 0 0 0 0 0 0 n                  # 未支持 io opcode
run io_flush      io 0x00 0x00000001 0 0 0 0 0 0 0 n                  # Flush（应成功）
echo "POC: ==== command matrix end ===="
echo "POC_RESULT=DONE"
poweroff -f
