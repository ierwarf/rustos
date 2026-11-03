// SPDX-License-Identifier: MIT
// Build separately on Linux; exercise the receive path without a DVM.
#define main rustos_dvm_net_main
#include "rustos-dvm-net.c"
#undef main
#include <assert.h>

int main(void) {
    int sockets[2];
    uint8_t *memory = calloc(1, APERTURE_BYTES);
    uint8_t oversized[MTU + 1U] = {0};
    const uint8_t valid[] = {1, 2, 3};
    struct shared_net net = {.fd = -1, .base = memory, .bytes = APERTURE_BYTES};
    struct raw_endpoint raw = {0};

    assert(memory);
    memcpy(memory, MAGIC, 8);
    put32(memory + 8, 1);
    put32(memory + 12, HEADER_BYTES);
    put32(memory + 16, APERTURE_BYTES);
    put32(memory + 24, SLOT_COUNT);
    put32(memory + 28, SLOT_BYTES);
    put32(memory + 32, MTU);
    put32(memory + FLAGS, FLAG_READY);
    memory[56] = 1;
    put32(memory + TX_HEAD, 1);
    put32(memory + TX_TAIL, UINT32_MAX);
    assert(header_is_valid(memory));
    put32(memory + TX_HEAD, 64);
    assert(!header_is_valid(memory));
    put32(memory + TX_HEAD, 0);
    put32(memory + TX_TAIL, 0);
    assert(socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) == 0);
    raw.fd = sockets[1];
    assert(send(sockets[0], oversized, sizeof(oversized), 0) == (ssize_t)sizeof(oversized));
    assert(send(sockets[0], valid, sizeof(valid), 0) == (ssize_t)sizeof(valid));
    drain_rx(&raw, &net);
    assert(net.rx_head == 1);
    assert(le32(memory + RX_HEAD) == 1);
    assert(le32(memory + RX_RING) == sizeof(valid));
    assert(memcmp(memory + RX_RING + 4, valid, sizeof(valid)) == 0);
    close(sockets[0]);
    close(sockets[1]);
    free(memory);
    return 0;
}
