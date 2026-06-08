# pcie-remote-phase — 决策日志 (路径 / 早期架构 ADR)

> OpenHCL pcie_remote 路径相关的早期 Architecture Decision Records。
> 跨切面 / 架构级决策见 [项目 DECISIONS.md](/usnvmemu/docs/DECISIONS.md)
> （含全 ADR 索引表）。

> 注：Phase W 拆出 `pcie_transport_openhcl` crate 后，本 ADR 宜随之迁入该 crate 的
> `docs/DECISIONS.md`（见 [项目 ADR-011](/usnvmemu/docs/DECISIONS.md)）。

---

## ADR-001 — PCIe Remote Path C (OpenHCL VTL2 + vsock) (2026-05-29..30)

**Context**：早期纠结 Path A (OpenVMM Linux) / Path B (mshv) / Path C
(OpenHCL VTL2)。

**Decision**：Path C，理由见 [SESSION_LOG.md](/usnvmemu/docs/pcie-remote-phase/SESSION_LOG.md) 早期段。

**Status**：active；Path A 仍可跑，Path B (mshv)
[MSHV_DIAGNOSIS.md](/usnvmemu/docs/pcie-remote-phase/MSHV_DIAGNOSIS.md) 搁置。
