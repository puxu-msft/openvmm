// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **CMB-P1b** — CMB 数据路径：CMB-vs-DMA dispatch helper + 本地合成-completion
//! 队列 + tail-drain（非重入）。
//!
//! 设计来源：`docs/plans/2026-06-12-cmb-dual-mode-design.md` §4 io.rs 集成 + §10#2
//! （architect 复核必改第 2 条）。要点：
//!
//! - CMB 命中的 guest 内存访问是**同步**内存读写（直接 `backing` 切片），但本项目的
//!   IO 状态机（`PendingOp` / `on_dma_complete_impl`）是纯 token 模型。故 CMB 命中
//!   退化为 vfio transport "同步合成 completion → 栈外投递" 的镜像：`guest_read`/
//!   `guest_write` 同步访问 backing、合成一条 [`CmbCompletion`] **入队**、同步返回
//!   token；**绝不**在 `access_guest` 调用栈内递归调 `on_dma_complete*`。
//!
//! - 真正喂 `on_dma_complete_impl` 的 [`drain_cmb_completions`] 只在**顶层调用栈尾**
//!   （`mmio_write` / `tick` / `on_dma_complete` 主体工作返回前）跑。drain 一条可能
//!   因其处理路径再发起 CMB 访问而再入队（cascade）→ 循环直到队列空；`cmb_draining`
//!   哨兵 + `MAX_CMB_DRAIN_ITERS` 深度上限防失控（对齐 `MAX_SHADOW_POLL_ITERS` 风格）。
//!
//! - token 命名空间：CMB token = [`CMB_TOKEN_TAG`]`| n`，tag 在 bit 63。**承重前提
//!   （reviewer MEDIUM-1，非普适不变量）**：bit-63 隔离仅对 vfio（低 16 位 msg_id）与
//!   pcie_remote/CaptureTransport（`1<<40` 起小步增）成立——它们的真 token 远低于 bit 63。
//!   **NVMe-oF TCP transport 不成立**：其 token slab 从 `1<<48` 起、每 conn 抬 `1<<40`
//!   （`nvme_of_tcp_target` TOKEN_SLAB_START/SIZE），足够多 conn 后会置 bit 63 与 CMB
//!   token 撞名。**之所以安全 = CMB 是 PCIe-only（设计 §6：TCP 上 CMBLOC/CMBSZ 返 0），
//!   故 CMB token 与 TCP token 结构性永不在同一 controller 实例共存**。⚠️ 若将来让某
//!   controller 同时 CMB + fabric，必须改用显式 `TokenKind` 路由替代 bit-63 tag。

use super::CmbCompletion;
use super::NvmeController;
use pcie_device_core::DeviceCtx;

/// **CMB-P1b** — CMB 命中判定结果。
enum CmbHit {
    /// 整段 `[gpa, gpa+len)` **完全落在** CMB 内 → 偏移（相对 backing 起点）。
    Contained(usize),
    /// 起点或区段与 CMB **部分重叠**但未完全包含（跨 CMB/非-CMB 边界）→ 非法访问，
    /// 合成 `ok=false`（**不**回退 DMA：straddle 是 driver bug，拆两路会让 CMB 段
    /// 落错 backing → 静默数据损坏）。
    Straddle,
    /// 与 CMB **完全不相交** → 走常规 DMA。
    Miss,
}

impl NvmeController {
    /// **CMB-P2** — 当前 CMB 窗口 `(cba, size)`，仅在 CMB **已启用**（CMBMSC.CMSE=1）
    /// 时为 `Some`；否则 `None`（无 CMB / 仅 CRE / 未编程）。SGL 三路径用它统一判
    /// CMB-relative 是否放行（`subtype_to_sc(_, cmb.is_some())`）+ rebase 偏移
    /// （`resolve_sgl_address(_, _, cmb)`）。语义与 `cmb_hit` 的"CMSE 未置 → 不拦截"
    /// 一致（同一启用判据，避免 classifier 放行了而 dispatch 又当 Miss 的不一致）。
    pub(super) fn cmb_window(&self) -> Option<(u64, u64)> {
        let cmb = self.cmb.as_ref()?;
        if cmb.cmse {
            Some((cmb.cba, cmb.size))
        } else {
            None
        }
    }

    /// **CMB-P1b** — CMB 合成 token 的高位 tag（bit 63）。见模块文档命名空间说明。
    pub(super) const CMB_TOKEN_TAG: u64 = 1u64 << 63;

    /// **CMB-P1b** — `drain_cmb_completions` 单次顶层调用的最大投递条数上限。撞顶
    /// （cascade 失控）即停止 drain 并置 CSTS.CFS，与 `MAX_SHADOW_POLL_ITERS` 同纪律
    /// （有限终止优先于隐性僵死）。正常 IO 远不会接近此值（一次 doorbell 派生的 CMB
    /// 访问链是有界的）。
    const MAX_CMB_DRAIN_ITERS: u32 = 1 << 20;

    /// **CMB-P1b** — 分配一个 CMB 合成 token（带 [`Self::CMB_TOKEN_TAG`]）。
    fn alloc_cmb_token(&mut self) -> u64 {
        // 低 63 位单调递增；与 tag 或运算。wrap 回 0 不影响正确性（同一时刻 in-flight
        // 的 CMB token 数远小于 2^63；与真 DMA token 因 tag 隔离不冲突）。
        let n = self.cmb_next_token & !Self::CMB_TOKEN_TAG;
        self.cmb_next_token = n.wrapping_add(1);
        Self::CMB_TOKEN_TAG | n
    }

    /// **CMB-P1b** — 判断 `[gpa, gpa+len)` 相对**已启用（CMSE=1）** 的 CMB 窗口
    /// `[cba, cba+size)` 的关系。要求**整段完全落在 CMB 内**才算命中（[`CmbHit::Contained`]）；
    /// 任意方向的部分重叠（起点在内越尾 / 起点在外伸入）都判 [`CmbHit::Straddle`]
    /// （对称处理，避免 reviewer H2 的"起点在外伸入 CMB"被误当 Miss 走 DMA 而损坏
    /// CMB 段）。窗口上界用 `checked_add` 防恶意 CBA 溢出 panic（reviewer M1）。
    fn cmb_hit(&self, gpa: u64, len: u32) -> CmbHit {
        let Some(cmb) = self.cmb.as_ref() else {
            return CmbHit::Miss;
        };
        if !cmb.cmse {
            return CmbHit::Miss;
        }
        // 窗口上界溢出 → 视为非命中（恶意 CBA；正常 4 KiB 对齐 CBA + 有界 size 不溢出）。
        let Some(win_end) = cmb.cba.checked_add(cmb.size) else {
            tracing::warn!(
                cba = format_args!("{:#x}", cmb.cba),
                size = cmb.size,
                "CMB 窗口上界 u64 溢出（恶意/非法 CBA）→ 视为非 CMB"
            );
            return CmbHit::Miss;
        };
        // 访问段上界：`gpa` 完整 u64 且来自 driver 控制的 PRP/SGL 段地址，无上游 bound，
        // 故 `gpa + len` 可溢出 → 必须 `checked_add`（reviewer HIGH-1：裸 `+` 在 debug
        // 构建对恶意 gpa 近 u64::MAX panic）。溢出 → 视为非 CMB（gpa 近 u64::MAX 不可能
        // 落在合法 4 KiB 对齐 CMB 窗口内）。len==0 也判 Miss（退化访问不走 CMB；现 rewired
        // 调用点本就在 remaining==0 早退、Boot Read 拒 len==0，此为防御性，reviewer LOW-1）。
        if len == 0 {
            return CmbHit::Miss;
        }
        let Some(acc_end) = gpa.checked_add(len as u64) else {
            return CmbHit::Miss;
        };
        let starts_in = gpa >= cmb.cba && gpa < win_end;
        let ends_in = acc_end > cmb.cba && acc_end <= win_end;
        match (starts_in, ends_in) {
            (true, true) => CmbHit::Contained((gpa - cmb.cba) as usize),
            (false, false) => {
                // 起点、终点都在窗口外。仍可能整段**横跨**窗口（gpa < cba 且 acc_end > win_end）。
                if gpa < cmb.cba && acc_end > win_end {
                    CmbHit::Straddle
                } else {
                    CmbHit::Miss
                }
            }
            // 仅一端在内 → 部分重叠。
            _ => CmbHit::Straddle,
        }
    }

    /// **CMB-P1b** — guest 内存**读** dispatch：gpa 命中 CMB → 同步读 backing +
    /// 合成 completion 入队 + 返回 CMB token（**不**调 `on_dma_complete*`）；否则走
    /// 常规 `ctx.dma_read`。返回的 token 语义与 `ctx.dma_read` 完全一致（caller 照常
    /// 存进 pending 表，由 drain/transport 完成回调推进状态机）。
    pub(super) fn guest_read(&mut self, ctx: &mut DeviceCtx<'_>, gpa: u64, len: u32) -> u64 {
        let off = match self.cmb_hit(gpa, len) {
            CmbHit::Miss => return ctx.dma_read(gpa, len),
            CmbHit::Contained(off) => Some(off),
            CmbHit::Straddle => None,
        };
        let token = self.alloc_cmb_token();
        // **非重入哨兵（§10#2 机器校验）** — backing 访问 + 入队期间置位；
        // `on_dma_complete_impl` 入口 debug_assert 它为 false。本块**绝不**调
        // completion 派发，故置位区间内无 completion 重入。
        self.cmb_in_access_guest = true;
        let cmb = self.cmb.as_ref().expect("cmb_hit 命中 → cmb 存在");
        let (ok, data) = match off {
            // cmb_hit 已保证 Contained 整段在 backing 内（off+len ≤ size == backing.len()）。
            Some(off) => (
                true,
                Some(cmb.backing.as_bytes()[off..off + len as usize].to_vec()),
            ),
            None => {
                tracing::warn!(
                    gpa = format_args!("{:#x}", gpa),
                    len,
                    cmb_len = cmb.size,
                    "CMB read 跨 CMB/非-CMB 边界（straddle）→ 合成 ok=false"
                );
                (false, None)
            }
        };
        self.cmb_completions
            .push_back(CmbCompletion { token, ok, data });
        self.cmb_in_access_guest = false;
        token
    }

    /// **CMB-P1b** — guest 内存**写** dispatch：gpa 命中 CMB → 同步写 backing +
    /// 合成 completion（write 的 `data=None`，对齐 `on_dma_complete` 契约）入队 +
    /// 返回 CMB token；否则走常规 `ctx.dma_write`。
    pub(super) fn guest_write(&mut self, ctx: &mut DeviceCtx<'_>, gpa: u64, data: Vec<u8>) -> u64 {
        let off = match self.cmb_hit(gpa, data.len() as u32) {
            CmbHit::Miss => return ctx.dma_write(gpa, data),
            CmbHit::Contained(off) => Some(off),
            CmbHit::Straddle => None,
        };
        let token = self.alloc_cmb_token();
        // **非重入哨兵（§10#2 机器校验）** — 同 guest_read。
        self.cmb_in_access_guest = true;
        let cmb = self.cmb.as_mut().expect("cmb_hit 命中 → cmb 存在");
        let ok = match off {
            Some(off) => {
                // cmb_hit 已保证整段在 backing 内。
                cmb.backing.as_bytes_mut()[off..off + data.len()].copy_from_slice(&data);
                true
            }
            None => {
                tracing::warn!(
                    gpa = format_args!("{:#x}", gpa),
                    len = data.len(),
                    cmb_len = cmb.size,
                    "CMB write 跨 CMB/非-CMB 边界（straddle）→ 合成 ok=false（未写入）"
                );
                false
            }
        };
        self.cmb_completions.push_back(CmbCompletion {
            token,
            ok,
            data: None,
        });
        self.cmb_in_access_guest = false;
        token
    }

    /// **CMB-P1b** — 顶层调用栈尾的 tail-drain：把 `cmb_completions` 里的合成完成
    /// 逐条喂给 `on_dma_complete_impl`，驱动 `PendingOp` 状态机。**必须只在顶层入口
    /// （`mmio_write`/`tick`/`on_dma_complete`）的收尾处调**——不在 `access_guest`/
    /// `guest_*` 调用栈内（设计 §10#2 铁律）。
    ///
    /// drain 一条的处理路径（`on_dma_complete_impl` → io.rs）可能再发起 CMB 访问 →
    /// 经 `guest_*` 再入队（cascade）；本循环持续消费直到队列空。`cmb_draining` 哨兵
    /// 保证顶层入口在 drain 已在跑时**不重入**起第二个 drain 循环（cascade 入队的新
    /// 条目由当前循环消费）。`MAX_CMB_DRAIN_ITERS` 防 cascade 失控（→ CSTS.CFS）。
    pub(super) fn drain_cmb_completions(&mut self, ctx: &mut DeviceCtx<'_>) {
        // 重入哨兵：若已在 drain（理论上不该发生，因 drain 只在顶层栈尾调，而 guest_*
        // 只入队不 drain），直接返回让外层循环继续消费。
        if self.cmb_draining {
            return;
        }
        self.cmb_draining = true;
        let mut iters: u32 = 0;
        while let Some(comp) = self.cmb_completions.pop_front() {
            iters += 1;
            if iters >= Self::MAX_CMB_DRAIN_ITERS {
                tracing::error!(
                    iters,
                    "CMB drain 自续深度撞上限（cascade 失控）→ 置 CSTS.CFS + 收链"
                );
                self.csts |= crate::regs::csts::CFS;
                self.cmb_completions.clear();
                break;
            }
            // 投递一条合成完成；其处理路径可能经 guest_* 再入队 → 下轮循环消费。
            self.on_dma_complete_impl(ctx, comp.token, comp.ok, comp.data.unwrap_or_default());
        }
        self.cmb_draining = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CMB token tag 高于 **vfio + pcie_remote** 的真 token 范围（bit-63 隔离对这两条
    /// transport 成立）。**不覆盖 NVMe-oF TCP**（其 token 会爬到高位，见模块文档承重前提：
    /// 安全靠 CMB 与 fabric 结构性互斥，而非 tag 隔离）。用运行期变量绕开 clippy
    /// `assertions_on_constants`。
    #[test]
    fn cmb_token_tag_does_not_overlap_transport_ranges() {
        let tag = std::hint::black_box(NvmeController::CMB_TOKEN_TAG);
        // vfio: 低 16 位（≤ 0xFFFF）；pcie_remote / CaptureTransport: 1<<40 起。
        let vfio_max = std::hint::black_box(0xFFFFu64);
        let pcie_start = std::hint::black_box(1u64 << 40);
        assert_eq!(tag, 1u64 << 63);
        assert!(tag > pcie_start, "高于 pcie_remote 起点");
        assert!(tag > vfio_max, "高于 vfio msg_id 范围");
    }
}
