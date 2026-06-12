// pt_diff — 最小 NVMe passthru 注入器（差分 POC 用）。
//
// 目的：把一条**字节级可控**的 NVMe 命令经 Linux 内核 passthru ioctl 注入到一个
// /dev/nvmeN，打印 controller 回的 status（SCT|SC，已去 phase 位）+ result(CDW0) +
// 注入路径的 errno。POC 的承重假设就靠它验：
//   (H1) 同一逻辑命令 → 到达两个 controller 的 SQE 等价（同一内核同一 PCIe 路径，按构造成立）；
//   (H2) 畸形/边界命令能等价注入 + 内核 passthru 透传（不在到达 controller 前被改写/拒绝）。
//
// 用法：
//   pt_diff <dev> <admin|io> <opcode> <nsid> <c10> <c11> <c12> <c13> <c14> <c15> <datalen> <dir>
//   dir: r=controller→host(读), w=host→controller(写), n=无数据
// 所有数字十六进制或十进制（strtoul base 0）。datalen 字节，buffer 4KiB 对齐。
//
// 输出单行：  status=0x%04x result=0x%08x ioctl_ret=%d errno=%d(%s)
// ioctl_ret>=0 即 controller 返回的 NVMe status（>>1 去 phase）；<0 表示内核在提交前
// 自己拒了（errno），这正是 H2 要暴露的"passthru 不透传"情形。

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>

// 摘自 <linux/nvme_ioctl.h>，避免依赖 guest 内是否有该头。
struct nvme_passthru_cmd {
    uint8_t  opcode;
    uint8_t  flags;
    uint16_t rsvd1;
    uint32_t nsid;
    uint32_t cdw2;
    uint32_t cdw3;
    uint64_t metadata;
    uint64_t addr;
    uint32_t metadata_len;
    uint32_t data_len;
    uint32_t cdw10;
    uint32_t cdw11;
    uint32_t cdw12;
    uint32_t cdw13;
    uint32_t cdw14;
    uint32_t cdw15;
    uint32_t timeout_ms;
    uint32_t result;
};

#define NVME_IOCTL_ADMIN_CMD _IOWR('N', 0x41, struct nvme_passthru_cmd)
#define NVME_IOCTL_IO_CMD    _IOWR('N', 0x43, struct nvme_passthru_cmd)

static uint32_t x(const char *s) { return (uint32_t)strtoul(s, NULL, 0); }

int main(int argc, char **argv) {
    if (argc < 13) {
        fprintf(stderr, "usage: %s dev admin|io opcode nsid c10 c11 c12 c13 c14 c15 datalen r|w|n\n", argv[0]);
        return 2;
    }
    const char *dev   = argv[1];
    int is_admin      = (strcmp(argv[2], "admin") == 0);
    uint8_t  opcode   = (uint8_t)x(argv[3]);
    uint32_t nsid     = x(argv[4]);
    uint32_t c10      = x(argv[5]);
    uint32_t c11      = x(argv[6]);
    uint32_t c12      = x(argv[7]);
    uint32_t c13      = x(argv[8]);
    uint32_t c14      = x(argv[9]);
    uint32_t c15      = x(argv[10]);
    uint32_t datalen  = x(argv[11]);
    char     dir      = argv[12][0];

    int fd = open(dev, O_RDWR);
    if (fd < 0) { fprintf(stderr, "open %s: %s\n", dev, strerror(errno)); return 3; }

    void *buf = NULL;
    if (datalen > 0) {
        if (posix_memalign(&buf, 4096, datalen) != 0) { perror("memalign"); return 3; }
        memset(buf, (dir == 'w') ? 0x5a : 0x00, datalen);
    }

    struct nvme_passthru_cmd cmd;
    memset(&cmd, 0, sizeof(cmd));
    cmd.opcode   = opcode;
    cmd.nsid     = nsid;
    cmd.addr     = (uint64_t)(uintptr_t)buf;
    cmd.data_len = datalen;
    cmd.cdw10    = c10;
    cmd.cdw11    = c11;
    cmd.cdw12    = c12;
    cmd.cdw13    = c13;
    cmd.cdw14    = c14;
    cmd.cdw15    = c15;
    cmd.timeout_ms = 5000;

    errno = 0;
    int ret = ioctl(fd, is_admin ? NVME_IOCTL_ADMIN_CMD : NVME_IOCTL_IO_CMD, &cmd);
    int e = errno;

    // ret>=0 → controller 返回的 NVMe status（内核已 >>1 去 phase）。
    uint16_t status = (ret >= 0) ? (uint16_t)ret : 0;
    printf("status=0x%04x result=0x%08x ioctl_ret=%d errno=%d(%s)\n",
           status, cmd.result, ret, (ret < 0) ? e : 0,
           (ret < 0) ? strerror(e) : "ok");

    close(fd);
    if (buf) free(buf);
    return 0;
}
