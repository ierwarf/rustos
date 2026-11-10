// SPDX-License-Identifier: MIT
#ifndef RUSTOS_DVM_GPU_DEADLINE_H
#define RUSTOS_DVM_GPU_DEADLINE_H
#include <stdint.h>
#include <errno.h>

static inline int gpu_deadline_after(uint64_t start, uint64_t duration, uint64_t *deadline)
{
    if (!start || !duration || !deadline || UINT64_MAX - start < duration) {
        errno = EOVERFLOW;
        return -1;
    }
    *deadline = start + duration;
    return 0;
}

static inline int gpu_deadline_remaining(uint64_t now, uint64_t deadline, uint64_t *remaining)
{
    if (!now || now >= deadline) {
        errno = ETIMEDOUT;
        return -1;
    }
    *remaining = deadline - now;
    return 0;
}

static inline uint64_t gpu_deadline_min(uint64_t a, uint64_t b)
{
    return a < b ? a : b;
}
#endif
