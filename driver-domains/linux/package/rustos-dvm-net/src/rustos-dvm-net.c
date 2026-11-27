// SPDX-License-Identifier: MIT
// Linux DVM raw Ethernet relay for the fixed RustOS two-ring ivshmem contract.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/ethtool.h>
#include <linux/if_packet.h>
#include <linux/sockios.h>
#include <limits.h>
#include <net/if.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define MAGIC "RSDVMNT1"
#define HEADER_BYTES 4096U
#define SLOT_COUNT 64U
#define SLOT_BYTES 2048U
#define MTU 1514U
#define FLAG_READY 1U
#define FLAG_DVM_READY 2U
#define KNOWN_FLAGS (FLAG_READY | FLAG_DVM_READY)
#define USED_BYTES (HEADER_BYTES + 2U * SLOT_COUNT * SLOT_BYTES)
#define APERTURE_BYTES (512U * 1024U)
#define UIO_NAME "rustos-dvm-net"
#define FLAGS 36U
#define TX_HEAD 40U
#define TX_TAIL 44U
#define RX_HEAD 48U
#define RX_TAIL 52U
#define TX_WAIT_ARMED 64U
#define RX_WAIT_ARMED 68U
#define TX_RING HEADER_BYTES
#define RX_RING (HEADER_BYTES + SLOT_COUNT * SLOT_BYTES)
/* Timeout recovers a lost notification and retries transient TX backpressure. */
#define RELAY_WATCHDOG_MS 20
#define TX_RETRY_MS 1
#define ACTIVE_POLL_TURNS 16U

_Static_assert(USED_BYTES <= APERTURE_BYTES, "network rings exceed BAR2");
_Static_assert((FLAGS % _Alignof(uint32_t)) == 0U, "flags must be atomic aligned");
_Static_assert((TX_HEAD % _Alignof(uint32_t)) == 0U, "tx head must be atomic aligned");
_Static_assert((TX_TAIL % _Alignof(uint32_t)) == 0U, "tx tail must be atomic aligned");
_Static_assert((RX_HEAD % _Alignof(uint32_t)) == 0U, "rx head must be atomic aligned");
_Static_assert((RX_TAIL % _Alignof(uint32_t)) == 0U, "rx tail must be atomic aligned");
_Static_assert((TX_WAIT_ARMED % _Alignof(uint32_t)) == 0U, "tx wait must be atomic aligned");
_Static_assert((RX_WAIT_ARMED % _Alignof(uint32_t)) == 0U, "rx wait must be atomic aligned");

struct shared_net { int fd; volatile uint8_t *base; size_t bytes; uint32_t tx_tail; uint32_t rx_head; int last_tx_errno; int doorbell; };
struct raw_endpoint { int fd; struct sockaddr_ll address; };

static void relay_log(const char *format, ...) __attribute__((format(printf, 1, 2)));
static void close_shared(struct shared_net *net);

static void relay_log(const char *format, ...) {
    char line[512];
    va_list arguments;
    int length;
    va_start(arguments, format);
    length = vsnprintf(line, sizeof(line), format, arguments);
    va_end(arguments);
    if (length <= 0) return;
    if ((size_t)length >= sizeof(line)) length = (int)sizeof(line) - 1;
    (void)write(STDERR_FILENO, line, (size_t)length);
}

static uint32_t le32(const volatile uint8_t *p) { return __atomic_load_n((const volatile uint32_t *)p, __ATOMIC_ACQUIRE); }
static uint64_t le64(const volatile uint8_t *p) { uint64_t v = 0; unsigned int i; for (i = 0; i < 8; i++) v |= (uint64_t)p[i] << (8U * i); return v; }
static void put32(volatile uint8_t *p, uint32_t v) { __atomic_store_n((volatile uint32_t *)p, v, __ATOMIC_RELEASE); }

/* Pairs with RustOS's arm-and-recheck. Publish the producer first, then
 * consume the request for a nudge. An entire RX batch needs at most one kick. */
static int take_rx_wait_arm(volatile uint8_t *base) {
    return __atomic_exchange_n((volatile uint32_t *)(base + RX_WAIT_ARMED), 0U,
                               __ATOMIC_SEQ_CST) == 1U;
}

static unsigned int arm_tx_wait(const struct shared_net *net) {
    __atomic_store_n((volatile uint32_t *)(net->base + TX_WAIT_ARMED), 1U, __ATOMIC_SEQ_CST);
    uint32_t pending = le32(net->base + TX_HEAD) - net->tx_tail;
    return pending <= SLOT_COUNT ? pending : 0;
}

static int read_text_file(const char *path, char *buffer, size_t capacity) {
    ssize_t count; int fd;
    if (capacity < 2U) { errno = EINVAL; return -1; }
    fd = open(path, O_RDONLY | O_CLOEXEC); if (fd < 0) return -1;
    count = read(fd, buffer, capacity - 1U); close(fd);
    if (count <= 0 || (size_t)count >= capacity) { errno = EPROTO; return -1; }
    while (count > 0 && (buffer[count - 1] == '\n' || buffer[count - 1] == '\r' || buffer[count - 1] == ' ' || buffer[count - 1] == '\t')) count--;
    buffer[count] = '\0';
    if (count == 0) { errno = EPROTO; return -1; }
    return 0;
}

static int read_u64_file(const char *path, uint64_t *value) {
    char text[64], *end = NULL; unsigned long long parsed;
    if (read_text_file(path, text, sizeof(text))) return -1;
    errno = 0; parsed = strtoull(text, &end, 0);
    if (errno || end == text || *end != '\0') { errno = EPROTO; return -1; }
    *value = (uint64_t)parsed; return 0;
}

static int find_uio(char *name, size_t name_capacity) {
    DIR *dir = opendir("/sys/class/uio"); struct dirent *entry; char selected[32] = "";
    if (!dir) return -1;
    while ((entry = readdir(dir))) {
        char path[PATH_MAX], value[64];
        if (strncmp(entry->d_name, "uio", 3U) != 0 || entry->d_name[3] == '\0') continue;
        if (snprintf(path, sizeof(path), "/sys/class/uio/%s/name", entry->d_name) >= (int)sizeof(path) || read_text_file(path, value, sizeof(value)) || strcmp(value, UIO_NAME)) continue;
        if (selected[0] != '\0') { closedir(dir); errno = EEXIST; return -1; }
        if (snprintf(selected, sizeof(selected), "%s", entry->d_name) >= (int)sizeof(selected)) { closedir(dir); errno = ENAMETOOLONG; return -1; }
    }
    closedir(dir);
    if (selected[0] == '\0') { errno = ENODEV; return -1; }
    {
        char path[PATH_MAX]; uint64_t bytes;
        if (snprintf(path, sizeof(path), "/sys/class/uio/%s/maps/map0/size", selected) >= (int)sizeof(path) || read_u64_file(path, &bytes) || bytes != APERTURE_BYTES) { errno = EPROTO; return -1; }
    }
    if (snprintf(name, name_capacity, "%s", selected) >= (int)name_capacity) { errno = ENAMETOOLONG; return -1; }
    return 0;
}

static int header_is_valid(const volatile uint8_t *base) {
    uint32_t flags = le32(base + FLAGS), tx_head = le32(base + TX_HEAD), tx_tail = le32(base + TX_TAIL), rx_head = le32(base + RX_HEAD), rx_tail = le32(base + RX_TAIL);
    return memcmp((const void *)base, MAGIC, 8) == 0 && le32(base + 8) == 1U && le32(base + 12) == HEADER_BYTES && le64(base + 16) == APERTURE_BYTES && le32(base + 24) == SLOT_COUNT && le32(base + 28) == SLOT_BYTES && le32(base + 32) == MTU && (flags & FLAG_READY) != 0 && (flags & ~KNOWN_FLAGS) == 0 && le64(base + 56) != 0 && tx_head - tx_tail <= SLOT_COUNT && rx_head - rx_tail <= SLOT_COUNT;
}

static int open_shared(struct shared_net *net) {
    char name[32], path[PATH_MAX], version[16];
    memset(net, 0, sizeof(*net)); net->fd = -1;
    if (find_uio(name, sizeof(name))) return -1;
    if (snprintf(path, sizeof(path), "/sys/class/uio/%s/version", name) >= (int)sizeof(path) ||
        read_text_file(path, version, sizeof(version))) return -1;
    if (strcmp(version, "1") && strcmp(version, "2")) { errno = EPROTO; return -1; }
    net->doorbell = !strcmp(version, "2");
    if (snprintf(path, sizeof(path), "/dev/%s", name) >= (int)sizeof(path)) return -1;
    net->fd = open(path, O_RDWR | O_CLOEXEC); if (net->fd < 0) return -1;
    net->base = mmap(NULL, APERTURE_BYTES, PROT_READ | PROT_WRITE, MAP_SHARED, net->fd, 0);
    if (net->base == MAP_FAILED) { net->base = NULL; close(net->fd); net->fd = -1; return -1; }
    net->bytes = APERTURE_BYTES;
    if (!header_is_valid(net->base)) { errno = EPROTO; close_shared(net); return -1; }
    net->tx_tail = le32(net->base + TX_TAIL); net->rx_head = le32(net->base + RX_HEAD); return 0;
}

static void close_shared(struct shared_net *net) {
    if (net->base && net->base != MAP_FAILED) munmap((void *)net->base, net->bytes);
    if (net->fd >= 0) close(net->fd);
    memset(net, 0, sizeof(*net)); net->fd = -1;
}

static volatile uint8_t *slot(volatile uint8_t *base, uint32_t ring, uint32_t seq) { return base + ring + (seq % SLOT_COUNT) * SLOT_BYTES; }

/* Slot payloads start at offset 4 and are 4-byte aligned. Keep every shared
 * access volatile; the local buffer needs no alignment and can use memcpy.
 * Do not round up: the caller may read or publish only the validated length. */
static void frame_from_shared(uint8_t *dst, const volatile uint8_t *src, size_t len) {
    size_t i = 0;
    for (; len - i >= sizeof(uint32_t); i += sizeof(uint32_t)) {
        uint32_t word = *(const volatile uint32_t *)(src + i);
        memcpy(dst + i, &word, sizeof(word));
    }
    for (; i < len; ++i) dst[i] = src[i];
}

static void frame_to_shared(volatile uint8_t *dst, const uint8_t *src, size_t len) {
    size_t i = 0;
    for (; len - i >= sizeof(uint32_t); i += sizeof(uint32_t)) {
        uint32_t word;
        memcpy(&word, src + i, sizeof(word));
        *(volatile uint32_t *)(dst + i) = word;
    }
    for (; i < len; ++i) dst[i] = src[i];
}

static int interface_flags(const char *name, struct ifreq *request) {
    int fd;
    if (strnlen(name, IFNAMSIZ) >= IFNAMSIZ) { errno = ENAMETOOLONG; return -1; }
    fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    memset(request, 0, sizeof(*request));
    memcpy(request->ifr_name, name, strlen(name) + 1U);
    if (ioctl(fd, SIOCGIFFLAGS, request)) { close(fd); return -1; }
    close(fd); return 0;
}

static int activate_interface(const char *name) {
    int fd; struct ifreq request;
    if (interface_flags(name, &request)) return -1;
    if (request.ifr_flags & IFF_UP) return 0;
    fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    request.ifr_flags |= IFF_UP;
    if (ioctl(fd, SIOCSIFFLAGS, &request)) { close(fd); return -1; }
    close(fd); return 0;
}

/* AF_PACKET observes ingress after GRO. The shared ABI carries one wire
 * frame per slot and deliberately rejects oversized records. */
static int disable_gro(const char *name) {
    struct ifreq request = {0};
    struct ethtool_value gro = { .cmd = ETHTOOL_SGRO, .data = 0 };
    if (strnlen(name, IFNAMSIZ) >= IFNAMSIZ) { errno = ENAMETOOLONG; return -1; }
    memcpy(request.ifr_name, name, strlen(name) + 1U);
    request.ifr_data = (void *)&gro;
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    int result = ioctl(fd, SIOCETHTOOL, &request);
    if (result == 0) {
        gro.cmd = ETHTOOL_GGRO;
        result = ioctl(fd, SIOCETHTOOL, &request);
        if (result == 0 && gro.data != 0) { errno = EPROTO; result = -1; }
    }
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return result;
}

static int raw_socket(struct raw_endpoint *endpoint) {
    int ignore = 1;
    memset(endpoint, 0, sizeof(*endpoint));
    endpoint->fd = socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, htons(ETH_P_ALL));
    if (endpoint->fd < 0) return -1;
    endpoint->address.sll_family = AF_PACKET;
    endpoint->address.sll_protocol = htons(ETH_P_ALL);
    endpoint->address.sll_ifindex = if_nametoindex("eth0");
    if (!endpoint->address.sll_ifindex || bind(endpoint->fd, (struct sockaddr *)&endpoint->address, sizeof(endpoint->address))) { close(endpoint->fd); return -1; }
    if (setsockopt(endpoint->fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &ignore, sizeof(ignore))) {
        int saved_errno = errno;
        close(endpoint->fd);
        errno = saved_errno;
        return -1;
    }
    return 0;
}

static void mark_dvm_ready(struct shared_net *net) {
    (void)__atomic_fetch_or((volatile uint32_t *)(net->base + FLAGS), FLAG_DVM_READY, __ATOMIC_ACQ_REL);
}

static void drain_tx(const struct raw_endpoint *raw, struct shared_net *net) {
    uint32_t head = le32(net->base + TX_HEAD);
    while (head - net->tx_tail <= SLOT_COUNT && head != net->tx_tail) {
        volatile uint8_t *s = slot(net->base, TX_RING, net->tx_tail); uint32_t len = le32(s);
        uint8_t frame[MTU]; ssize_t sent;
        if (len == 0 || len > MTU) {
            /* A malformed length must not wedge the whole TX ring: the slot is
             * already published, so drop it and advance past it. */
            net->tx_tail++; put32(net->base + TX_TAIL, net->tx_tail);
            continue;
        }
        frame_from_shared(frame, s + 4U, len);
        sent = sendto(raw->fd, frame, len, MSG_DONTWAIT, (const struct sockaddr *)&raw->address, sizeof(raw->address));
        if (sent != (ssize_t)len) {
            int tx_errno = sent < 0 ? errno : EIO;
            if (tx_errno != EAGAIN && tx_errno != EWOULDBLOCK && tx_errno != net->last_tx_errno) {
                struct ifreq state;
                unsigned int flags = interface_flags("eth0", &state) ? 0U : (unsigned int)state.ifr_flags;
                relay_log("rustos-dvm-net: transmit paused errno=%d flags=0x%x\n", tx_errno, flags);
            }
            net->last_tx_errno = tx_errno;
            return;
        }
        net->last_tx_errno = 0; net->tx_tail++; put32(net->base + TX_TAIL, net->tx_tail);
    }
}

static ssize_t receive_frame(int fd, uint8_t *frame, size_t capacity) {
    /* MSG_TRUNC reports the original datagram length instead of silently
     * forwarding an oversized Ethernet frame with its tail cut off. */
    return recv(fd, frame, capacity, MSG_DONTWAIT | MSG_TRUNC);
}

static void drain_rx(const struct raw_endpoint *raw, struct shared_net *net) {
    uint8_t frame[MTU];
    for (unsigned int budget = 0; budget < SLOT_COUNT; ++budget) {
        uint32_t tail = le32(net->base + RX_TAIL);
        if (net->rx_head - tail >= SLOT_COUNT) return;
        ssize_t len = receive_frame(raw->fd, frame, sizeof(frame));
        if (len < 0) return;
        if (len == 0 || (size_t)len > sizeof(frame)) continue;
        volatile uint8_t *s = slot(net->base, RX_RING, net->rx_head);
        frame_to_shared(s + 4U, frame, (size_t)len);
        put32(s, (uint32_t)len);
        net->rx_head++; put32(net->base + RX_HEAD, net->rx_head);
    }
}

static int relay_loop(struct shared_net *net, const struct raw_endpoint *raw) {
    int logged_irq = 0;
    unsigned int warm_turns = 0;
    uint32_t last_rx = net->rx_head;
    for (;;) {
        uint32_t previous_tx = net->tx_tail;
        drain_tx(raw, net);
        if (net->tx_tail != previous_tx || net->rx_head != last_rx)
            warm_turns = ACTIVE_POLL_TURNS;
        last_rx = net->rx_head;
        int timeout = TX_RETRY_MS;
        if (warm_turns != 0) {
            /* Batch a live burst without an interrupt for each little refill. */
            --warm_turns;
        } else if (net->doorbell) {
            unsigned int pending = arm_tx_wait(net);
            timeout = pending ? (net->last_tx_errno ? TX_RETRY_MS : 0) : RELAY_WATCHDOG_MS;
            if (pending) put32(net->base + TX_WAIT_ARMED, 0);
        } else {
            timeout = RELAY_WATCHDOG_MS;
        }
        /* If the RX ring is full, do not consume a frame or busy-loop on
         * the readable raw socket. Recheck credits at the bounded timeout. */
        int raw_fd = net->rx_head - le32(net->base + RX_TAIL) < SLOT_COUNT ? raw->fd : -1;
        if (raw_fd < 0 && timeout > TX_RETRY_MS) timeout = TX_RETRY_MS;
        struct pollfd p[2] = {{.fd = raw_fd, .events = POLLIN},
                              {.fd = net->doorbell ? net->fd : -1, .events = POLLIN}};
        int result = poll(p, 2, timeout);
        if (net->doorbell) put32(net->base + TX_WAIT_ARMED, 0);
        if (result < 0 && errno == EINTR) continue;
        if (result < 0 || ((p[0].revents | p[1].revents) & (POLLERR | POLLHUP | POLLNVAL))) return -1;
        if (p[1].revents & POLLIN) {
            uint32_t events;
            if (read(net->fd, &events, sizeof(events)) != sizeof(events)) return -1;
            if (!logged_irq) {
                relay_log("rustos-dvm-net: TX doorbell interrupt observed\n");
                logged_irq = 1;
            }
        }
        if (p[0].revents & POLLIN) {
            uint32_t before = net->rx_head;
            drain_rx(raw, net);
            if (net->doorbell && before != net->rx_head && take_rx_wait_arm(net->base)) {
                int32_t kick = 1;
                if (write(net->fd, &kick, sizeof(kick)) != sizeof(kick)) return -1;
            }
        }
    }
}

int main(int argc, char **argv) {
    if (argc != 2 || strcmp(argv[1], "serve")) { fprintf(stderr, "usage: %s serve\n", argv[0]); return EXIT_FAILURE; }
    for (;;) {
        struct shared_net net; struct raw_endpoint raw;
        if (open_shared(&net) || activate_interface("eth0")) { close_shared(&net); sleep(1); continue; }
        if (disable_gro("eth0")) {
            relay_log("rustos-dvm-net: cannot disable ingress GRO errno=%d\n", errno);
            close_shared(&net);
            sleep(1);
            continue;
        }
        if (raw_socket(&raw)) { close_shared(&net); sleep(1); continue; }
        struct ifreq state;
        mark_dvm_ready(&net);
        relay_log("rustos-dvm-net: active interface=eth0 mtu=%u slots=%u flags=0x%x gro=off notification=%s\n",
                  MTU, SLOT_COUNT, interface_flags("eth0", &state) ? 0U : (unsigned int)state.ifr_flags,
                  net.doorbell ? "msix" : "poll");
        (void)relay_loop(&net, &raw);
        relay_log("rustos-dvm-net: relay wait failed errno=%d\n", errno);
        if (net.doorbell) put32(net.base + TX_WAIT_ARMED, 0);
        (void)__atomic_fetch_and((volatile uint32_t *)(net.base + FLAGS), ~FLAG_DVM_READY, __ATOMIC_ACQ_REL);
        close(raw.fd);
        close_shared(&net);
        sleep(1);
    }
}
