#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netdb.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL

static void debug_line(const char *message) {
    char record[256];
    size_t len = strlen(message);
    if (len >= sizeof(record)) {
        static const char overflow[] = "netprobe: diagnostic record too long\n";
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, overflow, sizeof(overflow) - 1);
        return;
    }
    memcpy(record, message, len);
    record[len] = '\n';
    // One bounded syscall record prevents another service's line from being
    // interleaved between a proof message and its separate terminator.
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, record, len + 1);
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

// One datagram Echo exchange, not a raw socket or a TCP reachability proxy.
#define ECHO_PAYLOAD_BYTES 32
#define ECHO_PACKET_BYTES (8 + ECHO_PAYLOAD_BYTES)
#define GOOGLE_DEADLINE_US 15000000LL

static int64_t monotonic_us(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (int64_t)now.tv_sec * 1000000 + now.tv_nsec / 1000;
}

static uint16_t icmp_checksum(const unsigned char *packet, size_t length) {
    uint32_t sum = 0;
    for (size_t i = 0; i < length; i += 2) {
        sum += (uint16_t)packet[i] << 8;
        if (i + 1 < length) sum += packet[i + 1];
    }
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

static int wait_socket(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t now = monotonic_us();
        if (now < 0) return -1;
        if (now >= deadline) { errno = ETIMEDOUT; return -1; }
        struct pollfd pfd = {.fd = fd, .events = events};
        int result = poll(&pfd, 1, (int)((deadline - now + 999) / 1000));
        if (result < 0 && errno == EINTR) continue;
        if (result == 0) { errno = ETIMEDOUT; return -1; }
        if (result < 0) return -1;
        if (pfd.revents & (POLLERR | POLLHUP | POLLNVAL)) { errno = EIO; return -1; }
        if (pfd.revents & events) return 0;
    }
}

static int google_ping(void) {
    int64_t start = monotonic_us();
    if (start < 0) { log_errno_line("clock_gettime", errno); return 1; }
    int64_t deadline = start + GOOGLE_DEADLINE_US;
    struct addrinfo hints = {0};
    struct addrinfo *resolved = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    // Use the guest libc resolver, not a baked-in Google address.
    int error = getaddrinfo("google.com", NULL, &hints, &resolved);
    if (error != 0) {
        char message[128];
        snprintf(message, sizeof(message), "netprobe: google DNS failed gai=%d errno=%d", error, errno);
        log_line(message);
        return 1;
    }
    struct sockaddr_in peer = {0};
    for (struct addrinfo *it = resolved; it != NULL; it = it->ai_next) {
        if (it->ai_family == AF_INET && it->ai_addrlen >= sizeof(peer)) {
            memcpy(&peer, it->ai_addr, sizeof(peer));
            break;
        }
    }
    freeaddrinfo(resolved);
    char ip[INET_ADDRSTRLEN];
    if (peer.sin_family != AF_INET || inet_ntop(AF_INET, &peer.sin_addr, ip, sizeof(ip)) == NULL) {
        log_line("netprobe: google DNS returned no IPv4 address");
        return 1;
    }
    peer.sin_port = 0;
    char message[256];
    snprintf(message, sizeof(message), "netprobe: google DNS resolved host=google.com ip=%s", ip);
    log_line(message);
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, IPPROTO_ICMP);
    if (fd < 0) { log_errno_line("ICMP socket", errno); return 1; }
    const char *operation = "ICMP connect";
    if (connect(fd, (struct sockaddr *)&peer, sizeof(peer)) != 0) goto fail;
    struct sockaddr_in local = {0};
    socklen_t local_len = sizeof(local);
    operation = "ICMP getsockname";
    if (getsockname(fd, (struct sockaddr *)&local, &local_len) != 0) goto fail;
    if (local_len < sizeof(local) || local.sin_family != AF_INET || local.sin_port == 0) {
        errno = EPROTO;
        goto fail;
    }
    unsigned char request[ECHO_PACKET_BYTES] = {8, 0, 0, 0, 0, 0, 0, 1};
    memcpy(request + 4, &local.sin_port, sizeof(local.sin_port));
    operation = "getrandom";
    size_t random_bytes = 0;
    while (random_bytes < ECHO_PAYLOAD_BYTES) {
        int64_t now = monotonic_us();
        if (now < 0 || now >= deadline) {
            if (now >= deadline) errno = ETIMEDOUT;
            goto fail;
        }
        ssize_t count = getrandom(request + 8 + random_bytes, ECHO_PAYLOAD_BYTES - random_bytes, GRND_NONBLOCK);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) { if (count == 0) errno = EIO; goto fail; }
        random_bytes += (size_t)count;
    }
    uint16_t checksum = htons(icmp_checksum(request, sizeof(request)));
    memcpy(request + 2, &checksum, sizeof(checksum));
    operation = "ICMP send";
    int64_t sent_at = monotonic_us();
    if (sent_at < 0) goto fail;
    for (;;) {
        if (wait_socket(fd, POLLOUT, deadline) != 0) goto fail;
        ssize_t sent = send(fd, request, sizeof(request), 0);
        if (sent < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        if (sent != (ssize_t)sizeof(request)) { if (sent >= 0) errno = EIO; goto fail; }
        break;
    }
    operation = "ICMP reply";
    for (;;) {
        if (wait_socket(fd, POLLIN, deadline) != 0) goto fail;
        unsigned char reply[256];
        struct sockaddr_in source = {0};
        socklen_t source_len = sizeof(source);
        ssize_t received = recvfrom(fd, reply, sizeof(reply), 0, (struct sockaddr *)&source, &source_len);
        if (received < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        if (received < 0) goto fail;
        if (received != ECHO_PACKET_BYTES || source_len < sizeof(source) ||
            source.sin_family != AF_INET || source.sin_addr.s_addr != peer.sin_addr.s_addr ||
            reply[0] != 0 || reply[1] != 0 || icmp_checksum(reply, (size_t)received) != 0 ||
            memcmp(reply + 4, request + 4, ECHO_PACKET_BYTES - 4) != 0) continue;
        int64_t finished = monotonic_us();
        if (finished < 0) goto fail;
        if (finished >= deadline) { errno = ETIMEDOUT; goto fail; }
        snprintf(message, sizeof(message),
                 "netprobe: google echo reply host=google.com ip=%s id=%u seq=1 bytes=%d rtt_us=%lld verified=1",
                 ip, (unsigned)ntohs(local.sin_port), ECHO_PAYLOAD_BYTES, (long long)(finished - sent_at));
        log_line(message);
        close(fd);
        return 0;
    }
fail:
    log_errno_line(operation, errno);
    close(fd);
    return 1;
}

int main(void) {
    const char *qemu_mode = getenv("RUSTOS_NETPROBE_QEMU");
    int qemu_gateway = qemu_mode != NULL && strcmp(qemu_mode, "1") == 0;
    const char *google_mode = getenv("RUSTOS_NETPROBE_GOOGLE");
    int exercise_google = google_mode != NULL && strcmp(google_mode, "1") == 0;
    if (exercise_google && !qemu_gateway) return google_ping();
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
            return exercise_google ? google_ping() : 0;
        }
        log_errno_line("connect", errno);
        close(fd);
        return 1;
    }
    if (qemu_gateway) {
        log_line("netprobe: qemu gateway reachable");
        close(fd);
        return exercise_google ? google_ping() : 0;
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
