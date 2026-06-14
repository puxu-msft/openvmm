# usnvmemu → 母项目:Windows NVMe-oF 互通手册的证据本位回应

> **本文档来历与定位**
> 这是 **usnvmemu 团队**对 [nvme-of-interop-on-ws-handbook.md](nvme-of-interop-on-ws-handbook.md)
> (母项目 windows-nvme-controller-emulator 2026-06-13 交接)的回应。手册请 usnvmemu「据 §9 裁决」;
> 本文档把手册的每条断言**对照 usnvmemu 源码逐条核验**(file:line 锚定),加一个**实测 probe**
> (`tests/vt_static_controller_model_probe.rs`,3 绿),指出**一处事实错误**,并给出**决策排序**。
>
> **核心认同**:手册最大的价值不是"Windows 能连"(未证),而是把一个 usnvmemu 在纯 Linux 环境
> **结构性看不见**的东西摆到面前——**static controller model**。这一判断 usnvmemu 代码+实测**完全确认**。
>
> **置信度图例**:✅ 源码/probe 确认 / ❓ 仍未决(需真连) / ⚠️ caveat 成立 / ❌ 手册事实错误

> **更新（2026-06-14）— usnvmemu 侧已修复 + 🎉 真 WS2025 出盘实测打通**：
> static-model 盲区已在 usnvmemu 端落地 spec-conformant 修复（C1 commit `e2c022575` Connect CNTLID
> 校验 + SCT 0x07→0x01 修复 + IPO/IATTR；C2 discovery static-model 广告 + bin
> `--discovery-static-cntlid`）。下文 §2.1 描述的「静默 coerce」已改为：具体-CNTLID mismatch → reject
> SC=0x82 + IPO/IATTR；dynamic/static-any/具体匹配 → accept；discovery 可广告具体 CNTLID。
>
> **进一步：usnvmemu 团队自建 WS2025 Hyper-V VM 跑了 real Windows 真连**（不再待母项目）。结果：
> - ✅ **手册「未证的 Windows 能连」现已 PROVEN**：真 WS2025 inbox initiator（`stornvmeofi.sys` /
>   `nvmeofutil connect -ci`，static model）→ Hyper-V relay → usnvmemu target，`Get-Disk` 显
>   **"NVMe OpenHCL Userspace NVMe v2.0" Online / 64 MiB**，raw 4KB write+read **byte-equal**。
> - ✅ **§5 的 TCP-vs-RDMA 单点 gate 现已有答案**：**Windows inbox 支持 TCP**（实测出盘）——推翻手册
>   「RDMA-only」倾向。VM 内 RDMA 驱动在场但无 RDMA NIC，故 RDMA 路本轮未测（V9 RDMA 仍独立成线）。
> - 🔧 **真连暴露并修了第三个 transport-无关缺口（C3）**：Windows IO-queue Fabric Connect 的 1024B
>   Connect data **不走 in-capsule、走 Transport SGL（type 0x5/subtype 0xA）经 R2T/H2CData**（因
>   IOCCSZ=4 广告 IO 无 in-capsule，Windows 正确）；旧 target 在 parse 前 `data.len()!=1024` 拒掉、
>   从不发 R2T → IO 队列建不起、不出盘。Linux nvme-cli 总 in-capsule 故纯 Linux 永远盖不到。修复保
>   IOCCSZ=4、新增 transport-SGL Connect 路由 + R2T fetch + Windows H2CData PLEN=HLEN quirk 补读。
>   regression gate `vt_windows_transport_sgl_connect.rs`（3 测，精确 Windows wire）。
>
> 仅剩 §3 DHCHAP DH-group 缺口未触发（VM 未配 in-band auth），留待需认证的真连场景。

---

## 1. TL;DR + 裁断

- ✅ **static controller model 盲区成立且实测坐实**。usnvmemu 是 **dynamic-controller-model-only**:
  Discovery 只播 `0xFFFF`、Connect 无视 host 请求的 CNTLID 静默返 1。probe 实测:host 请求 CNTLID=5 →
  SC=0 + 分配 1(**静默 coerce,无错误**),non-conformant。
- 🔑 **usnvmemu 的增量洞察**:这个缺口在 **transport-无关**的 fabric/discovery 层(不在 TCP PDU 层)→
  **裁决问题 2「把 Windows 当 V9 RDMA 靶」时它照样继承**。不管 §5 的 TCP-vs-RDMA 怎么裁,这条 interop
  信息都成立、都要面对。
- ❌ **手册一处事实错误**:§7 称"usnvmemu 有 DH-2048..8192"——**反了**。usnvmemu DHCHAP 是 HMAC-only /
  `DHGROUP_NULL`,反而**拒绝**不提供 NULL 组的 host。这是第二个 Windows-only 盲区。
- ❓ **TCP vs RDMA 仍是单点 gate**,认同手册:静态分析不能定论,须一次真连。usnvmemu 侧无法补外部权威证据。

---

## 2. 代码 + probe 实证核验

### 2.1 static controller model 盲区 —— ✅ 两端源码 + probe 坐实

NVMe-oF Connect data(spec § 3.3)的 CNTLID 语义:`0xFFFF`=dynamic、`0xFFFE`=static-any、
其它=static 指定具体 controller。usnvmemu 两端:

| 层 | 证据 | 行为 |
|----|------|------|
| Discovery Log | [discovery_log.rs:173](../../crates/nvme_firmware/src/controller/discovery_log.rs) `cntlid: 0xFFFF, // dynamic` | **只播 dynamic** |
| Connect handler | [async_session.rs:634-774](../../crates/nvme_of_tcp_target/src/async_session.rs) 解析 `ConnectData` 但**从不读 `cd.cntlid`**;[:773](../../crates/nvme_of_tcp_target/src/async_session.rs) 无条件返 `self.cntlid` = `TEACHING_CNTLID=1`([fabric.rs:44](../../crates/nvme_of_tcp_target/src/fabric.rs)) | **无视请求,静默返 1** |
| 文档 | `grep static.?controller usnvmemu/docs/` = 0 命中 | **不在任何人雷达上** |

**实测 probe** [`tests/vt_static_controller_model_probe.rs`](../../crates/nvme_of_tcp_target/tests/vt_static_controller_model_probe.rs)(3 绿,raw wire 不经 nvme-cli/kernel——因为 kernel host 控制不了发什么 CNTLID,只有自构造 PDU 才能探 static):

| probe | host 请求 cd.cntlid | 实测 SC | 实测分配 CNTLID | 判 |
|-------|--------------------|---------|----------------|----|
| `dynamic_request_0xffff_assigns_cntlid_1` | `0xFFFF` | 0 | 1 | baseline(Linux 已覆盖) |
| `static_specific_request_is_silently_coerced_to_1` | `5` | **0** | **1** | **静默 coerce,non-conformant** |
| `static_any_request_0xfffe_assigns_cntlid_1` | `0xFFFE` | 0 | 1 | static-any 返 1 尚可接受 |

**spec 偏离**:static controller model 下,请求具体 CNTLID 若不可满足,应返 SC=0x82(Connect Invalid
Parameters),而非静默换一个。usnvmemu 当前 host 要 5、拿到 1、**无错误指示**。

**行为推断(不 overclaim)**:
- Windows 若走 **discovery-driven** connect,读到 `0xFFFF` 同样按 dynamic 连 → **可能也通**。
- 真正破裂点是 Windows static 模型用 `connect -ci <非1值>` 显式请求 → 静默 coerce(latent 缺口)。
- **所以 §5 真连测试必须覆盖两面**:`-dy true`(dynamic,Linux 已验)**和** 默认 `-dy false` + `-ci`,
  否则只测 dynamic 会"假通过"、根本没碰这个盲区。

### 2.2 storport / `BusTypeNvmeof` —— ⚠️ 是 Windows 呈现细节,非 usnvmemu 侧缺口

手册 §6 的 `BusTypeNvmeof(0x14) ≠ BusTypeNvme` 架构上成立且如实标注。**对 usnvmemu 无代码动作**——
它是目标定义提醒(本路达成"Windows 消费存储"非"Windows 看到 PCIe NVMe controller")。

## 3. 手册一处事实错误(证据本位指出)

**§7 line 152「usnvmemu 有 DH-2048..8192」—— ❌ 错,且方向相反。**

usnvmemu DHCHAP 是 **HMAC-only / `DHGROUP_NULL`(dhgid=0)**:[dhchap.rs:4](../../crates/nvme_of_tcp_target/src/dhchap.rs)
("HMAC-only")、[dhchap.rs:517](../../crates/nvme_of_tcp_target/src/dhchap.rs)("不实现 DH ephemeral key
exchange (dhgid=0 NULL)")。它不但没有 DH-2048+,反而在拒绝逻辑 [dhchap.rs:666-668](../../crates/nvme_of_tcp_target/src/dhchap.rs)
**拒绝**不列 NULL 组的 host(`FAIL_EXP_DHGROUP_UNUSABLE`,常量定义 [dhchap.rs:562](../../crates/nvme_of_tcp_target/src/dhchap.rs))。

**这反而加重 Windows 认证风险**:若 WS2025 `authkey`/`connect -hk/-sk` 要求真 DH 组(不接受 NULL),
usnvmemu **根本无法完成 in-band auth**。第二个 Linux-invisible 缺口(Linux nvme-cli 会列 NULL,Windows
是否列待测)。

## 4. 决策排序(usnvmemu 增量)

手册三问平铺;实际有依赖序:

```
Gate 0:  Windows inbox 支持 TCP 吗?  ──(一次真连测试,手册 §5)
  │
  ├─ 否(RDMA-only)→ TCP 路死;转裁决问题 2(V9 RDMA 靶)。
  │                  但 static-model(§2.1)+ DH-group(§3)缺口 transport-无关,V9 照样继承。
  │
  └─ 是 → 路通。下一个真实 blocker 按序:
          (a) static controller model(若 Windows 用 static + 显式 -ci)── §2.1 实测确认
          (b) DHCHAP DH-group 不匹配(若 Windows 要求真 DH 非 NULL)── §3 确认
          两者都是 Linux 测试盖不到、已确认的洞。
```

## 5. 三裁决问题的回应(给证据 + usnvmemu 倾向,产品方向留 owner)

**Q1 值不值得一次 host-root TCP 真连测试?** —— **usnvmemu 倾向值得**(target 已 production 级、
成本低)。**前提**:测试设计**必须同时覆盖 `-dy true` 和默认 `-dy false`+`-ci`**,否则等于没碰 §2.1 盲区。

**Q2 若 RDMA-only,把 Windows 当 V9 首个真消费方?** —— **证据强**:[ROADMAP.md §3 V9](../../docs/ROADMAP.md)
是 6+ 月架构级,其明列痛点之一就是"测试基础设施重写(soft-RoCE in WSL2)";现成的真 Windows RDMA
initiator 正好绕开 soft-RoCE 自环。**但诚实**:V9 会继承 §2.1 + §3 两个 fabric 层缺口,不是纯加 RDMA framing。

**Q3 `BusTypeNvmeof` 呈现差异可接受吗?** —— 纯目标定义问题,**留项目 owner**。证据=手册 §8:本路给的是
"Windows 消费我的存储",**不是**"Windows 看到 PCIe NVMe controller"(后者是 OpenHCL guest 路径 1/2 已达成)。

## 6. 给母项目的协作接口

usnvmemu 侧**已先在 Linux 把盲区坐实**(§2.1 probe),母项目跑 Windows 真连前,这端 static 行为已知。
可提供的**可监听 TCP target 命令**(手册 §5 的最小协作接口):

```bash
# usnvmemu 侧:起 NVMe-oF TCP target(production 级,已有)
cd usnvmemu/crates/nvme_of_tcp_target
truncate -s 1G /tmp/ns1.img
cargo run --release -- --listen 0.0.0.0:4420 --backing-file /tmp/ns1.img \
  --i-know-this-is-insecure          # 绑非 loopback 需显式确认
# subsys NQN 默认 nqn.2026-06.io.openhcl:nvme.userspace;Discovery 见 README
```

**母项目跑 Windows 真连时请务必两组都测**(否则漏 §2.1):
1. `nvmeofutil add -t sp -ta <ip> -ts 4420 -nq <nqn> -dy true`(dynamic)→ connect → 期望出盘。
2. 默认 `-dy false` + `nvmeofutil connect ... -ci <非1值>`(static specific)→ **观测**:usnvmemu 会**静默
   返 CNTLID=1**(§2.1 已知),看 Windows 这端**是否接受 result≠请求值**(若 Windows 校验则连失败 = 缺口现形)。

## 7. 复现

```bash
# usnvmemu 侧 static-model probe(Linux,无 sudo/kernel,deterministic)
cd usnvmemu/crates/nvme_of_tcp_target
cargo test --test vt_static_controller_model_probe -- --nocapture
# 3 绿:dynamic→1 / static specific 5→静默 1 / static-any→1
```

深层:盲区落档见 [ROADMAP.md §1 V-followup-static-controller-model](../../docs/ROADMAP.md);
导览见 [NVME_OF_TCP.md](../../docs/NVME_OF_TCP.md)。

---

**文档状态**:✅ static-model 盲区 = 源码+probe 确认 **+ 2026-06-14 usnvmemu 侧已修复(C1 `e2c022575` + C2)**;
❌ DH-group = 手册事实错误;❓ TCP 支持 = 仍待真连。**usnvmemu 已修复盲区 + 落档 + regression gate;
real Windows 真连待母项目(§6)。**
