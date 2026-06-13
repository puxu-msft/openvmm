#!/bin/busybox sh
# CMB SQ 回退点定位实验 —— 在真 QEMU guest 内用 ftrace/kretprobe 抓 Linux nvme 驱动
# `nvme_alloc_sq_cmds`（v6.8 drivers/nvme/host/pci.c）的 CMB 回退判定点。
#
# 要回答的问题：guest IO SQ 落 host RAM（而非 CMB BAR），是因为
#   (a) pci_alloc_p2pmem 返 NULL（p2p 池分配失败），还是
#   (b) pci_p2pmem_virt_to_bus 返 0（拿不到 bus 地址），还是
#   (c) dev->cmb_use_sqes 本就 false（连 if 分支都没进）？
#
# 手段：nvme_alloc_sq_cmds 被编译器 inline 进 nvme_create_queue（nvme.ko 符号表里
# 无独立符号），无法直接 kprobe 它。但它调用的 pci_alloc_p2pmem / pci_p2pmem_virt_to_bus
# 是 vmlinux 真导出符号（nvme.ko 里以 R_X86_64_PLT32 真调用形式存在，非 inline），
# 故用 **boot 内核已加载的 vmlinux 符号**挂 kprobe/kretprobe，在 insmod nvme 触发
# probe 前布置好，抓两函数的命中次数 + 返回值，直接区分 (a)/(b)/(c)：
#   - pci_alloc_p2pmem 命中 0 次          → 没进 if 分支 → (c) cmb_use_sqes==false
#   - pci_alloc_p2pmem 返 0x0（NULL）      → (a)
#   - pci_alloc_p2pmem 返非 0 但
#     pci_p2pmem_virt_to_bus 返 0          → (b)
#   - 两者都返非 0                          → CMB 成功（预期不会在本实验出现）
#
# 辅助 oracle：nvme_pci_supports_pci_p2pdma（决定能否设 cmb_use_sqes）的返回值，
# 以及 p2pmem sysfs（size/available）。

/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mount -t debugfs debugfs /sys/kernel/debug 2>/dev/null
# tracefs 在新内核可能要单独挂
mount -t tracefs nodev /sys/kernel/tracing 2>/dev/null

# ---- 0. 环境能力自检（如实报告，不假设）----
echo "FTRACE: ===== capability probe ====="
TR=/sys/kernel/tracing
[ -d "$TR" ] || TR=/sys/kernel/debug/tracing
if [ -d "$TR" ]; then
    echo "FTRACE: tracefs at $TR"
else
    echo "FTRACE: NO tracefs (CONFIG_FTRACE / debugfs 缺失) — 转间接证据"
    TR=""
fi
if [ -n "$TR" ] && [ -e "$TR/kprobe_events" ]; then
    echo "FTRACE: kprobe_events present (CONFIG_KPROBE_EVENTS=y)"
    HAVE_KPROBE=1
else
    echo "FTRACE: NO kprobe_events — 无法挂 kprobe，转间接证据"
    HAVE_KPROBE=0
fi
# 确认目标符号在 kallsyms 里（kptr_restrict 可能隐藏地址，但名字可见即可挂 kprobe）
if [ -e /proc/kallsyms ]; then
    for s in pci_alloc_p2pmem pci_p2pmem_virt_to_bus nvme_pci_supports_pci_p2pdma; do
        if grep -qw "$s" /proc/kallsyms; then echo "FTRACE: sym $s = present"; else echo "FTRACE: sym $s = MISSING"; fi
    done
fi

# ---- 1. 布置 kprobe/kretprobe（务必在 insmod nvme 之前）----
if [ "$HAVE_KPROBE" = "1" ]; then
    echo "FTRACE: installing kprobes…"
    # 先清空旧的（幂等）
    echo > "$TR/kprobe_events" 2>/dev/null
    # kretprobe 抓返回值：r:<group>/<name> <symbol> arg=$retval:x64
    # pci_alloc_p2pmem 返 void*（NULL 判 a）
    echo 'r:cmbprobe/alloc_p2pmem_ret pci_alloc_p2pmem rv=$retval:x64' >> "$TR/kprobe_events" 2>/dev/null \
        && echo "FTRACE: + kretprobe pci_alloc_p2pmem" || echo "FTRACE: ! kretprobe pci_alloc_p2pmem FAILED"
    # 入口也挂一个，确认是否被调用（命中计数）
    echo 'p:cmbprobe/alloc_p2pmem_in pci_alloc_p2pmem' >> "$TR/kprobe_events" 2>/dev/null \
        && echo "FTRACE: + kprobe pci_alloc_p2pmem (entry)" || echo "FTRACE: ! kprobe pci_alloc_p2pmem entry FAILED"
    # pci_p2pmem_virt_to_bus 返 dma bus 地址（0 判 b）
    echo 'r:cmbprobe/virt_to_bus_ret pci_p2pmem_virt_to_bus rv=$retval:x64' >> "$TR/kprobe_events" 2>/dev/null \
        && echo "FTRACE: + kretprobe pci_p2pmem_virt_to_bus" || echo "FTRACE: ! kretprobe pci_p2pmem_virt_to_bus FAILED"
    # nvme_pci_supports_pci_p2pdma 返 bool（决定 cmb_use_sqes，间接判 c）
    echo 'r:cmbprobe/supports_p2pdma_ret nvme_pci_supports_pci_p2pdma rv=$retval:x64' >> "$TR/kprobe_events" 2>/dev/null \
        && echo "FTRACE: + kretprobe nvme_pci_supports_pci_p2pdma" || echo "FTRACE: (nvme_pci_supports_pci_p2pdma not yet — module sym, 可能要 insmod 后)"
    # 启用所有 cmbprobe 事件 + 打开 tracing
    echo 1 > "$TR/events/cmbprobe/enable" 2>/dev/null && echo "FTRACE: cmbprobe events enabled" || echo "FTRACE: ! enable cmbprobe FAILED"
    echo > "$TR/trace" 2>/dev/null
    echo 1 > "$TR/tracing_on" 2>/dev/null
    echo "FTRACE: active kprobe_events:"; cat "$TR/kprobe_events" 2>/dev/null
fi

# ---- 2. 加载 nvme 驱动（触发 probe → nvme_create_queue → 回退判定点）----
if [ -d /modules ]; then
    for m in nvme-keyring nvme-auth nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
    for m in nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
fi

# nvme.ko 加载后，nvme_pci_supports_pci_p2pdma 才成为 kallsyms 符号 —— 补挂一次
if [ "$HAVE_KPROBE" = "1" ] && grep -qw nvme_pci_supports_pci_p2pdma /proc/kallsyms; then
    if ! cat "$TR/kprobe_events" 2>/dev/null | grep -q supports_p2pdma_ret; then
        echo 'r:cmbprobe/supports_p2pdma_ret nvme_pci_supports_pci_p2pdma rv=$retval:x64' >> "$TR/kprobe_events" 2>/dev/null \
            && echo "FTRACE: (post-insmod) + kretprobe nvme_pci_supports_pci_p2pdma" || true
        echo 1 > "$TR/events/cmbprobe/enable" 2>/dev/null
    fi
fi

MARKER=$(sed -n 's/.*gmarker=\([^ ]*\).*/\1/p' /proc/cmdline)
echo "GUEST: cmdline marker=[$MARKER]"

# 等 namespace 枚举
i=0
while [ $i -lt 100 ]; do [ -e /dev/nvme0n1 ] && break; sleep 0.1; i=$((i + 1)); done
echo "GUEST: nvme nodes:"; ls -l /dev/nvme* 2>/dev/null

# ---- 3. 做一次真 IO（迫使 IO 队列真正被创建，触发 nvme_alloc_sq_cmds）----
if [ -e /dev/nvme0n1 ]; then
    printf '%s' "$MARKER" > /tmp/m
    dd if=/tmp/m of=/dev/nvme0n1 bs=512 count=1 conv=fsync,sync 2>/dev/null
    sync
    RB=$(dd if=/dev/nvme0n1 bs=512 count=1 2>/dev/null | head -c ${#MARKER})
    echo "GUEST: readback=[$RB]"
    [ "$RB" = "$MARKER" ] && echo "GUEST_RESULT=PASS marker=$MARKER" || echo "GUEST_RESULT=FAIL_MISMATCH rb=[$RB]"
else
    echo "GUEST_RESULT=NO_NVME"
fi

# ---- 4. dump trace 结果（判定证据）----
echo "FTRACE: ===== trace output ====="
if [ -n "$TR" ] && [ "$HAVE_KPROBE" = "1" ]; then
    # 命中计数（哪个 probe 触发了几次）
    echo "FTRACE: --- per-probe hit counts ---"
    for p in alloc_p2pmem_in alloc_p2pmem_ret virt_to_bus_ret supports_p2pdma_ret; do
        # tracefs 没有直接 count，扫 trace 文本数行
        n=$(grep -c "cmbprobe/$p\| $p:" "$TR/trace" 2>/dev/null || echo 0)
        echo "FTRACE: probe $p hits(approx)=$n"
    done
    echo "FTRACE: --- raw trace (cmbprobe lines) ---"
    grep -E 'alloc_p2pmem|virt_to_bus|supports_p2pdma|rv=' "$TR/trace" 2>/dev/null | head -40
    echo "FTRACE: --- (full trace tail, fallback) ---"
    tail -40 "$TR/trace" 2>/dev/null
fi

# ---- 5. 间接证据（无论 kprobe 成败都收集）----
echo "FTRACE: ===== indirect evidence ====="
# nvme PCI 设备的 BDF（vfio-user-pci 通常 00:0X.0）
for d in /sys/bus/pci/devices/*/; do
    if [ -e "$d/class" ]; then
        cls=$(cat "$d/class" 2>/dev/null)
        # NVMe class = 0x010802
        case "$cls" in
            0x010802*)
                bdf=$(basename "$d")
                echo "FTRACE: NVMe PCI dev = $bdf class=$cls"
                echo "FTRACE:   resource(BARs):"; cat "$d/resource" 2>/dev/null | head -7
                if [ -d "$d/p2pmem" ]; then
                    echo "FTRACE:   p2pmem/ EXISTS:"
                    for f in size available published; do
                        [ -e "$d/p2pmem/$f" ] && echo "FTRACE:     p2pmem/$f = $(cat "$d/p2pmem/$f" 2>/dev/null)"
                    done
                else
                    echo "FTRACE:   p2pmem/ ABSENT (pci_p2pdma_add_resource 未注册或池未发布)"
                fi
                # nvme sysfs cmb 属性
                for nd in "$d"nvme/nvme*; do
                    [ -d "$nd" ] || continue
                    echo "FTRACE:   $(basename "$nd") cmb=$(cat "$nd/cmb" 2>/dev/null)"
                done
                ;;
        esac
    fi
done
echo "FTRACE: ===== dmesg p2p/cmb/nvme lines ====="
dmesg 2>/dev/null | grep -iE 'p2p|peer-to-peer|cmb|nvme' | tail -25

echo "FTRACE: ===== done ====="
poweroff -f
