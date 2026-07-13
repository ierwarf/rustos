// SPDX-License-Identifier: MIT
// Linux DVM raw Ethernet relay for the fixed RustOS two-ring ivshmem contract.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/if_packet.h>
#include <limits.h>
#include <net/if.h>
#include <netinet/ether.h>
#include <poll.h>
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
#define RECORD_BYTES 64U
#define SLOT_COUNT 64U
#define SLOT_BYTES 2048U
#define MTU 1514U
#define FLAG_READY 1U
#define FLAG_DVM_READY 2U
#define KNOWN_FLAGS (FLAG_READY | FLAG_DVM_READY)
#define REGION_BYTES (HEADER_BYTES + 2U * SLOT_COUNT * SLOT_BYTES)
#define FLAGS 36U
#define TX_HEAD 40U
#define TX_TAIL 44U
#define RX_HEAD 48U
#define RX_TAIL 52U
#define TX_RING HEADER_BYTES
#define RX_RING (HEADER_BYTES + SLOT_COUNT * SLOT_BYTES)

struct shared_net { int fd; volatile uint8_t *base; size_t bytes; uint32_t tx_tail; uint32_t rx_head; int last_tx_errno; };
struct raw_endpoint { int fd; struct sockaddr_ll address; };

static uint32_t le32(const volatile uint8_t *p) { return __atomic_load_n((const volatile uint32_t *)p, __ATOMIC_ACQUIRE); }
static uint64_t le64(const volatile uint8_t *p) { uint64_t v = 0; unsigned int i; for (i = 0; i < 8; i++) v |= (uint64_t)p[i] << (8U * i); return v; }
static void put32(volatile uint8_t *p, uint32_t v) { __atomic_store_n((volatile uint32_t *)p, v, __ATOMIC_RELEASE); }

static int matching_bar(char *out, size_t out_len, size_t *bytes) {
    DIR *dir = opendir("/sys/bus/pci/devices"); struct dirent *entry;
    if (!dir) return -1;
    while ((entry = readdir(dir))) {
        char base[64], vendor[80], device[80], resource[80], value[32]; FILE *f; unsigned long long start, end, flags; unsigned int line;
        if (entry->d_name[0] == '.' || strnlen(entry->d_name, 16U) == 16U) continue;
        snprintf(base, sizeof(base), "/sys/bus/pci/devices/%.15s", entry->d_name);
        snprintf(vendor, sizeof(vendor), "%s/vendor", base); f = fopen(vendor, "re"); if (!f || !fgets(value, sizeof(value), f)) { if (f) fclose(f); continue; } fclose(f);
        if (strncmp(value, "0x1af4", 6) != 0) continue;
        snprintf(device, sizeof(device), "%s/device", base); f = fopen(device, "re"); if (!f || !fgets(value, sizeof(value), f)) { if (f) fclose(f); continue; } fclose(f);
        if (strncmp(value, "0x1110", 6) != 0) continue;
        snprintf(resource, sizeof(resource), "%s/resource", base); f = fopen(resource, "re"); if (!f) continue;
        for (line = 0; line <= 2U; line++) if (fscanf(f, "%llx %llx %llx", &start, &end, &flags) != 3) break;
        fclose(f);
        if (line != 3U || end < start || end - start + 1U < RECORD_BYTES) continue;
        snprintf(resource, sizeof(resource), "%s/resource2", base);
        int fd = open(resource, O_RDONLY | O_CLOEXEC); volatile uint8_t *map;
        if (fd < 0) continue;
        map = mmap(NULL, (size_t)(end - start + 1U), PROT_READ, MAP_SHARED, fd, 0);
        close(fd);
        if (map == MAP_FAILED) continue;
        uint32_t transport_flags = le32(map + FLAGS);
        int match = memcmp((const void *)map, MAGIC, 8) == 0 && le32(map + 8) == 1U && le32(map + 12) == HEADER_BYTES && le64(map + 16) >= REGION_BYTES && (transport_flags & FLAG_READY) != 0 && (transport_flags & ~KNOWN_FLAGS) == 0;
        munmap((void *)map, (size_t)(end - start + 1U));
        if (!match) continue;
        if (snprintf(out, out_len, "%s/resource2", base) >= (int)out_len) continue;
        *bytes = (size_t)(end - start + 1U); closedir(dir); return 0;
    }
    closedir(dir); errno = ENODEV; return -1;
}

static int open_shared(struct shared_net *net) {
    char path[80]; size_t bytes;
    memset(net, 0, sizeof(*net)); net->fd = -1;
    if (matching_bar(path, sizeof(path), &bytes) || bytes < REGION_BYTES) return -1;
    net->fd = open(path, O_RDWR | O_CLOEXEC); if (net->fd < 0) return -1;
    net->base = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, net->fd, 0);
    if (net->base == MAP_FAILED) { close(net->fd); return -1; }
    net->bytes = bytes; net->tx_tail = le32(net->base + TX_TAIL); net->rx_head = le32(net->base + RX_HEAD); return 0;
}

static void close_shared(struct shared_net *net) {
    if (net->base && net->base != MAP_FAILED) munmap((void *)net->base, net->bytes);
    if (net->fd >= 0) close(net->fd);
    memset(net, 0, sizeof(*net)); net->fd = -1;
}

static volatile uint8_t *slot(volatile uint8_t *base, uint32_t ring, uint32_t seq) { return base + ring + (seq % SLOT_COUNT) * SLOT_BYTES; }

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

static int raw_socket(struct raw_endpoint *endpoint) {
    int ignore = 1;
    memset(endpoint, 0, sizeof(*endpoint));
    endpoint->fd = socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, htons(ETH_P_ALL));
    if (endpoint->fd < 0) return -1;
    endpoint->address.sll_family = AF_PACKET;
    endpoint->address.sll_protocol = htons(ETH_P_ALL);
    endpoint->address.sll_ifindex = if_nametoindex("eth0");
    if (!endpoint->address.sll_ifindex || bind(endpoint->fd, (struct sockaddr *)&endpoint->address, sizeof(endpoint->address))) { close(endpoint->fd); return -1; }
    (void)setsockopt(endpoint->fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &ignore, sizeof(ignore)); return 0;
}

static void mark_dvm_ready(struct shared_net *net) {
    (void)__atomic_fetch_or((volatile uint32_t *)(net->base + FLAGS), FLAG_DVM_READY, __ATOMIC_RELEASE);
}

static void drain_tx(const struct raw_endpoint *raw, struct shared_net *net) {
    uint32_t head = le32(net->base + TX_HEAD);
    __sync_synchronize();
    while (head - net->tx_tail <= SLOT_COUNT && head != net->tx_tail) {
        volatile uint8_t *s = slot(net->base, TX_RING, net->tx_tail); uint32_t len = le32(s);
        if (len == 0 || len > MTU) break;
        uint8_t frame[MTU]; unsigned int i; ssize_t sent;
        for (i = 0; i < len; i++) frame[i] = s[4U + i];
        sent = sendto(raw->fd, frame, len, MSG_DONTWAIT, (const struct sockaddr *)&raw->address, sizeof(raw->address));
        if (sent != (ssize_t)len) {
            int tx_errno = sent < 0 ? errno : EIO;
            if (tx_errno != EAGAIN && tx_errno != EWOULDBLOCK && tx_errno != net->last_tx_errno) {
                struct ifreq state;
                unsigned int flags = interface_flags("eth0", &state) ? 0U : (unsigned int)state.ifr_flags;
                fprintf(stderr, "rustos-dvm-net: transmit paused errno=%d flags=0x%x\n", tx_errno, flags);
            }
            net->last_tx_errno = tx_errno;
            return;
        }
        net->last_tx_errno = 0; net->tx_tail++; __sync_synchronize(); put32(net->base + TX_TAIL, net->tx_tail);
    }
}

static void drain_rx(const struct raw_endpoint *raw, struct shared_net *net) {
    uint8_t frame[MTU];
    for (;;) {
        ssize_t len = recv(raw->fd, frame, sizeof(frame), MSG_DONTWAIT); uint32_t tail;
        if (len < 0) { if (errno == EAGAIN || errno == EWOULDBLOCK) return; return; }
        if (len == 0) continue;
        tail = le32(net->base + RX_TAIL);
        if (net->rx_head - tail > SLOT_COUNT || net->rx_head - tail == SLOT_COUNT) return;
        volatile uint8_t *s = slot(net->base, RX_RING, net->rx_head); unsigned int i; for (i = 0; i < (unsigned int)len; i++) s[4U + i] = frame[i]; put32(s, (uint32_t)len);
        __sync_synchronize(); net->rx_head++; put32(net->base + RX_HEAD, net->rx_head);
    }
}

int main(int argc, char **argv) {
    if (argc != 2 || strcmp(argv[1], "serve")) { fprintf(stderr, "usage: %s serve\n", argv[0]); return EXIT_FAILURE; }
    for (;;) {
        struct shared_net net; struct raw_endpoint raw;
        if (open_shared(&net) || activate_interface("eth0") || raw_socket(&raw)) { close_shared(&net); sleep(1); continue; }
        struct ifreq state;
        mark_dvm_ready(&net);
        fprintf(stderr, "rustos-dvm-net: active interface=eth0 mtu=%u slots=%u flags=0x%x\n", MTU, SLOT_COUNT, interface_flags("eth0", &state) ? 0U : (unsigned int)state.ifr_flags); fflush(stderr);
        for (;;) { struct pollfd p = {.fd = raw.fd, .events = POLLIN}; drain_tx(&raw, &net); (void)poll(&p, 1, 20); if (p.revents) drain_rx(&raw, &net); }
    }
}
