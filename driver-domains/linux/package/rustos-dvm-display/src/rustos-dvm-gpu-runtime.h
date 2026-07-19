// SPDX-License-Identifier: MIT
#ifndef RUSTOS_DVM_GPU_RUNTIME_H
#define RUSTOS_DVM_GPU_RUNTIME_H

#include <stddef.h>
#include <stdint.h>

struct rustos_gpu_runtime;

#define RUSTOS_GPU_PIPELINE_PRIME_BUDGET_US 500000U

struct rustos_gpu_damage {
    uint32_t x;
    uint32_t y;
    uint32_t width;
    uint32_t height;
};

struct rustos_gpu_frame {
    uint32_t framebuffer_id;
    uint32_t output_index;
    uint32_t source_slot;
    int in_fence_fd;
    uint32_t context_id;
    uint32_t context_epoch;
    uint64_t submit_value;
    uint64_t generation;
    uint64_t sequence;
    uint64_t render_started_ns;
    uint32_t budget_us;
};

int rustos_gpu_runtime_open(int drm_fd, uint32_t output_width, uint32_t output_height,
                            uint32_t atlas_width, uint32_t atlas_height,
                            uint32_t atlas_stride_bytes,
                            struct rustos_gpu_runtime **runtime_out);
int rustos_gpu_runtime_import_dmabuf_sources(struct rustos_gpu_runtime *runtime,
                                             const int *source_fds,
                                             size_t source_count);
int rustos_gpu_runtime_render_prime(struct rustos_gpu_runtime *runtime,
                                    struct rustos_gpu_frame *frame);
int rustos_gpu_runtime_render_batch(struct rustos_gpu_runtime *runtime,
                                    const uint8_t *atlas_pixels, size_t atlas_bytes,
                                    const struct rustos_gpu_damage *damage,
                                    uint32_t damage_count,
                                    const uint8_t *batch, size_t batch_bytes,
                                    uint32_t binding_slot, uint64_t generation,
                                    uint64_t sequence, int source_acquire_fence_fd,
                                    struct rustos_gpu_frame *frame);
void rustos_gpu_runtime_presented(struct rustos_gpu_runtime *runtime,
                                  uint32_t output_index);
void rustos_gpu_runtime_close(struct rustos_gpu_runtime *runtime);
const char *rustos_gpu_runtime_driver(const struct rustos_gpu_runtime *runtime);
const char *rustos_gpu_runtime_renderer(const struct rustos_gpu_runtime *runtime);
const char *rustos_gpu_runtime_stage(const struct rustos_gpu_runtime *runtime);
int rustos_gpu_runtime_uses_dmabuf_sources(const struct rustos_gpu_runtime *runtime);

#endif
