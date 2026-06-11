# W5a 档2 — firmware-in-VTL2 真机部署 + DMA 零拷贝真 NVMe IO

**日期**：2026-06-12  **VM**：pcie-remote-exp（真 OpenHCL，HCL kernel 6.18）

## 目的
firmware-as-VTL2-进程 dev 部署（spec §6 W5a）+ POC-1/3/6 真机汇合：真 `nvme_firmware`
ELF 在真 VTL2 跑 AF_UNIX server，test client（真 `vfio_user_device` 客户端 API）
经 DMA_MAP 把**真 guest RAM**(/dev/mshv_vtl_low) fd 传给 firmware，驱 NVMe Identify，
firmware **零拷贝 DMA 进真 guest 物理内存**，client 读回 MN 验证。

## 跑
```bash
./build_and_stage.sh        # cross-build firmware+client 静态 musl → /tmp/w5a_stage
./run_e2e.sh                # tar→base64→ohcldiag-dev run 推进 VTL2 + 起 fw + 跑 client
```
预期末行：`W5A PASSED ✓ — firmware-in-VTL2 经 DMA_MAP 真 guest RAM 零拷贝 Identify 端到端`

## scope（用户 2026-06-12 拍板档2）
- W5a（本 harness）= dev 部署 + 真 guest RAM 零拷贝真 NVMe IO。
- **W5b（underhill Command::new supervisor + reconnect + 降权）≡ W6**，推迟（需改 underhill_core，standalone 测不了，spec 自承 W5b 起依赖 W6）。
- **W5c（IGVM initrd 出厂 / A-B image）** 远期。

## 真机修复（real-VM surfaced 的 W3 真 bug）
W5a 真机 e2e 首次暴露：`vfio_user_transport::dma::map_dma_fd` 的 fstat `st_size` 上界
（防 SIGBUS，对 memfd 有意义）会**误拒字符设备** `/dev/mshv_vtl_low`（st_size=0）→
零拷贝失效 → 退回 message 路径 → client 无 server-initiated DMA pump → 挂。
**W3 只用 memfd（有 st_size）测，漏了真目标（mshv_vtl_low 字符设备）。** 修：仅对
普通文件(S_IFREG)施 st_size 上界；字符/块设备跳过（mmap 有效性由驱动 GPA-range
mmap handler 定，POC-6 证裸 mmap @ file_offset=GPA 工作）。fw.log 验 `zero_copy=true`。

## guest RAM 安全
test client 在真 guest 物理内存 [GPA_BASE, +64KiB) 布 NVMe 队列（默认 GPA_BASE=0x100000，
POC-3/6 真机在此写 marker 存活过；env `W5A_GPA_BASE` 可调）。写活 guest RAM 有干扰风险——
失稳 = 真发现，指向 W6 需 underhill GuestMemory mediation（underhill 知 guest RAM 布局）。
本次真机跑：guest 无异常，VM 全程 Running。

## 真机结果（2026-06-12，pcie-remote-exp）
```
[w5a-client] handshake ok: server 0.1
[w5a-client] num_regions=9 num_irqs=5
[w5a-client] dma_map [0x100000, +0x10000) ok
[w5a-client] controller enabled (CSTS.RDY)
[w5a-client] Identify MN = "OpenHCL Userspace NVMe v2.0"
[w5a-client] W5A PASSED ✓ — firmware-in-VTL2 经 DMA_MAP 真 guest RAM 零拷贝 Identify 端到端
```
fw.log 关键证据（零拷贝命中真 guest RAM，wire 上无 server-initiated DMA_READ）：
- `DMA_MAP added addr=0x100000 size=65536 readable=true writeable=true zero_copy=true`（Stage 0 修复生效，字符设备 fd mmap 成功）
- `VfioUserSession.dma_read OK; enqueue on_dma_complete token=32768 gpa=0x100000 len=64`（SQE fetch 零拷贝读真 guest RAM）
- `dispatch SQE cid=1 opc=0x6` → `Identify cns=1` → `dma_write OK gpa=0x102000 bytes=4096`（Identify 数据零拷贝写真 guest RAM）→ `post CQE gpa=0x101000`
- `fire_interrupt: 向量未配置 eventfd`（预期 WARN：本 harness 未 set_irqs，client 轮询 CQE 而非等 MSI-X；无害）

**结论**：firmware-in-VTL2（topology A）+ vfio_user_device 客户端 API（W1-W4）+ DMA_MAP
真 guest RAM 零拷贝（W3 + 本 Stage 0 字符设备修复）在真 OpenHCL VM 首次端到端汇合。

## W6a async 迁移注记（2026-06-12）

`vfio_user_device` client 在 W6a 迁到 async（pal_async PolledSocket + SCM_RIGHTS）。本
harness client 已同步更新为 async（`DefaultPool::run_with` 包裹 + `.await`），**host 与
x86_64-unknown-linux-musl 静态构建均通过**（pal_async 在 musl 上 build clean）。

**但上面记录的真机 e2e PASS 结果是 W5a（commit `6a633c4b`，pre-async）的**；async 版
harness 仅验证了编译（host + musl），真 VM re-verify 随 W6b 真 underhill 集成一并重跑
（W6b 会以 pcie_remote 风格 async worker 驱动 client，是更有代表性的真机验证点）。wire
行为与 pre-async 一致（同 `vfio_user_wire` 编解码 + 同 SCM_RIGHTS 字节），async 只改收发
调度，不改协议字节。
