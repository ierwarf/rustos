#ifndef RUSTOS_DVM_GPU_BACKENDS_H
#define RUSTOS_DVM_GPU_BACKENDS_H

#include <stddef.h>
#include <stdint.h>
#include <string.h>

/*
 * GPU mechanism and GPU certification are deliberately separate.  The frame
 * executor consumes one common fixed command stream; this registry is the
 * only place that admits a DRM driver and binds it to a transport class.
 * Adding a driver requires format/modifier, fence, KMS, reset and recovery
 * evidence before adding an entry here.
 */
#define RUSTOS_GPU_SOURCE_STAGED_COPY 1U
#define RUSTOS_GPU_SOURCE_DIRECT_DMABUF 2U

#define RUSTOS_GPU_BACKEND_VIRTUAL_STAGED "virtual-staged"
#define RUSTOS_GPU_BACKEND_PHYSICAL_DIRECT "physical-direct"

struct rustos_gpu_backend_policy {
    const char *drm_driver;
    const char *backend_class;
    uint32_t source_mode;
    const char *renderer_token_a;
    const char *renderer_token_b;
};

static const struct rustos_gpu_backend_policy RUSTOS_GPU_BACKENDS[] = {
    {"virtio_gpu", RUSTOS_GPU_BACKEND_VIRTUAL_STAGED,
     RUSTOS_GPU_SOURCE_STAGED_COPY, "virgl", NULL},
    {"amdgpu", RUSTOS_GPU_BACKEND_PHYSICAL_DIRECT,
     RUSTOS_GPU_SOURCE_DIRECT_DMABUF, "amd", "radeon"},
};

static inline const struct rustos_gpu_backend_policy *
rustos_gpu_backend_policy(const char *driver) {
    size_t index;
    if (driver == NULL || driver[0] == '\0')
        return NULL;
    for (index = 0U;
         index < sizeof(RUSTOS_GPU_BACKENDS) / sizeof(RUSTOS_GPU_BACKENDS[0]);
         index++) {
        if (strcmp(driver, RUSTOS_GPU_BACKENDS[index].drm_driver) == 0)
            return &RUSTOS_GPU_BACKENDS[index];
    }
    return NULL;
}

#endif
