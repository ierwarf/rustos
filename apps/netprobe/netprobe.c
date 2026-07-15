#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL

static void debug_line(const char *message) {
    size_t len = strlen(message);
    if (len != 0) {
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, message, len);
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static void log_line(const char *message) {
    // netprobe is deliberately no-display and may have no console consumer.
    // The KVM acceptance gate reads debugcon, so publish lifecycle/proof
    // markers to the same bounded diagnostic ABI used by shell and abifuzz.
    // stdout remains useful when the probe is launched manually.
    debug_line(message);
    printf("%s\r\n", message);
    fflush(stdout);
}

static void log_errno_line(const char *operation, int error) {
    char message[128];
    int written = snprintf(message, sizeof(message), "netprobe: %s failed errno=%d", operation,
                           error);
    if (written > 0 && (size_t)written < sizeof(message)) {
        log_line(message);
    } else {
        log_line("netprobe: failure diagnostic truncated");
    }
}

int main(void) {
    const char *qemu_mode = getenv("RUSTOS_NETPROBE_QEMU");
    int qemu_gateway = qemu_mode != NULL && strcmp(qemu_mode, "1") == 0;
    const char *target = qemu_gateway ? "10.0.2.2" : "142.250.72.14";
    unsigned short port = qemu_gateway ? 9 : 80;
    log_line(qemu_gateway ? "netprobe: start target=qemu-gateway" : "netprobe: start");

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        log_errno_line("socket", errno);
        return 1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, target, &addr.sin_addr) != 1) {
        log_line("netprobe: inet_pton failed");
        close(fd);
        return 1;
    }

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        if (qemu_gateway && errno == ECONNREFUSED) {
            log_line("netprobe: qemu gateway reachable");
            close(fd);
            return 0;
        }
        log_errno_line("connect", errno);
        close(fd);
        return 1;
    }
    if (qemu_gateway) {
        log_line("netprobe: qemu gateway reachable");
        close(fd);
        return 0;
    }
    log_line("netprobe: connect ok");

    const char request[] =
        "GET / HTTP/1.0\r\n"
        "Host: google.com\r\n"
        "Connection: close\r\n"
        "\r\n";
    ssize_t sent = send(fd, request, sizeof(request) - 1, 0);
    if (sent < 0) {
        log_errno_line("send", errno);
        close(fd);
        return 1;
    }
    printf("netprobe: sent %ld bytes\r\n", (long)sent);
    fflush(stdout);

    char buffer[512];
    ssize_t received = recv(fd, buffer, sizeof(buffer) - 1, 0);
    if (received < 0) {
        log_errno_line("recv", errno);
        close(fd);
        return 1;
    }
    buffer[received] = '\0';
    printf("netprobe: received %ld bytes\r\n", (long)received);
    printf("netprobe: response begin\r\n%.*s\r\nnetprobe: response end\r\n", (int)received, buffer);
    fflush(stdout);

    close(fd);
    return 0;
}
