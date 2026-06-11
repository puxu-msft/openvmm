# POC：非-QEMU vfio-user client 的 fd-passing 零拷贝 DMA

**日期**：2026-06-11
**状态**：实验（feasibility POC，非生产代码）
**关联**：`vfio_user_device`（OpenHCL VTL2 vfio-user client）Spec A / Spec B 的承重假设验证

## POC 矩阵（本目录三个 POC）

| POC | 文件 | 验的承重假设 | 环境 | 结果 |
|-----|------|------------|------|------|
| **1** | `poc.py` + `RESULT.md` | 非-QEMU vfio-user client 的 fd-passing DMA_MAP + server mmap **零拷贝 DMA** 端到端 | 本地 WSL2 | **✅ PASSED** |
| **3** | `poc_mshv_probe.py` | OpenHCL VTL2 sidecar 经 `/dev/mshv_vtl_low` 拿**真 guest RAM** fd + 自映射 | 需真 OpenHCL VTL2 | ⏳ **ENV-GATED**（本地确认设备缺席，符合预期；探针就绪，待真环境跑） |
| **4** | `poc_convert_revoke.py` | （Spec B / CVM）owner 无法单方面撤销 sidecar 的共享映射 ⟹ 必须 revoke-before-convert | 本地（模型） | **✅ PASSED**（实测复现审计 CRITICAL 根因） |

**净结论**：Spec A 客户端零拷贝路径**已本地证实可行**（POC-1）；真 guest RAM fd 导出
是 env-gated 研究门（POC-3，探针就绪）；Spec B 的 CVM 难点**已用模型证明审计判断正确**
（POC-4，naive 直访不安全、需显式撤销协议）。

---

## 为什么做这个 POC

设计"OpenHCL 作为 vfio-user client 直访 guest 内存零拷贝"（Spec A，非-CVM）依赖一条
承重假设：

> **我们自己写的（非 QEMU）vfio-user client，能否经 AF_UNIX + SCM_RIGHTS 把一段
> 'guest RAM' 的 fd 交给 firmware server，让 server 对它 mmap 零拷贝 DMA，端到端跑通？**

机制的 server 侧已被真 QEMU 11 e2e 证过（QEMU 传 guest RAM memfd，firmware mmap，真
guest IO 通过 —— 见 `nvme-of`/`vfio-user-real-guest-io-milestone`）。但**我们自己的
client 侧从未证过**——现有 `scripts/interop_py/enumerate_smoke.py` 只做枚举/cfg-space，
不碰 DMA_MAP-with-fd。`vfio_user_device` 的核心正是这个 client。

POC-first（用户 2026-06-11 指示：未验证承重假设先做 POC 再定方案）：在**完全本地**
（Linux/WSL2，无需真 Hyper-V / 无需 OpenHCL build）先把这条 client 侧零拷贝路径跑通，
拿硬数据再敲定 Spec A。

## 方法

`poc.py`（Python stdlib，复用 interop_py 的 `vfio_proto` 基础 wire）：

1. `memfd_create` 一段 256 KiB 当 "guest RAM"，client 自己也 mmap 它。
2. spawn `nvme_firmware --vfio-user-sock`（现有 server，零改动），AF_UNIX 连入。
3. VERSION 握手 + GET_INFO。
4. **DMA_MAP**：把整段 memfd 作为 guest RAM region [addr=0, size=256K]，
   READABLE|WRITEABLE，**经 `sendmsg` SCM_RIGHTS 把 memfd 传给 server** → server
   `DmaBacking::Mmap` 零拷贝映射。
5. NVMe admin bring-up（REGION_WRITE BAR0）：AQA / ASQ=0x1000 / ACQ=0x2000 / CC.EN=1，
   poll CSTS.RDY。
6. 在 client 的 memfd mmap 里写一条 **Identify Controller** SQE（PRP1=0x3000），
   ring SQ0 tail doorbell。
7. server 零拷贝 dma_read SQE（memfd@0x1000）→ 处理 Identify → 零拷贝 dma_write
   4KiB 结果到 PRP1（memfd@0x3000）+ CQE 到 ACQ（memfd@0x2000）。
8. client 读**自己的 memfd mmap**：验 0x3000 处 Identify 的 MN 字符串 + 0x2000 处
   CQE phase/status。

## 零拷贝的自证（关键）

POC **故意不服务任何 server-initiated DMA_READ/WRITE 命令**。若 server 走的是
message-mediated（非 mmap），它会发 DMA_READ 来取 SQE 并**阻塞等我们的 reply**——
我们不回，Identify 永不完成、POC 超时失败。**因此：Identify 成功完成 ⟺ server 走了
mmap 零拷贝**（数据直接落进我们传去的 memfd）。这是一条无需读 server 日志的结构性证明。

## 跑

```bash
cd usnvmemu/experiments/2026-06-11-vfio-client-zerocopy-dma
python3 poc.py
# 退出码 0 = 零拷贝 DMA 端到端可行（含 MN 校验 + CQE 校验 + 零拷贝自证）
```

需要：`nvme_firmware` 能 `cargo build --features vfio-user`（poc.py 自动 build+spawn）。

## 结果

见本目录 `RESULT.md`（POC 跑完后写入）。

## 边界（这个 POC 证什么、不证什么）

- ✅ 证：非-QEMU client 的 fd-passing DMA_MAP + server mmap 零拷贝 DMA 端到端可行。
- ✅ 证：`vfio_proto` wire + SCM_RIGHTS fd 传递在 Python client 侧работает。
- ❌ 不证：OpenHCL VTL2 同内核 sidecar + AF_UNIX 可用性（需真 OpenHCL/Hyper-V，Spec A 的
  另一条承重假设，留作需特殊环境的 POC）。
- ❌ 不证：从 `guestmem::GuestMemorySharing` / `/dev/mshv_vtl_low` 拿到 *真 guest RAM* 的
  fd（这里用 memfd 模拟 guest RAM；真 OpenHCL 侧导出 fd 是另一条待验路径）。
- ❌ 不证：CVM（page-convert / private 内存）——那是 Spec B，本 POC 明确非-CVM。
