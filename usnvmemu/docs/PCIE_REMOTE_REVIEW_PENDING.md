# 待用户决定的文档改进建议（已记录但不自动应用）

> 由 doc-updater subagent (2026-05-30) 提出 LOW severity。
> 主 agent 评估为"价值低 / 可保留 / 不破坏功能"，**有意识地未应用**，
> 不是遗漏。用户随时可来决定改不改。
>
> 用户原则："对于低优先级建议，可以记录，留待用户决定；但不要为低优先级
> 建议等待用户决定；不阻止你自动应用，只是当你认为低优先级建议有价值但
> 不打算做的时候，需要记录而不是丢弃。"

## 当前记录项

### L-1 — `create_and_boot_openhcl.ps1` 用 `$args` 覆盖 PowerShell 自动变量

- 文件：`docs/superpowers/scripts/hyperv/create_and_boot_openhcl.ps1:37-50`
- 现状：`$args = @{...}` splat hashtable。`$args` 是 PowerShell 自动变量
- 主 agent 决策：**不修**。脚本已 deprecated（顶部有 `⚠ DEPRECATED` header
  指向 `create_openhcl_vm_correct.ps1`），不会被推荐使用；改它的好处和"扰动
  历史脚本"的风险不平衡
- 用户若要修：把所有 `$args` 改为 `$vmArgs`，重新跑一次 deprecated 脚本验证

### L-4 — 中英文括号混排风格

- 文件：`docs/superpowers/PCIE_REMOTE_HYPERV_RUNBOOK.md` 多处
- 现状：中文文本里圆括号包英文，如 `期望（实测真 Hyper-V 输出）：`
- 主 agent 决策：**不修**。是项目整体风格，docs/ 所有中文文档一致；统一改
  会触及很多文件且无功能意义
- 用户若要修：搜 `（` 全文统一为半角 `(` + 空格

### L-6 — `enable_vmbus_redirect.ps1` deprecation header 措辞

- 文件：`docs/superpowers/scripts/hyperv/enable_vmbus_redirect.ps1:1-14`
- 现状：注释说"保留作 ModifySystemSettings + CimSerializer + 异步 Job
  等待的代码范例参考"
- 主 agent 决策：**不修**。subagent 自己也建议保留；当前措辞读得通

### L-8 — USER_TODO §C "Service GUID 注册" 是历史段

- 文件：`docs/superpowers/PCIE_REMOTE_USER_TODO_LEGACY.md:78-83`
- 现状：§C 是"一次性已完成"参考段（onboarding 时回查用）
- 主 agent 决策：**不修**。删了反而损失了 onboarding 路径文档；H-2 已把
  脚本路径修对了，内容上完整

## 已应用项（记录用，下次 review 时回查）

- L-2 / L-3 — commit hash 引用失效 → SESSION_LOG.md 改为"早期 commit；
  squash 后 hash 已变"措辞（commit 8f3f8201）
- L-5 — HYPERV_RUNBOOK.md "跨编自" 注释精确化为 "vsock_main.rs 跨编自"
  （commit 8f3f8201）
- L-7 — spec §3.4 `DmaCompletion.data` 加 inline `// data ≤ 64 KiB
  (MAX_DMA_BYTES)` 注释（`WriteGpaRequest.data` 原本就有，subagent 误判
  说没有；顺手把 DmaCompletion 也加上）

## 状态

| 编号 | 状态 |
|---|---|
| L-1 | 记录（不修） |
| L-4 | 记录（不修） |
| L-6 | 记录（不修） |
| L-8 | 记录（不修） |
| L-2 / L-3 / L-5 / L-7 | 已应用 |
