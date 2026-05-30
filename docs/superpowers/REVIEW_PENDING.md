# 待用户决定的低优先级文档改进建议

> 由 doc-updater subagent (2026-05-30) 提出，主 agent 评估为 LOW severity，
> **未自动应用**，留待用户在 review 时决定。
> 用户指示："对于低优先级建议，可以记录，留待用户决定"。

## 待决定项

### L-1: `create_and_boot_openhcl.ps1` 用 `$args` 覆盖 PowerShell 自动变量

- 文件：`docs/superpowers/scripts/hyperv/create_and_boot_openhcl.ps1:37-50`
- 现状：脚本里 `$args = @{...}` splat hashtable；`$args` 是 PowerShell 自动变量，覆盖之属反模式
- 影响：该脚本已 deprecated，不会被推荐使用；不动也安全
- 建议：若清理则改为 `$vmArgs = @{...}`；否则不动
- 决策：☐ 改 / ☐ 不改

### L-4: 中英文括号混排风格

- 文件：`docs/superpowers/HYPERV_RUNBOOK.md` 多处
- 现状：中文文本里圆括号包英文，如 `期望（实测真 Hyper-V 输出）：`
- 影响：是项目整体风格，文档其他地方一致
- 建议：保持现状
- 决策：☐ 保持 / ☐ 全文统一为半角空格分隔

### L-6: `enable_vmbus_redirect.ps1` deprecation header 措辞

- 文件：`docs/superpowers/scripts/hyperv/enable_vmbus_redirect.ps1:1-14`
- 现状：注释说"保留作 ModifySystemSettings + CimSerializer + 异步 Job 等待的代码范例参考"
- 影响：可读性 OK，subagent 也建议保留
- 决策：☐ 保留 / ☐ 改写

### L-7: spec §3.4 protobuf `WriteGpaRequest.data` 字段说明

- 文件：`docs/superpowers/specs/2026-05-29-pcie-remote-design.md:287`
- 现状：`bytes data = 3;` 没在字段处注释 ≤ 64 KiB（但全局注释行 296 已提）
- 影响：非阻塞；不影响代码（`MAX_DMA_BYTES = 64 << 10` 在 lib.rs 强制）
- 建议：可在字段处加内联注释 `// ≤ MAX_DMA_BYTES = 64 KiB`
- 决策：☐ 加注释 / ☐ 不加

### L-8: USER_TODO §C 已是历史段

- 文件：`docs/superpowers/USER_TODO.md:79-83`
- 现状：§C "Service GUID 注册" 路径已修正（H-2），但整段是"历史已完成参考"
- 建议：保留作 onboarding 参考
- 决策：☐ 保留 / ☐ 移到 HYPERV_RUNBOOK §3 后删除

## 状态

| 编号 | 状态 |
|---|---|
| L-1 | 待决定 |
| L-4 | 待决定 |
| L-6 | 待决定 |
| L-7 | 待决定 |
| L-8 | 待决定 |
