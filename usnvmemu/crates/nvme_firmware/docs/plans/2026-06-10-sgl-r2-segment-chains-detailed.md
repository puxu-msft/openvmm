# Plan — SGL R2：Segment chains + Bit Bucket（完整 SGL 数据路径）

> 状态：✅ **DONE（2026-06-10）**。R2a/b/c/d 全段完成并各过 rust-reviewer + revert-verify。
> commits：R2a 83b0cdf4 / R2b 059de7cd / R2c 9240ff87 / R2d 2babf32a。100 lib + 16 e2e
> pass，clippy 0 warning。SGLS bit16 已回填，advertise⟺implement 重新对齐。
> **R2d 附带收获**：anchored 测试（sc:: ↔ nvme_spec::Status）暴露并校正了 R1 把 SGL SC
> 全手填错（0x14-0x17 实为别的码）+ SANITIZE_IN_PROGRESS=0x12 的 live wire bug。
>
> 原始 scope（已全部落地）：
> 类型：niche（OpenHCL + fabric 两接入都走 PRP 进 controller，SGL 极少被驱动）但真
> spec completion；silent-corruption-class（数据路径）—— 须完整严谨循环。

## What（缺口）

`io.rs::resolve_data_pointers`（PSDT 分流）现状：
- PSDT=00：PRP（完整）。
- PSDT=01：inline 单 SGL Data Block sub_type=0 ≤1page → 映射成 `(prp1=address, prp2=0)`
  复用 PRP 路径（R1，done）。Bit Bucket / Segment / Keyed / >1page → `SGL_DESCRIPTOR_TYPE_INVALID`。
- PSDT=10（SGL Segment pointer）→ `SGL_DESCRIPTOR_TYPE_INVALID`（**R2 缺口**）。

R2 = 支持 PSDT=10 的 segment chain + Bit Bucket：descriptor 指向 host 内存里一段
descriptor 数组（Segment / Last Segment 链式），数据是任意 (address, length) fragment
的 scatter-gather——**无法**映射成单个 `(prp1, prp2)`，需平行数据路径。

## 复用 / 现成件

- `sgl.rs::SglDescriptor::parse` + `parse_sgl_list`（解析 16-byte descriptor，done）。
- `sgl.rs::flatten_data_blocks` + `SglFragment{Data, BitBucket}`（**dead-code**，正为 R2 写的）：
  把 descriptor 展平成 (address, length) 片段，Bit Bucket 标记跳过。
- 镜像 `prp_list_ops` 机件做 `sgl_ops`：异步 DMA-read segment 页 → parse → 递归下一段
  （Last Segment 终止）→ 展平 fragment plan → 逐 fragment DMA（Read=写 host / Write=读 host），
  Bit Bucket fragment（Read 跳过即 host 留旧值 / Write 不读）→ 全到齐 post CQE。

## 子阶段

- **R2a**：PSDT=10 单 Segment（不链）+ 多 Data Block fragment 的 Read/Write 传输路径
  （新 `sgl_ops` + `NvmSgl{Fetch,Data}` PendingOp + completion arm）。
- **R2b**：Segment chain（Last Segment 链式递归 fetch）。
- **R2c**：Bit Bucket（Read 跳过区段、Write 丢弃区段）。
- **R2d**：SGLS 加回 bit16(Bit Bucket)；length-mismatch（SGL length 与 NLB*sector 不符）
  的 SC（`SGL_DATA_BLOCK_GRANULARITY_INVALID` / `DATA_SGL_LENGTH_INVALID`）。

## 测试（用 OpenHCL harness 驱 SGL command）

harness 现只发 PRP（cdw0 PSDT=00）。R2 测试需构造 PSDT=10 SQE + 在 guest-mem 放 SGL
segment 页（descriptor 指向非连续 fragment）。oracle：fragment 各按 (address,length)
落地（非连续 + 跨 fragment）+ Bit Bucket 区被跳过；**revert-verify** 注入单-fragment
假设确认多-fragment 测试 FAIL。+ 单元（`CaptureTransport` 验 fragment DMA 偏移）。

## 风险

- 平行数据路径（SGL fragment vs PRP page）—— 不要污染 PRP 路径；抽公共"transfer plan"
  抽象或显式分流。
- 任意 length/alignment：fragment 可非页对齐、跨页 → DMA 切分须按 fragment 而非 page。
- 与 PI / fused / 4K 的交互：SGL × 这些组合的 sector 感知。

## 起手提示

read 本 plan + `sgl.rs`（flatten_data_blocks 等现成件）+ `io.rs::resolve_data_pointers`
+ PRP-list 路径（`prp_list_ops` + `NvmReadPrpList*`，作镜像参考）。每子阶段 revert-verify
+ rust-reviewer（数据路径）。补完 R2d 后改 `cmd.rs` SGLS 加回 bit16 + 更新一致性测试。
