# RESULT — POC-1：非-QEMU vfio-user client 零拷贝 DMA

**跑期**：2026-06-11，本地 WSL2（无 Hyper-V）
**命令**：`python3 poc.py` → **退出码 0，PASSED**

## 实测输出（关键行）

```
VERSION handshake OK
✓ GET_INFO num_regions=9
DMA_MAP with memfd OK (server should mmap zero-copy)
wrote AQA/ASQ/ACQ/CC.EN
✓ CSTS.RDY=1 (controller enabled)
placed Identify SQE at ASQ[0] (cid=0x42, PRP1=0x3000)
rang SQ0 tail doorbell = 1
✓ CQE 出现在我们的 memfd（server 零拷贝 dma_write 写回）
✓ CQE CID = 0x42（匹配我们提交的命令）
✓ CQE status = 0 (success)
Identify MN field = 'OpenHCL Userspace NVMe v2.0'
✓ Identify MN 含预期厂商串（零拷贝 DMA 写回）
零拷贝自证成立：全程未服务任何 DMA_READ/WRITE，Identify 仍完成 ⟹ server 走 mmap
```

## 证明了什么

1. **我们自己写的（非 QEMU）vfio-user client 能驱动 fd-passing DMA_MAP**：Python
   `sendmsg` + `SCM_RIGHTS` 把 memfd 交给 firmware server，server 接受并 mmap。
2. **server 对 client 传去的 fd 做零拷贝双向 DMA**：
   - dma_read：从 memfd@ASQ[0] 零拷贝取出我们写的 Identify SQE（否则 CQE 不会带我们的 cid=0x42）；
   - dma_write：把 4 KiB Identify 结果 + 16B CQE 零拷贝写回 memfd@PRP1 / @ACQ，我们在
     **自己的 mmap** 里读到了 MN 串与 CQE。
3. **零拷贝（mmap）而非 message-mediated**：POC 全程不响应任何 server-initiated
   DMA_READ/WRITE；若 server 走消息路径它会阻塞等 reply、Identify 永不完成。Identify
   成功 ⟹ 数据直接经 mmap 落进 memfd。结构性证明，无需读 server 日志。

## 对 Spec A 的意义

Spec A 的承重假设之一——"非-QEMU vfio-user client + fd-passing + server mmap 零拷贝
DMA 端到端可行"——**已用硬数据验证为真**。`vfio_user_device` 的 client 侧 DMA 路径
不再是假设。

剩余两条承重假设（见 README 边界）属**需特殊环境**，POC 见同目录 `poc_mshv_probe.py`
（OpenHCL VTL2 真 guest RAM fd 导出）与 `poc_convert_revoke.py`（CVM page-convert
撤销不变量，Spec B）。
