# usnvmemu（用户态 NVMe firmware）— 项目工作约定

> 本文件 = **必须遵守的规则 + 知识路由**。项目事实/教训/里程碑不写这里——它们在 auto-memory
> `MEMORY.md`（每会话自动加载，属背景上下文）和 `usnvmemu/docs/` 权威文档里。本文件保持精简
> （规则 + 指路），单一真相源，避免与 memory/docs 重复。

## 这是什么 / 回项目第一步

microsoft/openvmm fork 上 building 的用户态 NVMe firmware：runtime-agnostic controller core +
trait Transport，3 个接入（OpenHCL vsock / vfio-user / NVMe-oF TCP），各接入平等、按价值挑活。

回到本项目第一步：读 `usnvmemu/docs/ROADMAP.md`（找 HIGH 未做）+ auto-memory `MEMORY.md`
（当前状态入口）。深层：`docs/{LESSONS,DECISIONS,MILESTONES,PRINCIPLES,PROJECT_VISION}.md`。

## 硬约束（指令，非背景建议）

1. **语言**：新增/修改的注释、文档、PR、对话用**中文**；原 upstream 英文内容保持不动。
2. **fork + 多会话共享工作树**：可能有别的 Claude 会话并行改同一树。
   - **可以改别的会话也在改的文件**（必要时就改）；约束不在"碰不碰"，而在"提交时
     **line-range / hunk 级隔离**"——只把自己的改动塞进 commit，**绝不裹挟别人的 hunk**。
   - 隔离技法（`git add -p` 交互式在本环境不可用，改用 `git apply --cached`）：
     `git diff -- <file>` 看全部 hunk → 截出只含自己 hunk 的 patch → `git apply --cached <my.patch>`
     塞进 index → **裸 `git commit`**（读 index，不带 pathspec）。整文件精确替换则用
     `git hash-object` + `git update-index` 塞内容再裸 commit。
   - **陷阱**：`git commit -- <pathspec>` 提交的是**工作树版**（含别人的 hunk），**不是** index 版
     ——精确 staging 后**不要**带 pathspec commit。见 memory `shared-worktree-fmt-hazard-index-commit`。
   - **绝不 `git add -A` / `git add .`**；新建文件需先**精确** `git add <新文件>`；
     `-m` 等 option 放 `--` 前；提交前 `git status` 确认 index 只含自己的东西。
3. **构建/运行按目标而定**：OpenHCL 走 WSL 交叉编译（`cargo xflowey build-igvm x64`），产物在
   Windows Hyper-V 跑（无原生 Windows 构建）；toolchain 已钉 **1.95**（命令不加 `+1.95`）。
   vfio-user / NVMe-oF TCP 是 Linux 原生可测。
4. **改动完同步对应文档**：改了 spec/特性就同步 ROADMAP / SPEC_CONFORMANCE / README / codemap
   ——这是"完成"的组成部分，不是可选收尾。

## 工作方式（本项目强制）

- 改完代码/文档/脚本必经**对应 subagent review**（rust-reviewer / architect / 等）再 commit；
  "我验过了"≠ review。
- commit 按**语义单元粗粒度**，不逐 step（抽一个 crate = 单 commit；subagent 可分 step 工作但
  只在语义单元结尾 commit）。
- 大任务先 **plan → architect review → subagent-driven** 执行；依赖未验证承重假设的设计**先 POC**
  再定方案。
- 让用户去 **new context** 执行时，配齐"完整可粘贴的初始提示词 + 目标明确经核验的 plan 文档"；
  混合详度 plan 显式标注 execution-ready vs roadmap，依赖前序结论的 phase 加详化 gate 不伪详化。
- 价值取向：只认**长远正确 + 设计良好**；要**有意义的完整**改动，不为小而小、不拿成本/ROI 砍
  掉让事情真正能用的部分（可拆基础/高级）。

## 知识在哪（路由，勿在本文件复制内容）

- 当前进度 / 状态 / 里程碑 / 教训 / 用户偏好 → auto-memory `MEMORY.md`（自动加载）。
- 设计决策 ADR → `usnvmemu/docs/DECISIONS.md`；技术教训 → `docs/LESSONS.md`；
  历史里程碑 → `docs/MILESTONES.md`；原则 → `docs/PRINCIPLES.md`；愿景 → `docs/PROJECT_VISION.md`。
- 新增 transport 指南 → `docs/HOW_TO_ADD_TRANSPORT.md`；host-root 运行 → `docs/RUNBOOK_HOST_ROOT.md`。
- 待执行的跨会话任务 + 设计 → `docs/superpowers/plans/*.md` 与 `docs/superpowers/specs/*.md`。
