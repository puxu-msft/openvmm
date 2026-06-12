# W6b reconnect 模型 POC 归档（operator 拥有 usnvmemu + underhill 持久 reconnect）

**日期**：2026-06-12  **VM**：pcie-remote-exp（真 OpenHCL，本会话 W6b IGVM）
**目标**：验证「operator 经 ohcldiag-dev 起 usnvmemu（运行时、可换二进制、可启停）+ underhill
持久 reconnect」这条最宽松路线的承重假设。方法：**最宽松假设起，失败再逐步加严**。

术语：**usnvmemu** = 用户态 NVMe 模拟器（在 VTL2 跑 vfio-user AF_UNIX server 的进程，
crate `nvme_firmware`，`--vfio-user-sock` 模式）。underhill = VTL2 supervisor（vfio-user client）。

---

## POC-1 — namespace 共享（最宽松假设）✅ PASS

**假设**：ohcldiag-dev `run` 子进程与 underhill_core 同 mount namespace → operator 起的
usnvmemu 的 `/tmp/X.sock` 对 underhill 可见。

**方法**：ohcldiag-dev run 中读 `/proc/self/ns/mnt`、`/proc/1/ns/mnt`、遍历找 underhill 进程的 ns/mnt。

**结果**：全部相同 `mnt:[4026531832]`：
- run 子进程 / `underhill-init`(PID1) / `openvmm_hcl`(PID36) / `underhill-vm`(PID42)。

**结论**：承重假设成立。之前 realize-only 的 restart 失败是 underhill worker `restart` 重置
VTL2 /tmp，**非** namespace 问题。持久 reconnect（不 restart）模型下，operator 起的 usnvmemu
socket 对 underhill 可见 → 可连。

---

## POC-2 — usnvmemu setsid 持久（脱离 ohcldiag-dev run 存活）✅ PASS

**假设**：operator 经 `ohcldiag-dev run` 起的 usnvmemu，用 setsid 脱离 run-shell 后，run
命令返回后仍持续 listening。

**方法**：run #A2 push fw+cl（**校验 size==host**，防 ohcldiag-dev 大 push 截断）→ `setsid
/tmp/fw --vfio-user-sock /tmp/vfio_nvme.sock --backing-file ... &` → run 返回。run #B（**独立
invocation**）查 socket + 进程。

**结果**：run #A2 `size_match=yes` + socket=yes + fw.log "server listening"（pid 61）；run #B
`socket_persisted=yes` + 同 pid 61（未重生）。

**结论**：setsid 持久成立。**坑**：首次 run #A 无校验时 ohcldiag-dev run 超时导致 fw 二进制
**截断** → 启动即崩（fw.log 空、无 socket）。operator 启动工具必须校验二进制完整性。

## POC-3 — 跨 run 进程 connect + 全握手 + DMA（underhill 的 proxy）✅ PASS

**假设**：一个与 usnvmemu 不同的进程（独立 ohcldiag-dev run，代表将来的 underhill reconnect）
能 connect + vfio-user 握手 + 读 identity + DMA。

**方法**：run #B 跑 W5a test client（真 `vfio_user_device` 客户端 API）against 持久 usnvmemu。

**结果**：`handshake ok: server 0.1` / `num_regions=9 num_irqs=5` / `dma_map ok` / `Identify MN
= "OpenHCL Userspace NVMe v2.0"` / `W5A PASSED ✓`（零拷贝真 guest RAM 端到端，跨独立 invocation）。

**结论**：operator 起的持久 usnvmemu 可被另一进程完整驱动。underhill 将来作此 client（加
reconnect 循环）即可。

## POC-4 — detach/re-attach 循环（stop → connect fail → restart → 重连）✅ PASS

**假设**：operator kill usnvmemu（detach）后 connect 失败；relaunch（restart/换二进制）后
fresh connect 成功（re-attach）。这是「运行时启停 + dev 换二进制」的平台基础。

**方法**：`pkill -x fw`（**精确 comm 匹配**，避免 `pkill -f vfio-user-sock` 误杀含该串的 run-shell
自身——首次踩坑 255 自杀）→ client 连（应 fail）→ rm 残留 socket + relaunch → client 连（应 ok）。

**结果**：kill 后 `sock_file=lingers`（socket 文件残留）+ client = `Connection refused (os error
111)`；relaunch（rm+rebind）后 sock=yes + client `handshake ok` + `Identify MN` + `PASSED`。

**结论**：detach/re-attach 平台可行。**关键设计输入**：
1. usnvmemu 死后 socket 文件**残留** → connect 得 **ECONNREFUSED(111)** 而非 ENOENT。
   underhill reconnect 必须把 ENOENT（从未起）/ ECONNREFUSED（死了留残留）/ 握手失败
   **都当作"不可用，继续重试"**。
2. relaunch 前必须 `rm` 残留 socket 再 bind。
3. `pkill -f <pattern>` 会误杀含该串的 run-shell 自身——VTL2 内进程操作用精确 comm 匹配。

---

## POC 链总结 → 设计输入

最宽松路线（operator 起持久 usnvmemu + underhill 持久 reconnect，全程不重建 IGVM、不 restart
underhill）的**所有平台承重假设已验证成立**（POC-1..4）。**唯一剩余未验证 = underhill reconnect
代码本身**（持久重连循环 + Connecting/Live/Lost 状态机驱动 device 呈现），属实现，非平台假设。

下一步设计聚焦：①underhill 把"一次性 connect+cap→Absent"改成"持久 reconnect 循环"（参照
pcie_remote K-20 hotplug/transport_swap，但 client 主动连而非 listen）；②device 跨 Live/Lost 的
guest 呈现 + identity bootstrap（首次连上才知 identity）；③ENOENT/ECONNREFUSED/握手失败统一重试。
