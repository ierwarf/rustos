#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_RUSTOS_DEBUG_PRINT
#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#endif

#define SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT 0x5255000fUL
#define SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER 0x52550022UL
#define SYS_RUSTOS_PROC_PREPARE_BROKER 0x52550026UL
#define IPC_SERVICE_INPUTD 8UL
#define IPC_SERVICE_DRIVERD 5UL

struct service_driver_resource_args {
    uint16_t abi_version;
    uint16_t op;
    uint32_t flags;
    uint64_t subject_pid;
    uint64_t subject_tid;
    uint64_t arg0;
    uint64_t arg1;
    uint64_t arg2;
    uint64_t out_ptr;
    uint64_t out_len;
    uint64_t reserved0;
};

struct proc_prepare_args {
    uint16_t abi_version;
    uint16_t format;
    uint32_t flags;
    uint64_t reserved0;
};

static void debug_write(const char *message) {
    size_t len = strlen(message);
    if (len != 0) {
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, message, len);
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static void log_status(const char *name, long rc, int err) {
    char line[192];
    snprintf(line, sizeof(line), "abifuzz: %s rc=%ld errno=%d", name, rc, err);
    debug_write(line);
    printf("%s\r\n", line);
    fflush(stdout);
}

static unsigned long parse_delay_ms(const char *arg) {
    const char prefix[] = "--delay-ms=";
    if (strncmp(arg, prefix, sizeof(prefix) - 1) != 0) {
        return 0;
    }
    char *end = NULL;
    unsigned long value = strtoul(arg + sizeof(prefix) - 1, &end, 10);
    if (end == arg + sizeof(prefix) - 1 || *end != '\0') {
        return 0;
    }
    return value;
}

static void sleep_ms(unsigned long ms) {
    while (ms > 0) {
        int chunk = ms > 1000 ? 1000 : (int)ms;
        (void)poll(NULL, 0, chunk);
        ms -= (unsigned long)chunk;
    }
}

static void fuzz_bad_user_pointers(void) {
    static const uintptr_t ptrs[] = {
        0,
        1,
        0x0000800000000000ULL,
        0xffff800000000000ULL,
        UINTPTR_MAX,
    };

    for (size_t i = 0; i < sizeof(ptrs) / sizeof(ptrs[0]); ++i) {
        errno = 0;
        long rc = syscall(SYS_write, STDOUT_FILENO, (const void *)ptrs[i], 16UL);
        log_status("bad-write-ptr", rc, errno);
    }
}

static void fuzz_raw_service_lookup(void) {
    errno = 0;
    long rc = syscall(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_INPUTD);
    log_status("lookup-inputd", rc, errno);

    errno = 0;
    rc = syscall(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_DRIVERD);
    log_status("lookup-driverd", rc, errno);
}

static void fuzz_privileged_brokers(void) {
    unsigned char out[64];
    struct service_driver_resource_args resource;
    memset(&resource, 0, sizeof(resource));
    resource.abi_version = 1;
    resource.op = 1;
    resource.subject_pid = (uint64_t)getpid();
    resource.subject_tid = (uint64_t)syscall(SYS_gettid);
    resource.arg0 = 0xfee00000UL;
    resource.arg1 = 0x1000UL;
    resource.out_ptr = (uint64_t)(uintptr_t)out;
    resource.out_len = sizeof(out);

    errno = 0;
    long rc = syscall(SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER, &resource);
    log_status("service-driver-broker", rc, errno);

    struct proc_prepare_args prepare;
    memset(&prepare, 0, sizeof(prepare));
    prepare.abi_version = 1;
    prepare.format = 1;

    errno = 0;
    rc = syscall(SYS_RUSTOS_PROC_PREPARE_BROKER, &prepare);
    log_status("proc-prepare-broker", rc, errno);
}

static ssize_t send_rights(int sock, const int *rights, size_t right_count) {
    char byte = 'r';
    struct iovec iov = {
        .iov_base = &byte,
        .iov_len = 1,
    };
    char control[CMSG_SPACE(sizeof(int) * 8)];
    memset(control, 0, sizeof(control));
    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = CMSG_SPACE(sizeof(int) * right_count);

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int) * right_count);
    memcpy(CMSG_DATA(cmsg), rights, sizeof(int) * right_count);

    return sendmsg(sock, &msg, MSG_DONTWAIT);
}

static int close_received_rights(struct msghdr *msg, const int *preserve, size_t preserve_count) {
    int closed = 0;
    for (struct cmsghdr *cmsg = CMSG_FIRSTHDR(msg); cmsg != NULL; cmsg = CMSG_NXTHDR(msg, cmsg)) {
        if (cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS) {
            continue;
        }
        size_t fd_bytes = cmsg->cmsg_len - CMSG_LEN(0);
        int *rights = (int *)CMSG_DATA(cmsg);
        for (size_t i = 0; i < fd_bytes / sizeof(int); ++i) {
            int keep = 0;
            for (size_t j = 0; j < preserve_count; ++j) {
                if (rights[i] == preserve[j]) {
                    keep = 1;
                    break;
                }
            }
            if (!keep && rights[i] >= 0) {
                close(rights[i]);
                ++closed;
            }
        }
    }
    return closed;
}

static void fuzz_unix_control_queue(void) {
    int fds[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, fds) != 0) {
        log_status("socketpair", -1, errno);
        return;
    }

    char control[CMSG_SPACE(sizeof(int) * 8)];
    char byte = 'x';
    struct iovec iov = {
        .iov_base = &byte,
        .iov_len = 1,
    };

    int sends = 0;
    for (int i = 0; i < 96; ++i) {
        memset(control, 0, sizeof(control));
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);

        struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
        cmsg->cmsg_level = SOL_SOCKET;
        cmsg->cmsg_type = SCM_RIGHTS;
        cmsg->cmsg_len = CMSG_LEN(sizeof(int) * 8);
        int *rights = (int *)CMSG_DATA(cmsg);
        for (int j = 0; j < 8; ++j) {
            rights[j] = fds[0];
        }

        errno = 0;
        ssize_t rc = sendmsg(fds[0], &msg, MSG_DONTWAIT);
        if (rc < 0) {
            log_status("sendmsg-control-stop", rc, errno);
            break;
        }
        ++sends;
        if ((sends % 32) == 0) {
            char line[128];
            snprintf(line, sizeof(line), "abifuzz: sendmsg-control sends=%d", sends);
            debug_write(line);
        }
    }

    char line[128];
    snprintf(line, sizeof(line), "abifuzz: sendmsg-control total=%d", sends);
    debug_write(line);

    int recvs = 0;
    int closed_received = 0;
    debug_write("abifuzz: recvmsg-control begin");
    while (recvs < sends) {
        char recv_byte = 0;
        char recv_control[CMSG_SPACE(sizeof(int) * 16)];
        struct iovec recv_iov = {
            .iov_base = &recv_byte,
            .iov_len = 1,
        };
        struct msghdr recv_msg;
        memset(&recv_msg, 0, sizeof(recv_msg));
        recv_msg.msg_iov = &recv_iov;
        recv_msg.msg_iovlen = 1;
        recv_msg.msg_control = recv_control;
        recv_msg.msg_controllen = sizeof(recv_control);

        errno = 0;
        ssize_t rc = recvmsg(fds[1], &recv_msg, MSG_DONTWAIT);
        if (rc < 0) {
            log_status("recvmsg-control-stop", rc, errno);
            break;
        }
        ++recvs;
        for (struct cmsghdr *cmsg = CMSG_FIRSTHDR(&recv_msg); cmsg != NULL;
             cmsg = CMSG_NXTHDR(&recv_msg, cmsg)) {
            if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
                size_t fd_bytes = cmsg->cmsg_len - CMSG_LEN(0);
                int *rights = (int *)CMSG_DATA(cmsg);
                for (size_t j = 0; j < fd_bytes / sizeof(int); ++j) {
                    if (rights[j] >= 0 && rights[j] != fds[0] && rights[j] != fds[1] &&
                        closed_received < 64) {
                        close(rights[j]);
                        ++closed_received;
                    }
                }
            }
        }
        if ((recvs % 16) == 0) {
            snprintf(line, sizeof(line), "abifuzz: recvmsg-control recvs=%d closed=%d", recvs,
                     closed_received);
            debug_write(line);
        }
    }
    snprintf(line, sizeof(line), "abifuzz: recvmsg-control total=%d closed=%d", recvs,
             closed_received);
    debug_write(line);

    close(fds[0]);
    close(fds[1]);
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, fds) != 0) {
        log_status("socketpair-edge", -1, errno);
        return;
    }

    char trunc_control[CMSG_SPACE(sizeof(int) * 2)];
    memset(trunc_control, 0, sizeof(trunc_control));
    struct msghdr trunc_send;
    memset(&trunc_send, 0, sizeof(trunc_send));
    trunc_send.msg_iov = &iov;
    trunc_send.msg_iovlen = 1;
    trunc_send.msg_control = trunc_control;
    trunc_send.msg_controllen = sizeof(trunc_control);
    struct cmsghdr *trunc_cmsg = CMSG_FIRSTHDR(&trunc_send);
    trunc_cmsg->cmsg_level = SOL_SOCKET;
    trunc_cmsg->cmsg_type = SCM_RIGHTS;
    trunc_cmsg->cmsg_len = CMSG_LEN(sizeof(int) * 2);
    int *trunc_rights = (int *)CMSG_DATA(trunc_cmsg);
    trunc_rights[0] = fds[0];
    trunc_rights[1] = fds[1];
    errno = 0;
    ssize_t trunc_send_rc = sendmsg(fds[0], &trunc_send, MSG_DONTWAIT);
    log_status("sendmsg-control-trunc-seed", trunc_send_rc, errno);
    if (trunc_send_rc >= 0) {
        char tiny_control[sizeof(struct cmsghdr)];
        char recv_byte = 0;
        struct iovec tiny_iov = {
            .iov_base = &recv_byte,
            .iov_len = 1,
        };
        struct msghdr tiny_recv;
        memset(&tiny_recv, 0, sizeof(tiny_recv));
        tiny_recv.msg_iov = &tiny_iov;
        tiny_recv.msg_iovlen = 1;
        tiny_recv.msg_control = tiny_control;
        tiny_recv.msg_controllen = sizeof(tiny_control);
        errno = 0;
        ssize_t tiny_rc = recvmsg(fds[1], &tiny_recv, MSG_DONTWAIT);
        log_status("recvmsg-control-truncated", tiny_rc, errno);
        snprintf(line, sizeof(line), "abifuzz: recvmsg-control-truncated flags=0x%x controllen=%lu",
                 tiny_recv.msg_flags, (unsigned long)tiny_recv.msg_controllen);
        debug_write(line);
    }

    char mixed_control[CMSG_SPACE(sizeof(int) * 2)];
    memset(mixed_control, 0, sizeof(mixed_control));
    struct msghdr mixed_msg;
    memset(&mixed_msg, 0, sizeof(mixed_msg));
    mixed_msg.msg_iov = &iov;
    mixed_msg.msg_iovlen = 1;
    mixed_msg.msg_control = mixed_control;
    mixed_msg.msg_controllen = sizeof(mixed_control);
    struct cmsghdr *mixed_cmsg = CMSG_FIRSTHDR(&mixed_msg);
    mixed_cmsg->cmsg_level = SOL_SOCKET;
    mixed_cmsg->cmsg_type = SCM_RIGHTS;
    mixed_cmsg->cmsg_len = CMSG_LEN(sizeof(int) * 2);
    int *mixed_rights = (int *)CMSG_DATA(mixed_cmsg);
    mixed_rights[0] = fds[0];
    mixed_rights[1] = -1;
    errno = 0;
    ssize_t mixed_rc = sendmsg(fds[0], &mixed_msg, MSG_DONTWAIT);
    log_status("sendmsg-control-mixed-invalid", mixed_rc, errno);

    memset(control, 0, sizeof(control));
    struct msghdr bad_msg;
    memset(&bad_msg, 0, sizeof(bad_msg));
    bad_msg.msg_iov = &iov;
    bad_msg.msg_iovlen = 1;
    bad_msg.msg_control = control;
    bad_msg.msg_controllen = sizeof(struct cmsghdr);
    struct cmsghdr *bad_cmsg = (struct cmsghdr *)control;
    bad_cmsg->cmsg_level = SOL_SOCKET;
    bad_cmsg->cmsg_type = SCM_RIGHTS;
    bad_cmsg->cmsg_len = sizeof(struct cmsghdr) - 1;
    errno = 0;
    ssize_t bad_rc = sendmsg(fds[0], &bad_msg, MSG_DONTWAIT);
    log_status("sendmsg-control-malformed", bad_rc, errno);

    close(fds[0]);
    close(fds[1]);
}

static void fuzz_fd_lifetime_edges(void) {
    char line[160];
    int tx[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, tx) != 0) {
        log_status("socketpair-lifetime", -1, errno);
        return;
    }

    int pairs[16][2];
    int pair_count = 0;
    for (; pair_count < 16; ++pair_count) {
        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, pairs[pair_count]) != 0) {
            log_status("socketpair-pressure-stop", -1, errno);
            break;
        }
    }

    int sent = 0;
    for (int i = 0; i < pair_count; ++i) {
        int rights[2] = {pairs[i][0], pairs[i][1]};
        errno = 0;
        ssize_t rc = send_rights(tx[0], rights, 2);
        if (rc < 0) {
            log_status("sendmsg-fd-pressure-stop", rc, errno);
            break;
        }
        ++sent;
    }
    for (int i = 0; i < pair_count; ++i) {
        close(pairs[i][0]);
        close(pairs[i][1]);
    }

    int recvd = 0;
    int closed = 0;
    while (recvd < sent) {
        char byte = 0;
        char control[CMSG_SPACE(sizeof(int) * 4)];
        struct iovec iov = {
            .iov_base = &byte,
            .iov_len = 1,
        };
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        errno = 0;
        ssize_t rc = recvmsg(tx[1], &msg, MSG_DONTWAIT);
        if (rc < 0) {
            log_status("recvmsg-fd-pressure-stop", rc, errno);
            break;
        }
        ++recvd;
        int preserve[2] = {tx[0], tx[1]};
        closed += close_received_rights(&msg, preserve, 2);
    }
    snprintf(line, sizeof(line), "abifuzz: fd-pressure sent=%d recvd=%d closed=%d", sent, recvd,
             closed);
    debug_write(line);

    int trunc[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, trunc) != 0) {
        log_status("socketpair-null-control", -1, errno);
        close(tx[0]);
        close(tx[1]);
        return;
    }
    int right = trunc[0];
    errno = 0;
    ssize_t send_rc = send_rights(trunc[0], &right, 1);
    log_status("sendmsg-null-control-seed", send_rc, errno);
    if (send_rc >= 0) {
        char byte = 0;
        struct iovec iov = {
            .iov_base = &byte,
            .iov_len = 1,
        };
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        errno = 0;
        ssize_t recv_rc = recvmsg(trunc[1], &msg, MSG_DONTWAIT);
        log_status("recvmsg-null-control", recv_rc, errno);
        snprintf(line, sizeof(line), "abifuzz: recvmsg-null-control flags=0x%x controllen=%lu",
                 msg.msg_flags, (unsigned long)msg.msg_controllen);
        debug_write(line);
    }
    close(trunc[0]);
    close(trunc[1]);
    close(tx[0]);
    close(tx[1]);
}

int main(int argc, char **argv) {
    int fd_transfer_stress = 0;
    unsigned long delay_ms = 0;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--fd-transfer-stress") == 0) {
            fd_transfer_stress = 1;
        } else {
            unsigned long parsed_delay = parse_delay_ms(argv[i]);
            if (parsed_delay != 0) {
                delay_ms = parsed_delay;
            }
        }
    }

    if (delay_ms != 0) {
        char line[128];
        snprintf(line, sizeof(line), "abifuzz: delay-ms=%lu", delay_ms);
        debug_write(line);
        sleep_ms(delay_ms);
    }

    debug_write("abifuzz: start");
    printf("abifuzz: start\r\n");
    fflush(stdout);

    fuzz_raw_service_lookup();
    fuzz_privileged_brokers();
    fuzz_bad_user_pointers();
    if (fd_transfer_stress) {
        fuzz_unix_control_queue();
        fuzz_fd_lifetime_edges();
    } else {
        debug_write("abifuzz: fd-transfer-stress skipped");
    }

    debug_write("abifuzz: done");
    printf("abifuzz: done\r\n");
    fflush(stdout);
    return 0;
}
