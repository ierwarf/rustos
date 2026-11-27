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
    const uint8_t sample[] = {1, 3, 5, 7, 9, 11, 13, 15};
    uint8_t copy[sizeof(sample) + 2];
    for (size_t len = 0; len <= sizeof(sample); ++len) {
        memset(memory + TX_RING, 0xa5, 16);
        memset(copy, 0xa5, sizeof(copy));
        frame_to_shared(memory + TX_RING + 4, sample, len);
        frame_from_shared(copy + 1, memory + TX_RING + 4, len);
        assert(memcmp(copy + 1, sample, len) == 0);
        assert(copy[0] == 0xa5 && copy[len + 1] == 0xa5);
        assert(memory[TX_RING + 3] == 0xa5 && memory[TX_RING + 4 + len] == 0xa5);
    }
    assert(arm_tx_wait(&net) == 0);
    assert(le32(memory + TX_WAIT_ARMED) == 1);
    put32(memory + TX_HEAD, 1);
    assert(arm_tx_wait(&net) == 1);
    put32(memory + TX_HEAD, SLOT_COUNT + 1);
    assert(arm_tx_wait(&net) == 0);
    put32(memory + TX_HEAD, 0);
    assert(!take_rx_wait_arm(memory));
    put32(memory + RX_WAIT_ARMED, 1);
    assert(take_rx_wait_arm(memory));
    assert(!take_rx_wait_arm(memory));
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
