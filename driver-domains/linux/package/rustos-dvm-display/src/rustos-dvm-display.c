// SPDX-License-Identifier: MIT
// DVM-owned DRM/KMS consumer for the fixed RustOS ivshmem display contract.
//
// This process deliberately has no host-control, input, or device-management
// protocol. It reads only module-validated command batches and cacheable,
// read-only atlas pixels, executes the bounded GPU vocabulary, and presents
// only GPU-completed output buffers through atomic DRM/KMS.

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#include "rustos-dvm-gpu-runtime.h"
#include "rustos-dvm-gpu-backends.h"

#define IVSHMEM_VENDOR_ID "0x1af4"
#define IVSHMEM_DEVICE_ID "0x1110"
#define IVSHMEM_RESOURCE_INDEX 2U
#define RUSTOS_IVSHMEM_UIO_NAME "rustos-dvm-ivshmem-uio"
#define RUSTOS_DVM_HOST_INVITED_ATTRIBUTE "rustos_dvm_host_invited"
#define RUSTOS_DVM_DISPLAY_READY_ATTRIBUTE "rustos_dvm_display_ready"
#define RUSTOS_DVM_DISPLAY_OFFLINE_ATTRIBUTE "rustos_dvm_display_offline"
#define RUSTOS_DVM_GPU_PRIME_ATTRIBUTE "rustos_dvm_gpu_prime"
#define RUSTOS_DVM_GPU_COMPLETION_ATTRIBUTE "rustos_dvm_gpu_completion"
#define RUSTOS_DVM_STATE_DIR "/run/rustos-dvm"
#define RUSTOS_DVM_DISPLAY_READY_NAME "display-ready.lock"
#define RUSTOS_DVM_DISPLAY_OWNER_NAME "display-owner.lock"
#define RUSTOS_DVM_DISPLAY_READY_CANDIDATE ".display-ready.next"
#define RUSTOS_DVM_DISPLAY_EVIDENCE "/run/rustos-dvm/display-evidence-v2.env"
#define RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP "/run/rustos-dvm/display-evidence-v2.env.tmp"
#define GUI_POOL_MAGIC "RSGUI002"
#define GUI_POOL_VERSION 2U
#define GUI_POOL_HEADER_BYTES 4096U
#define GUI_POOL_SLOT_COUNT 3U
#define GUI_POOL_INVITATION_OFFSET 336U
#define GUI_POOL_READY_CONFIRMATION_OFFSET 352U
#define GPU_ATLAS_HEADER_OFFSET 512U
#define GPU_ATLAS_HEADER_BYTES 64U
#define GPU_ATLAS_MAGIC "RSGPUA01"
#define GPU_ATLAS_VERSION 3U
#define GPU_ATLAS_SUBMIT_MAGIC "RSGPUQ01"
#define GPU_ATLAS_SUBMIT_BYTES 64U
#define GPU_ATLAS_DAMAGE_BYTES 16U
#define GPU_ATLAS_MAX_DAMAGE_RECTS 64U
#define GPU_ATLAS_COMMAND_SLOT_BYTES (36U * 1024U)
#define GPU_ATLAS_INVITATION_OFFSET 2048U
#define GPU_ATLAS_COMPLETION_ACK_OFFSET 2112U
#define GPU_ATLAS_COMPLETION_SEQUENCE_OFFSET 2080U
#define GPU_ATLAS_COMPLETION_BYTES 256U
#define GPU_ATLAS_PRIME_COMPLETION_OFFSET 1792U
#define GPU_ATLAS_CONTEXT_ID_OFFSET 2144U
#define GPU_ATLAS_CONTEXT_EPOCH_OFFSET 2148U
#define GPU_ATLAS_PRIME_FENCE_OFFSET 2152U
#define GPU_ATLAS_SLOT_COUNT 3U
#define GPU_ATLAS_COMPLETION_MAGIC "RSGPUC01"
#define GPU_RENDER_COMPLETION_MAGIC "RSGPUD01"
#define GPU_PRESENT_COMPLETION_MAGIC "RSGPUF01"
#define GPU_PRIME_COMPLETION_MAGIC "RSGPUP01"
#define GPU_PRIME_COMPLETION_BYTES 64U
#define GPU_PIPELINE_PRIME_MAX_NS \
    ((uint64_t)RUSTOS_GPU_PIPELINE_PRIME_BUDGET_US * 1000ULL)
#define GPU_RENDER_VERSION 1U
#define GPU_PRIME_COMPLETION_VERSION 2U
#define GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY 1U
#define GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF 2U
#define SCANOUT_BUFFER_COUNT 3U
#define NO_SCANOUT_BUFFER UINT32_MAX
#define RUSTOS_DVM_DMABUF_DEVICE "/dev/rustos-dvm-display-dmabuf"
#define RUSTOS_DVM_DMABUF_EXPORT_ATLAS 1U

enum gpu_source_mode {
    GPU_SOURCE_STAGED_COPY = RUSTOS_GPU_SOURCE_STAGED_COPY,
    GPU_SOURCE_DIRECT_DMABUF = RUSTOS_GPU_SOURCE_DIRECT_DMABUF,
};

_Static_assert(GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY == RUSTOS_GPU_SOURCE_STAGED_COPY,
               "staged-copy source mode contract drift");
_Static_assert(GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF == RUSTOS_GPU_SOURCE_DIRECT_DMABUF,
               "direct-DMA-BUF source mode contract drift");

struct rustos_dvm_dmabuf_request {
    uint32_t slot;
    uint32_t flags;
};

struct rustos_dvm_acquire_request {
    uint32_t slot;
    uint32_t reserved;
    uint64_t generation;
    uint64_t sequence;
    uint64_t acquire_value;
};

#define RUSTOS_DVM_DMABUF_IOCTL_EXPORT \
    _IOW('R', 0x41, struct rustos_dvm_dmabuf_request)
#define RUSTOS_DVM_DMABUF_IOCTL_ACQUIRE \
    _IOW('R', 0x42, struct rustos_dvm_acquire_request)

/*
 * Multiple DVM services share the serial tty. stdio may split one formatted
 * line into several writes, allowing another process to splice bytes into a
 * machine-checked readiness or timing record. Emit every relay record through
 * one bounded write so log corruption cannot create a false failure/success.
 */
static void relay_log(const char *format, ...) __attribute__((format(printf, 1, 2)));
static int write_complete(int fd, const void *buffer, size_t length);

static void relay_log(const char *format, ...) {
    char line[1024];
    va_list arguments;
    int length;

    va_start(arguments, format);
    length = vsnprintf(line, sizeof(line), format, arguments);
    va_end(arguments);
    if (length <= 0) {
        return;
    }
    if ((size_t)length >= sizeof(line)) {
        length = (int)sizeof(line) - 1;
    }
    (void)write(STDERR_FILENO, line, (size_t)length);
}
#define DISPLAY_BYTES_PER_PIXEL 4U
#define GUI_POOL_MAX_REGION_BYTES (128U * 1024U * 1024U)
#define DISPLAY_STATS_INTERVAL_NS (1000ULL * 1000ULL * 1000ULL)
#define GPU_FRAME_LOG_INTERVAL 120U
#define DISPLAY_RELAY_RR_PRIORITY 9
#define DISPLAY_RELAY_RTTIME_SOFT_US 50000U
#define DISPLAY_RELAY_RTTIME_HARD_US 100000U
#define DISPLAY_SERVE_RETRY (-1)
#define DISPLAY_SERVE_FATAL (-2)

struct display_scheduler_guard {
    int active;
    int fatal;
    int saved_policy;
    struct sched_param saved_param;
    struct rlimit saved_rttime;
};

static int display_scheduler_leave(struct display_scheduler_guard *guard) {
    struct sched_param observed_param;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno = 0;

    if (!guard->active)
        return guard->fatal ? -1 : 0;
    if (sched_setscheduler(0, guard->saved_policy, &guard->saved_param) != 0) {
        saved_errno = errno;
    }
    if (setrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0 && saved_errno == 0) {
        saved_errno = errno;
    }
    guard->active = 0;
    observed_policy = sched_getscheduler(0);
    if ((observed_policy != guard->saved_policy || sched_getparam(0, &observed_param) != 0 ||
         observed_param.sched_priority != guard->saved_param.sched_priority ||
         getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
         observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur ||
         observed_rttime.rlim_max != guard->saved_rttime.rlim_max) &&
        saved_errno == 0) {
        saved_errno = errno != 0 ? errno : EINVAL;
    }
    if (saved_errno != 0) {
        guard->fatal = 1;
        errno = saved_errno;
        return -1;
    }
    return 0;
}

/*
 * The authenticated display relay is the only latency-critical thread in this
 * process. Admit it below the input relay's priority and bound continuous CPU
 * time so a wedged Mesa/DRM path cannot starve DVM recovery or control work.
 * Linux terminates the process at the hard limit; ordinary exits restore the
 * saved scheduler policy through display_scheduler_leave().
 */
static int display_scheduler_enter(struct display_scheduler_guard *guard) {
    struct rlimit bounded_rttime = {
        .rlim_cur = DISPLAY_RELAY_RTTIME_SOFT_US,
        .rlim_max = DISPLAY_RELAY_RTTIME_HARD_US,
    };
    struct sched_param realtime = {.sched_priority = DISPLAY_RELAY_RR_PRIORITY};
    struct sched_param observed;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno;

    memset(guard, 0, sizeof(*guard));
    guard->saved_policy = sched_getscheduler(0);
    if (guard->saved_policy < 0 || sched_getparam(0, &guard->saved_param) != 0 ||
        getrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0)
        return -1;
    if (guard->saved_policy != SCHED_OTHER || guard->saved_param.sched_priority != 0) {
        errno = EINVAL;
        return -1;
    }
    if (setrlimit(RLIMIT_RTTIME, &bounded_rttime) != 0)
        return -1;
    if (getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
        observed_rttime.rlim_cur != bounded_rttime.rlim_cur ||
        observed_rttime.rlim_max != bounded_rttime.rlim_max) {
        saved_errno = errno != 0 ? errno : EINVAL;
        if (setrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0)
            guard->fatal = 1;
        errno = saved_errno;
        return -1;
    }
    if (sched_setscheduler(0, SCHED_RR, &realtime) != 0) {
        saved_errno = errno;
        if (setrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0)
            guard->fatal = 1;
        errno = saved_errno;
        return -1;
    }
    guard->active = 1;
    observed_policy = sched_getscheduler(0);
    if (observed_policy != SCHED_RR || sched_getparam(0, &observed) != 0 ||
        observed.sched_priority != DISPLAY_RELAY_RR_PRIORITY ||
        getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
        observed_rttime.rlim_cur != bounded_rttime.rlim_cur ||
        observed_rttime.rlim_max != bounded_rttime.rlim_max) {
        saved_errno = errno != 0 ? errno : EINVAL;
        (void)display_scheduler_leave(guard);
        errno = saved_errno;
        return -1;
    }
    return 0;
}
#define DISPLAY_PAGEFLIP_TIMEOUT_MS 100
#define DISPLAY_RETRY_SECONDS 1U
#define UIO_BIND_RETRIES 50U
#define UIO_BIND_RETRY_NS (20ULL * 1000ULL * 1000ULL)

struct gui_pool_header {
    uint64_t region_bytes;
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
    uint64_t slot_bytes;
    uint32_t flags;
};

struct gpu_relay_metrics {
    uint64_t render_time_ns;
    uint64_t render_measurements;
    uint64_t last_render_time_ns;
    uint64_t last_render_measurements;
    uint64_t render_max_ns;
};

struct gpu_atlas_header {
    uint64_t region_bytes;
    uint64_t command_offset;
    uint64_t atlas_offset;
    uint64_t atlas_slot_bytes;
    uint32_t atlas_width;
    uint32_t atlas_height;
    uint32_t atlas_stride_bytes;
    uint32_t flags;
};

struct shared_display {
    int fd;
    int uio_fd;
    volatile uint8_t *base;
    const uint8_t *pixels;
    size_t bytes;
    size_t pixel_bytes;
    char pci_bdf[32];
    struct gui_pool_header header;
    struct gpu_atlas_header atlas;
};

struct atomic_property_ids {
    uint32_t connector_crtc_id;
    uint32_t crtc_mode_id;
    uint32_t crtc_active;
    uint32_t plane_fb_id;
    uint32_t plane_crtc_id;
    uint32_t plane_src_x;
    uint32_t plane_src_y;
    uint32_t plane_src_w;
    uint32_t plane_src_h;
    uint32_t plane_crtc_x;
    uint32_t plane_crtc_y;
    uint32_t plane_crtc_w;
    uint32_t plane_crtc_h;
    uint32_t plane_fb_damage_clips;
    uint32_t plane_in_fence_fd;
    uint32_t crtc_out_fence_ptr;
};

struct kms_display {
    int fd;
    int source_exporter_fd;
    const char *setup_stage;
    uint32_t connector_id;
    uint32_t crtc_id;
    uint32_t primary_plane_id;
    uint32_t mode_blob_id;
    drmModeModeInfo mode;
    struct atomic_property_ids properties;
    uint32_t front_buffer;
    uint32_t source_width;
    uint32_t source_height;
    uint64_t pageflip_latency_time_ns;
    uint64_t pageflip_latency_max_ns;
};

static int parse_gui_pool_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                 size_t aperture_bytes, struct gui_pool_header *header);
static int parse_gpu_atlas_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                  struct gpu_atlas_header *header);

static uint32_t read_le32(const volatile uint8_t *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) | ((uint32_t)bytes[2] << 16) |
           ((uint32_t)bytes[3] << 24);
}

static uint64_t read_le64(const volatile uint8_t *bytes) {
    uint64_t value = 0;
    unsigned int index;
    for (index = 0; index < 8U; index++) {
        value |= (uint64_t)bytes[index] << (index * 8U);
    }
    return value;
}

static void write_le32(uint8_t *bytes, uint32_t value) {
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
    bytes[2] = (uint8_t)(value >> 16);
    bytes[3] = (uint8_t)(value >> 24);
}

static void write_le64(uint8_t *bytes, uint64_t value) {
    unsigned int index;
    for (index = 0U; index < 8U; index++) {
        bytes[index] = (uint8_t)(value >> (index * 8U));
    }
}

static int bytes_all_zero(const uint8_t *bytes, size_t bytes_count) {
    size_t index;
    for (index = 0U; index < bytes_count; index++) {
        if (bytes[index] != 0U) {
            return 0;
        }
    }
    return 1;
}

static int read_text_file(const char *path, char *buffer, size_t buffer_size) {
    int fd;
    ssize_t bytes;
    if (buffer_size < 2U) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    bytes = read(fd, buffer, buffer_size - 1U);
    close(fd);
    if (bytes <= 0) {
        return -1;
    }
    buffer[bytes] = '\0';
    buffer[strcspn(buffer, "\r\n")] = '\0';
    return 0;
}

static int ivshmem_bar_path(char *path, size_t path_size, size_t *bar_bytes, char *pci_bdf,
                            size_t pci_bdf_size) {
    DIR *directory = opendir("/sys/bus/pci/devices");
    struct dirent *entry;
    if (directory == NULL) {
        return -1;
    }
    while ((entry = readdir(directory)) != NULL) {
        char device_dir[PATH_MAX];
        char vendor_path[PATH_MAX];
        char device_path[PATH_MAX];
        char resource_path[PATH_MAX];
        char vendor[32];
        char device[32];
        FILE *resource;
        unsigned int index;
        unsigned long long start;
        unsigned long long end;
        unsigned long long flags;
        int fd;
        volatile uint8_t *mapping;
        struct gui_pool_header header;

        if (entry->d_name[0] == '.') {
            continue;
        }
        if (snprintf(device_dir, sizeof(device_dir), "/sys/bus/pci/devices/%s", entry->d_name) >=
            (int)sizeof(device_dir)) {
            continue;
        }
        if (snprintf(vendor_path, sizeof(vendor_path), "%s/vendor", device_dir) >=
                (int)sizeof(vendor_path) ||
            snprintf(device_path, sizeof(device_path), "%s/device", device_dir) >=
                (int)sizeof(device_path) ||
            read_text_file(vendor_path, vendor, sizeof(vendor)) != 0 ||
            read_text_file(device_path, device, sizeof(device)) != 0 ||
            strcmp(vendor, IVSHMEM_VENDOR_ID) != 0 || strcmp(device, IVSHMEM_DEVICE_ID) != 0) {
            continue;
        }
        if (snprintf(resource_path, sizeof(resource_path), "%s/resource", device_dir) >=
            (int)sizeof(resource_path)) {
            continue;
        }
        resource = fopen(resource_path, "re");
        if (resource == NULL) {
            continue;
        }
        for (index = 0; index <= IVSHMEM_RESOURCE_INDEX; index++) {
            if (fscanf(resource, "%llx %llx %llx", &start, &end, &flags) != 3) {
                break;
            }
        }
        fclose(resource);
        if (index != IVSHMEM_RESOURCE_INDEX + 1U || end < start ||
            end - start + 1U < GUI_POOL_HEADER_BYTES) {
            continue;
        }
        if (snprintf(path, path_size, "%s/resource%u", device_dir, IVSHMEM_RESOURCE_INDEX) >=
            (int)path_size) {
            continue;
        }
        fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            continue;
        }
        mapping = mmap(NULL, GUI_POOL_HEADER_BYTES, PROT_READ, MAP_SHARED, fd, 0);
        close(fd);
        if (mapping == MAP_FAILED) {
            continue;
        }
        if (parse_gui_pool_header(mapping, GUI_POOL_HEADER_BYTES,
                                  (size_t)(end - start + 1U), &header) != 0) {
            munmap((void *)mapping, GUI_POOL_HEADER_BYTES);
            continue;
        }
        munmap((void *)mapping, GUI_POOL_HEADER_BYTES);
        *bar_bytes = (size_t)(end - start + 1U);
        if (snprintf(pci_bdf, pci_bdf_size, "%s", entry->d_name) >= (int)pci_bdf_size) {
            continue;
        }
        closedir(directory);
        return 0;
    }
    closedir(directory);
    errno = ENODEV;
    return -1;
}

static int parse_gui_pool_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                 size_t aperture_bytes, struct gui_pool_header *header) {
    uint64_t required;
    if (mapped_bytes < GUI_POOL_HEADER_BYTES ||
        memcmp((const void *)bytes, GUI_POOL_MAGIC, 8U) != 0 ||
        read_le32(bytes + 8U) != GUI_POOL_VERSION ||
        read_le32(bytes + 12U) != GUI_POOL_HEADER_BYTES ||
        read_le32(bytes + 36U) != DISPLAY_BYTES_PER_PIXEL ||
        read_le32(bytes + 40U) != 1U ||
        read_le32(bytes + 44U) != GUI_POOL_SLOT_COUNT) {
        errno = EPROTO;
        return -1;
    }
    header->region_bytes = read_le64(bytes + 16U);
    header->width = read_le32(bytes + 24U);
    header->height = read_le32(bytes + 28U);
    header->stride_bytes = read_le32(bytes + 32U);
    header->slot_bytes = read_le64(bytes + 48U);
    header->flags = read_le32(bytes + 56U);
    required = GUI_POOL_HEADER_BYTES + header->slot_bytes * GUI_POOL_SLOT_COUNT;
    if (header->region_bytes > aperture_bytes ||
        header->region_bytes > GUI_POOL_MAX_REGION_BYTES ||
        header->width == 0U || header->width > UINT16_MAX ||
        header->height == 0U || header->height > UINT16_MAX || header->flags != 1U ||
        header->stride_bytes < header->width * DISPLAY_BYTES_PER_PIXEL ||
        header->stride_bytes % DISPLAY_BYTES_PER_PIXEL != 0U ||
        header->slot_bytes != (uint64_t)header->stride_bytes * header->height ||
        header->slot_bytes % 4096U != 0U ||
        required < header->slot_bytes || required > header->region_bytes) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int parse_gpu_atlas_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                  struct gpu_atlas_header *header) {
    uint64_t command_end;
    uint64_t atlas_end;
    if (bytes == NULL || header == NULL ||
        mapped_bytes < GPU_ATLAS_HEADER_OFFSET + GPU_ATLAS_HEADER_BYTES ||
        memcmp((const void *)(bytes + GPU_ATLAS_HEADER_OFFSET), GPU_ATLAS_MAGIC, 8U) != 0 ||
        read_le32(bytes + GPU_ATLAS_HEADER_OFFSET + 8U) != GPU_ATLAS_VERSION ||
        read_le32(bytes + GPU_ATLAS_HEADER_OFFSET + 12U) != GPU_ATLAS_HEADER_BYTES) {
        errno = EPROTO;
        return -1;
    }
    bytes += GPU_ATLAS_HEADER_OFFSET;
    header->region_bytes = read_le64(bytes + 16U);
    header->command_offset = read_le64(bytes + 24U);
    header->atlas_offset = read_le64(bytes + 32U);
    header->atlas_slot_bytes = read_le64(bytes + 40U);
    header->atlas_width = read_le32(bytes + 48U);
    header->atlas_height = read_le32(bytes + 52U);
    header->atlas_stride_bytes = read_le32(bytes + 56U);
    header->flags = read_le32(bytes + 60U);
    if (header->region_bytes == 0U || header->region_bytes > GUI_POOL_MAX_REGION_BYTES ||
        header->command_offset < GUI_POOL_HEADER_BYTES ||
        header->command_offset % 4096U != 0U || header->atlas_offset % 4096U != 0U ||
        header->atlas_width == 0U || header->atlas_width > 8192U ||
        header->atlas_height == 0U || header->atlas_height > 8192U ||
        header->atlas_stride_bytes < header->atlas_width * DISPLAY_BYTES_PER_PIXEL ||
        header->atlas_stride_bytes % DISPLAY_BYTES_PER_PIXEL != 0U ||
        header->atlas_slot_bytes !=
            (uint64_t)header->atlas_stride_bytes * header->atlas_height ||
        header->atlas_slot_bytes == 0U || header->atlas_slot_bytes % 4096U != 0U ||
        header->flags != 1U ||
        header->command_offset > UINT64_MAX -
            (uint64_t)GPU_ATLAS_COMMAND_SLOT_BYTES * GPU_ATLAS_SLOT_COUNT ||
        header->atlas_offset > UINT64_MAX -
            header->atlas_slot_bytes * GPU_ATLAS_SLOT_COUNT) {
        errno = EPROTO;
        return -1;
    }
    command_end = header->command_offset +
                  (uint64_t)GPU_ATLAS_COMMAND_SLOT_BYTES * GPU_ATLAS_SLOT_COUNT;
    atlas_end = header->atlas_offset + header->atlas_slot_bytes * GPU_ATLAS_SLOT_COUNT;
    if (command_end > header->atlas_offset || atlas_end > header->region_bytes) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int open_shared_display(struct shared_display *shared) {
    char path[PATH_MAX];
    char pci_bdf[sizeof(shared->pci_bdf)];
    size_t bar_bytes;
    memset(shared, 0, sizeof(*shared));
    shared->fd = -1;
    shared->uio_fd = -1;
    if (ivshmem_bar_path(path, sizeof(path), &bar_bytes, pci_bdf, sizeof(pci_bdf)) != 0) {
        return -1;
    }
    shared->fd = open(path, O_RDONLY | O_CLOEXEC);
    if (shared->fd < 0) {
        return -1;
    }
    shared->base = mmap(NULL, GUI_POOL_HEADER_BYTES, PROT_READ, MAP_SHARED, shared->fd, 0);
    if (shared->base == MAP_FAILED) {
        shared->base = NULL;
        close(shared->fd);
        shared->fd = -1;
        return -1;
    }
    shared->bytes = GUI_POOL_HEADER_BYTES;
    shared->pixel_bytes = 0U;
    if (parse_gui_pool_header(shared->base, shared->bytes, bar_bytes, &shared->header) != 0) {
        munmap((void *)shared->base, shared->bytes);
        close(shared->fd);
        shared->base = NULL;
        shared->fd = -1;
        return -1;
    }
    if (parse_gpu_atlas_header(shared->base, shared->bytes, &shared->atlas) != 0 ||
        shared->atlas.region_bytes != shared->header.region_bytes) {
        munmap((void *)shared->base, shared->bytes);
        close(shared->fd);
        shared->base = NULL;
        shared->fd = -1;
        return -1;
    }
    shared->pixel_bytes = (size_t)(shared->atlas.region_bytes - GUI_POOL_HEADER_BYTES);
    if (snprintf(shared->pci_bdf, sizeof(shared->pci_bdf), "%s", pci_bdf) >=
        (int)sizeof(shared->pci_bdf)) {
        munmap((void *)shared->base, shared->bytes);
        close(shared->fd);
        shared->base = NULL;
        shared->fd = -1;
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

static void close_shared_display(struct shared_display *shared) {
    if (shared->pixels != NULL) {
        munmap((void *)shared->pixels, shared->pixel_bytes);
    }
    if (shared->base != NULL) {
        munmap((void *)shared->base, shared->bytes);
    }
    if (shared->fd >= 0) {
        close(shared->fd);
    }
    if (shared->uio_fd >= 0) {
        close(shared->uio_fd);
    }
    memset(shared, 0, sizeof(*shared));
    shared->fd = -1;
    shared->uio_fd = -1;
}

static int open_existing_uio(const char *uio_dir_path) {
    DIR *directory;
    struct dirent *entry;
    int result = -1;
    directory = opendir(uio_dir_path);
    if (directory == NULL) {
        return -1;
    }
    while ((entry = readdir(directory)) != NULL) {
        char devnode[PATH_MAX];
        char name_path[PATH_MAX];
        char name[64];
        if (strncmp(entry->d_name, "uio", 3U) != 0 ||
            snprintf(devnode, sizeof(devnode), "/dev/%s", entry->d_name) >= (int)sizeof(devnode) ||
            snprintf(name_path, sizeof(name_path), "%s/%s/name", uio_dir_path, entry->d_name) >=
                (int)sizeof(name_path) ||
            read_text_file(name_path, name, sizeof(name)) != 0 ||
            strcmp(name, RUSTOS_IVSHMEM_UIO_NAME) != 0) {
            continue;
        }
        result = open(devnode, O_RDONLY | O_CLOEXEC);
        if (result >= 0) {
            break;
        }
    }
    closedir(directory);
    return result;
}

static int open_uio_interrupt(struct shared_display *shared) {
    char device_dir[PATH_MAX];
    char uio_dir_path[PATH_MAX];
    const struct timespec retry_delay = {.tv_sec = 0, .tv_nsec = UIO_BIND_RETRY_NS};
    unsigned int attempt;
    int result;
    if (snprintf(device_dir, sizeof(device_dir), "/sys/bus/pci/devices/%s", shared->pci_bdf) >=
            (int)sizeof(device_dir) ||
        snprintf(uio_dir_path, sizeof(uio_dir_path), "%s/uio", device_dir) >=
            (int)sizeof(uio_dir_path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    /*
     * The dedicated module matches the fixed RustOS header and allocates one
     * MSI-X vector before it exposes its UIO child. Retrying only this setup
     * race never changes PCI binding from userspace and cannot fall back to
     * the INTx-only generic UIO provider.
     */
    for (attempt = 0U; attempt < UIO_BIND_RETRIES; attempt++) {
        result = open_existing_uio(uio_dir_path);
        if (result >= 0) {
            return result;
        }
        (void)nanosleep(&retry_delay, NULL);
    }
    return -1;
}

static int map_gpu_pixel_pool(struct shared_display *shared) {
    if (shared == NULL || shared->uio_fd < 0 || shared->pixel_bytes == 0U) {
        errno = EINVAL;
        return -1;
    }
    shared->pixels = mmap(NULL, shared->pixel_bytes, PROT_READ, MAP_SHARED,
                          shared->uio_fd, 0);
    if (shared->pixels == MAP_FAILED) {
        shared->pixels = NULL;
        return -1;
    }
    return 0;
}

static int host_invitation_pending(const struct shared_display *shared, int *pending) {
    char path[PATH_MAX];
    char value[8];
    ssize_t bytes;
    int fd;
    if (shared == NULL || pending == NULL ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_HOST_INVITED_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    bytes = read(fd, value, sizeof(value));
    close(fd);
    if (bytes != 2 || value[0] < '0' || value[0] > '1' || value[1] != '\n') {
        errno = EPROTO;
        return -1;
    }
    *pending = value[0] == '1';
    return 0;
}

static int acknowledge_host_invitation(const struct shared_display *shared) {
    char path[PATH_MAX];
    static const char ready[] = "ready\n";
    ssize_t bytes;
    int fd;
    if (shared == NULL ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_DISPLAY_READY_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    bytes = write(fd, ready, sizeof(ready) - 1U);
    close(fd);
    if (bytes != (ssize_t)(sizeof(ready) - 1U)) {
        if (bytes >= 0) {
            errno = EIO;
        }
        return -1;
    }
    return 0;
}

static int host_confirmed_peer_ready(const struct shared_display *shared);

static int notify_host_offline(const struct shared_display *shared) {
    char path[PATH_MAX];
    static const char offline[] = "offline\n";
    ssize_t bytes;
    int fd;
    if (shared == NULL ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_DISPLAY_OFFLINE_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    bytes = write(fd, offline, sizeof(offline) - 1U);
    close(fd);
    if (bytes != (ssize_t)(sizeof(offline) - 1U)) {
        if (bytes >= 0)
            errno = EIO;
        return -1;
    }
    return 0;
}

static void report_host_offline(const struct shared_display *shared) {
    (void)notify_host_offline(shared);
}

/*
 * LIFECYCLE: a replacement relay may inherit an aperture whose confirmation
 * still names its dead predecessor. It must revoke that lease before creating
 * a new GPU context; accepting the old confirmation would let stale
 * completions cross process lifetimes. The host interrupt handler clears the
 * confirmation and increments the context epoch. Bound the acknowledgement
 * wait so a stopped RustOS peer cannot turn DVM recovery into an infinite
 * wait.
 */
static int revoke_predecessor_lease(const struct shared_display *shared) {
    const unsigned int attempts = 200U;
    unsigned int attempt;
    if (!host_confirmed_peer_ready(shared))
        return 0;
    if (notify_host_offline(shared) != 0)
        return -1;
    for (attempt = 0U; attempt < attempts; attempt++) {
        if (read_le64(shared->base + GUI_POOL_READY_CONFIRMATION_OFFSET) == 0U) {
            relay_log("rustos-dvm-display: predecessor lease revoked before rebind\n");
            return 0;
        }
        usleep(10000U);
    }
    errno = ETIMEDOUT;
    return -1;
}

static int host_confirmed_peer_ready(const struct shared_display *shared) {
    uint64_t invitation = read_le64(shared->base + GUI_POOL_INVITATION_OFFSET);
    uint64_t confirmed = read_le64(shared->base + GUI_POOL_READY_CONFIRMATION_OFFSET);
    return invitation != 0U && (invitation & 1U) == 0U && invitation == confirmed;
}

static uint32_t select_crtc(const drmModeRes *resources, const drmModeEncoder *encoder) {
    int index;
    if (encoder->crtc_id != 0U) {
        return encoder->crtc_id;
    }
    for (index = 0; index < resources->count_crtcs; index++) {
        if ((encoder->possible_crtcs & (1U << index)) != 0U) {
            return resources->crtcs[index];
        }
    }
    return 0U;
}

static uint32_t select_connector_crtc(int fd, const drmModeRes *resources,
                                      const drmModeConnector *connector) {
    uint32_t index;
    if (connector->encoder_id != 0U) {
        drmModeEncoder *encoder = drmModeGetEncoder(fd, connector->encoder_id);
        if (encoder != NULL) {
            uint32_t crtc = select_crtc(resources, encoder);
            drmModeFreeEncoder(encoder);
            if (crtc != 0U)
                return crtc;
        }
    }
    /* A freshly initialized passthrough GPU can report a connected eDP panel
     * without a currently bound encoder. Atomic modesetting is allowed to pick
     * one of the connector's possible encoders; requiring encoder_id would
     * make first boot depend on stale firmware display state. */
    if (connector->count_encoders <= 0 || connector->encoders == NULL)
        return 0U;
    for (index = 0U; index < (uint32_t)connector->count_encoders; index++) {
        drmModeEncoder *encoder;
        uint32_t crtc;
        if (connector->encoders[index] == connector->encoder_id)
            continue;
        encoder = drmModeGetEncoder(fd, connector->encoders[index]);
        if (encoder == NULL)
            continue;
        crtc = select_crtc(resources, encoder);
        drmModeFreeEncoder(encoder);
        if (crtc != 0U)
            return crtc;
    }
    return 0U;
}

static int select_kms_target(int fd, const struct gui_pool_header *header, uint32_t *connector_id,
                             uint32_t *crtc_id, drmModeModeInfo *mode) {
    drmModeRes *resources = drmModeGetResources(fd);
    int result = -1;
    uint32_t fallback_connector = 0U;
    uint32_t fallback_crtc = 0U;
    drmModeModeInfo fallback_mode;
    int connector_index;
    if (resources == NULL) {
        return -1;
    }
    for (connector_index = 0; connector_index < resources->count_connectors; connector_index++) {
        drmModeConnector *connector = drmModeGetConnector(fd, resources->connectors[connector_index]);
        uint32_t candidate_crtc;
        int mode_index;
        if (connector == NULL) {
            continue;
        }
        if (connector->connection != DRM_MODE_CONNECTED || connector->count_modes == 0) {
            drmModeFreeConnector(connector);
            continue;
        }
        candidate_crtc = select_connector_crtc(fd, resources, connector);
        if (candidate_crtc == 0U) {
            drmModeFreeConnector(connector);
            continue;
        }
        if (fallback_connector == 0U) {
            fallback_connector = connector->connector_id;
            fallback_crtc = candidate_crtc;
            fallback_mode = connector->modes[0];
        }
        for (mode_index = 0; mode_index < connector->count_modes; mode_index++) {
            if ((uint32_t)connector->modes[mode_index].hdisplay == header->width &&
                (uint32_t)connector->modes[mode_index].vdisplay == header->height) {
                *connector_id = connector->connector_id;
                *crtc_id = candidate_crtc;
                *mode = connector->modes[mode_index];
                result = candidate_crtc == 0U ? -1 : 0;
                break;
            }
        }
        drmModeFreeConnector(connector);
        if (result == 0) {
            break;
        }
    }
    drmModeFreeResources(resources);
    if (result != 0 && fallback_connector != 0U) {
        *connector_id = fallback_connector;
        *crtc_id = fallback_crtc;
        *mode = fallback_mode;
        return 0;
    }
    if (result != 0) {
        errno = ENODEV;
    }
    return result;
}

static uint32_t object_property_id(int fd, uint32_t object_id, uint32_t object_type,
                                   const char *name, uint64_t *value) {
    drmModeObjectProperties *properties;
    uint32_t result = 0U;
    uint32_t index;
    properties = drmModeObjectGetProperties(fd, object_id, object_type);
    if (properties == NULL) {
        return 0U;
    }
    for (index = 0U; index < properties->count_props; index++) {
        drmModePropertyRes *property = drmModeGetProperty(fd, properties->props[index]);
        if (property != NULL && strcmp(property->name, name) == 0) {
            result = property->prop_id;
            if (value != NULL) {
                *value = properties->prop_values[index];
            }
            drmModeFreeProperty(property);
            break;
        }
        if (property != NULL) {
            drmModeFreeProperty(property);
        }
    }
    drmModeFreeObjectProperties(properties);
    return result;
}

static int crtc_index_for_id(int fd, uint32_t crtc_id, uint32_t *index) {
    drmModeRes *resources = drmModeGetResources(fd);
    int result = -1;
    int candidate;
    if (resources == NULL || index == NULL) {
        errno = EINVAL;
        return -1;
    }
    for (candidate = 0; candidate < resources->count_crtcs; candidate++) {
        if (resources->crtcs[candidate] == crtc_id) {
            *index = (uint32_t)candidate;
            result = 0;
            break;
        }
    }
    drmModeFreeResources(resources);
    if (result != 0) {
        errno = ENODEV;
    }
    return result;
}

static int select_primary_plane(int fd, uint32_t crtc_id, uint32_t *plane_id) {
    drmModePlaneRes *planes;
    uint32_t crtc_index;
    uint32_t index;
    if (plane_id == NULL || crtc_index_for_id(fd, crtc_id, &crtc_index) != 0 || crtc_index >= 32U) {
        errno = EINVAL;
        return -1;
    }
    planes = drmModeGetPlaneResources(fd);
    if (planes == NULL) {
        return -1;
    }
    for (index = 0U; index < planes->count_planes; index++) {
        drmModePlane *plane = drmModeGetPlane(fd, planes->planes[index]);
        uint64_t type = UINT64_MAX;
        uint32_t type_property;
        if (plane == NULL) {
            continue;
        }
        type_property = object_property_id(fd, plane->plane_id, DRM_MODE_OBJECT_PLANE, "type", &type);
        if ((plane->possible_crtcs & (1U << crtc_index)) != 0U && type_property != 0U &&
            type == DRM_PLANE_TYPE_PRIMARY) {
            *plane_id = plane->plane_id;
            drmModeFreePlane(plane);
            drmModeFreePlaneResources(planes);
            return 0;
        }
        drmModeFreePlane(plane);
    }
    drmModeFreePlaneResources(planes);
    errno = ENODEV;
    return -1;
}

static int load_atomic_property_ids(struct kms_display *display) {
    struct atomic_property_ids *properties = &display->properties;
    properties->connector_crtc_id =
        object_property_id(display->fd, display->connector_id, DRM_MODE_OBJECT_CONNECTOR,
                           "CRTC_ID", NULL);
    properties->crtc_mode_id = object_property_id(display->fd, display->crtc_id,
                                                   DRM_MODE_OBJECT_CRTC, "MODE_ID", NULL);
    properties->crtc_active = object_property_id(display->fd, display->crtc_id,
                                                  DRM_MODE_OBJECT_CRTC, "ACTIVE", NULL);
    properties->plane_fb_id = object_property_id(display->fd, display->primary_plane_id,
                                                  DRM_MODE_OBJECT_PLANE, "FB_ID", NULL);
    properties->plane_crtc_id = object_property_id(display->fd, display->primary_plane_id,
                                                    DRM_MODE_OBJECT_PLANE, "CRTC_ID", NULL);
    properties->plane_src_x = object_property_id(display->fd, display->primary_plane_id,
                                                  DRM_MODE_OBJECT_PLANE, "SRC_X", NULL);
    properties->plane_src_y = object_property_id(display->fd, display->primary_plane_id,
                                                  DRM_MODE_OBJECT_PLANE, "SRC_Y", NULL);
    properties->plane_src_w = object_property_id(display->fd, display->primary_plane_id,
                                                  DRM_MODE_OBJECT_PLANE, "SRC_W", NULL);
    properties->plane_src_h = object_property_id(display->fd, display->primary_plane_id,
                                                  DRM_MODE_OBJECT_PLANE, "SRC_H", NULL);
    properties->plane_crtc_x = object_property_id(display->fd, display->primary_plane_id,
                                                   DRM_MODE_OBJECT_PLANE, "CRTC_X", NULL);
    properties->plane_crtc_y = object_property_id(display->fd, display->primary_plane_id,
                                                   DRM_MODE_OBJECT_PLANE, "CRTC_Y", NULL);
    properties->plane_crtc_w = object_property_id(display->fd, display->primary_plane_id,
                                                   DRM_MODE_OBJECT_PLANE, "CRTC_W", NULL);
    properties->plane_crtc_h = object_property_id(display->fd, display->primary_plane_id,
                                                   DRM_MODE_OBJECT_PLANE, "CRTC_H", NULL);
    properties->plane_fb_damage_clips =
        object_property_id(display->fd, display->primary_plane_id, DRM_MODE_OBJECT_PLANE,
                           "FB_DAMAGE_CLIPS", NULL);
    properties->plane_in_fence_fd =
        object_property_id(display->fd, display->primary_plane_id, DRM_MODE_OBJECT_PLANE,
                           "IN_FENCE_FD", NULL);
    properties->crtc_out_fence_ptr =
        object_property_id(display->fd, display->crtc_id, DRM_MODE_OBJECT_CRTC,
                           "OUT_FENCE_PTR", NULL);
    if (properties->connector_crtc_id == 0U || properties->crtc_mode_id == 0U ||
        properties->crtc_active == 0U || properties->plane_fb_id == 0U ||
        properties->plane_crtc_id == 0U || properties->plane_src_x == 0U ||
        properties->plane_src_y == 0U || properties->plane_src_w == 0U ||
        properties->plane_src_h == 0U || properties->plane_crtc_x == 0U ||
        properties->plane_crtc_y == 0U || properties->plane_crtc_w == 0U ||
        properties->plane_crtc_h == 0U || properties->plane_fb_damage_clips == 0U ||
        properties->plane_in_fence_fd == 0U || properties->crtc_out_fence_ptr == 0U) {
        errno = EOPNOTSUPP;
        return -1;
    }
    return 0;
}

static int add_plane_properties(const struct kms_display *display, drmModeAtomicReq *request,
                                uint32_t framebuffer_id, uint32_t damage_blob_id) {
    const struct atomic_property_ids *properties = &display->properties;
    uint64_t source_width = (uint64_t)display->source_width << 16U;
    uint64_t source_height = (uint64_t)display->source_height << 16U;
    if (drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_fb_id,
                                 framebuffer_id) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_crtc_id,
                                 display->crtc_id) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_src_x, 0U) <
            0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_src_y, 0U) <
            0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_src_w,
                                 source_width) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_src_h,
                                 source_height) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_crtc_x,
                                 0U) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_crtc_y,
                                 0U) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_crtc_w,
                                 display->mode.hdisplay) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id, properties->plane_crtc_h,
                                 display->mode.vdisplay) < 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id,
                                 properties->plane_fb_damage_clips, damage_blob_id) < 0) {
        return -1;
    }
    return 0;
}

static int monotonic_time_ns(uint64_t *value) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0) {
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000ULL * 1000ULL * 1000ULL + (uint64_t)now.tv_nsec;
    return 0;
}

static int open_display_state_directory(void) {
    struct stat state;
    int fd = open(RUSTOS_DVM_STATE_DIR, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0)
        return -1;
    if (fstat(fd, &state) != 0 || !S_ISDIR(state.st_mode) || state.st_uid != geteuid() ||
        (state.st_mode & 0777U) != 0700U) {
        int saved = errno != 0 ? errno : EPERM;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

static int claim_display_process_owner(void) {
    struct stat state;
    int directory_fd = open_display_state_directory();
    int owner_fd;
    int saved;
    if (directory_fd < 0)
        return -1;
    owner_fd = openat(directory_fd, RUSTOS_DVM_DISPLAY_OWNER_NAME,
                      O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    saved = errno;
    close(directory_fd);
    if (owner_fd < 0) {
        errno = saved;
        return -1;
    }
    if (fstat(owner_fd, &state) != 0 || !S_ISREG(state.st_mode) ||
        state.st_uid != geteuid() || (state.st_mode & 0777U) != 0600U || state.st_nlink != 1 ||
        flock(owner_fd, LOCK_EX | LOCK_NB) != 0) {
        saved = errno != 0 ? errno : EPERM;
        close(owner_fd);
        errno = saved;
        return -1;
    }
    return owner_fd;
}

static int publish_display_ready_lock(int dmabuf_sources) {
    static const char staged_state[] =
        "DISPLAY_RELAY_SCHEMA=2\n"
        "STATE=ready\n"
        "MODE=gpu-compositor-staged-copy\n"
        "ZERO_COPY=0\n"
        "GPU_COMPOSITION=1\n"
        "EXPLICIT_FENCE=1\n";
    static const char dmabuf_state[] =
        "DISPLAY_RELAY_SCHEMA=3\n"
        "STATE=ready\n"
        "MODE=gpu-compositor-dmabuf-source\n"
        "SOURCE_PATH=dmabuf\n"
        "ZERO_COPY=1\n"
        "GPU_COMPOSITION=1\n"
        "EXPLICIT_FENCE=1\n"
        "ATOMIC_KMS_SCANOUT=1\n"
        "SCANOUT_BUFFERS=3\n"
        "STAGED_DAMAGE_COPY=0\n"
        "CPU_FINAL_COMPOSE=0\n";
    const char *state = dmabuf_sources ? dmabuf_state : staged_state;
    size_t state_length = dmabuf_sources ? sizeof(dmabuf_state) - 1U
                                         : sizeof(staged_state) - 1U;
    struct stat file_state;
    int directory_fd = open_display_state_directory();
    int ready_fd = -1;
    int candidate_created = 0;
    int installed = 0;
    int saved;
    if (directory_fd < 0)
        return -1;
    if (unlinkat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE, 0) != 0 && errno != ENOENT)
        goto fail;
    ready_fd = openat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE,
                      O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (ready_fd < 0)
        goto fail;
    candidate_created = 1;
    if (fstat(ready_fd, &file_state) != 0 || !S_ISREG(file_state.st_mode) ||
        file_state.st_uid != geteuid() || (file_state.st_mode & 0777U) != 0600U ||
        file_state.st_nlink != 1 || flock(ready_fd, LOCK_EX | LOCK_NB) != 0 ||
        write_complete(ready_fd, state, state_length) != 0 || fsync(ready_fd) != 0 ||
        renameat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE, directory_fd,
                 RUSTOS_DVM_DISPLAY_READY_NAME) != 0)
        goto fail;
    candidate_created = 0;
    installed = 1;
    if (fsync(directory_fd) != 0)
        goto fail;
    close(directory_fd);
    return ready_fd;

fail:
    saved = errno != 0 ? errno : EIO;
    if (!installed && candidate_created)
        (void)unlinkat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE, 0);
    if (ready_fd >= 0)
        close(ready_fd);
    close(directory_fd);
    errno = saved;
    return -1;
}

static int write_complete(int fd, const void *buffer, size_t length) {
    const uint8_t *cursor = buffer;
    while (length != 0U) {
        ssize_t written = write(fd, cursor, length);
        if (written < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        cursor += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static int publish_display_evidence(const struct kms_display *display, int dmabuf_sources,
                                    uint64_t sample_sequence,
                                    uint64_t sample_monotonic_ns, uint64_t window_ns,
                                    uint64_t pageflip_completions, uint64_t frame_hz_milli,
                                    uint64_t pageflip_latency_us_avg,
                                    uint64_t pageflip_latency_us_max,
                                    uint64_t atomic_commit_us_avg) {
    char state[1024];
    int length;
    int fd;
    int saved;
    length = snprintf(
        state, sizeof(state),
        "DISPLAY_EVIDENCE_SCHEMA=2\nSAMPLE_SEQUENCE=%llu\nSAMPLE_MONOTONIC_NS=%llu\n"
        "WINDOW_NS=%llu\nPAGEFLIP_COMPLETIONS=%llu\nFRAME_HZ_MILLI=%llu\n"
        "CPU_COPY_US_AVG=0\nPAGEFLIP_LATENCY_US_AVG=%llu\n"
        "PAGEFLIP_LATENCY_US_MAX=%llu\nATOMIC_COMMIT_US_AVG=%llu\n"
        "CONNECTOR_ID=%u\nMODE_WIDTH=%u\nMODE_HEIGHT=%u\n"
        "SOURCE_PATH=%s\nZERO_COPY=%s\nGPU_COMPOSITION=yes\nEXPLICIT_FENCE=yes\n"
        "ATOMIC_KMS_SCANOUT=yes\nSCANOUT_BUFFERS=3\nSTAGED_DAMAGE_COPY=%s\n",
        (unsigned long long)sample_sequence, (unsigned long long)sample_monotonic_ns,
        (unsigned long long)window_ns, (unsigned long long)pageflip_completions,
        (unsigned long long)frame_hz_milli, (unsigned long long)pageflip_latency_us_avg,
        (unsigned long long)pageflip_latency_us_max,
        (unsigned long long)atomic_commit_us_avg, display->connector_id,
        display->mode.hdisplay, display->mode.vdisplay,
        dmabuf_sources ? "dmabuf" : "staged-copy",
        dmabuf_sources ? "yes" : "no", dmabuf_sources ? "no" : "yes");
    if (length <= 0 || (size_t)length >= sizeof(state)) {
        errno = EOVERFLOW;
        return -1;
    }
    fd = open(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP,
              O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0)
        return -1;
    if (fchmod(fd, 0600) != 0 || write_complete(fd, state, (size_t)length) != 0 || fsync(fd) != 0) {
        saved = errno == 0 ? EIO : errno;
        (void)close(fd);
        (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    if (close(fd) != 0) {
        saved = errno == 0 ? EIO : errno;
        (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    if (rename(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP, RUSTOS_DVM_DISPLAY_EVIDENCE) != 0) {
        saved = errno == 0 ? EIO : errno;
        (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    return 0;
}

static int report_relay_stats(struct kms_display *display, int dmabuf_sources,
                              uint64_t pageflip_completions,
                              uint64_t atomic_commit_time_ns,
                              uint64_t atomic_commit_measurements, uint64_t *last_reported_ns,
                              uint64_t *last_pageflip_completions,
                              uint64_t *last_atomic_commit_time_ns,
                              uint64_t *last_atomic_commit_measurements,
                              uint64_t *last_pageflip_latency_time_ns,
                              uint64_t *sample_sequence,
                              struct gpu_relay_metrics *gpu_metrics) {
    uint64_t now;
    uint64_t elapsed_ns;
    uint64_t submitted_frames;
    uint64_t frame_hz_milli = 0U;
    uint64_t average_atomic_commit_us = 0U;
    uint64_t average_pageflip_latency_us = 0U;
    uint64_t maximum_pageflip_latency_us = display->pageflip_latency_max_ns / 1000ULL;
    uint64_t pageflip_latency_time_ns;
    uint64_t measured_atomic_commits;
    uint64_t measured_gpu_renders = 0U;
    uint64_t average_gpu_render_us = 0U;
    uint64_t maximum_gpu_render_us = 0U;
    if (monotonic_time_ns(&now) != 0 || now <= *last_reported_ns ||
        now - *last_reported_ns < DISPLAY_STATS_INTERVAL_NS) {
        return 0;
    }
    elapsed_ns = now - *last_reported_ns;
    submitted_frames = pageflip_completions - *last_pageflip_completions;
    pageflip_latency_time_ns =
        display->pageflip_latency_time_ns - *last_pageflip_latency_time_ns;
    measured_atomic_commits = atomic_commit_measurements - *last_atomic_commit_measurements;
    if (measured_atomic_commits != submitted_frames) {
        errno = EIO;
        return -1;
    }
    if (gpu_metrics != NULL) {
        uint64_t measured_gpu_render_time_ns;
        if (gpu_metrics->render_measurements < gpu_metrics->last_render_measurements ||
            gpu_metrics->render_time_ns < gpu_metrics->last_render_time_ns) {
            errno = EOVERFLOW;
            return -1;
        }
        measured_gpu_renders =
            gpu_metrics->render_measurements - gpu_metrics->last_render_measurements;
        measured_gpu_render_time_ns =
            gpu_metrics->render_time_ns - gpu_metrics->last_render_time_ns;
        if (measured_gpu_renders != submitted_frames) {
            errno = EIO;
            return -1;
        }
        if (measured_gpu_renders != 0U) {
            uint64_t average_gpu_render_ns =
                measured_gpu_render_time_ns / measured_gpu_renders;
            if (measured_gpu_render_time_ns % measured_gpu_renders != 0U)
                average_gpu_render_ns++;
            average_gpu_render_us = average_gpu_render_ns / 1000ULL;
            if (average_gpu_render_ns % 1000ULL != 0U)
                average_gpu_render_us++;
        }
        maximum_gpu_render_us = gpu_metrics->render_max_ns / 1000ULL;
        if (gpu_metrics->render_max_ns % 1000ULL != 0U)
            maximum_gpu_render_us++;
    }
    if (submitted_frames != 0U) {
        frame_hz_milli = (submitted_frames * 1000ULL * 1000ULL * 1000ULL * 1000ULL) /
                         elapsed_ns;
        uint64_t average_atomic_commit_ns =
            (atomic_commit_time_ns - *last_atomic_commit_time_ns) / submitted_frames;
        uint64_t average_pageflip_latency_ns = pageflip_latency_time_ns / submitted_frames;
        if ((atomic_commit_time_ns - *last_atomic_commit_time_ns) % submitted_frames != 0U)
            average_atomic_commit_ns++;
        if (pageflip_latency_time_ns % submitted_frames != 0U)
            average_pageflip_latency_ns++;
        average_atomic_commit_us = average_atomic_commit_ns / 1000ULL;
        average_pageflip_latency_us = average_pageflip_latency_ns / 1000ULL;
        if (average_atomic_commit_ns % 1000ULL != 0U)
            average_atomic_commit_us++;
        if (average_pageflip_latency_ns % 1000ULL != 0U)
            average_pageflip_latency_us++;
    }
    if (*sample_sequence == UINT64_MAX) {
        errno = EOVERFLOW;
        return -1;
    }
    if (publish_display_evidence(display, dmabuf_sources, *sample_sequence + 1U, now, elapsed_ns,
                                 submitted_frames, frame_hz_milli,
                                 average_pageflip_latency_us, maximum_pageflip_latency_us,
                                 average_atomic_commit_us) != 0)
        return -1;
    (*sample_sequence)++;
    if (gpu_metrics != NULL) {
        relay_log(
            "rustos-dvm-display: stats sample_sequence=%llu elapsed_ms=%llu frame_hz_milli=%llu pageflip_completions=%llu relay_cpu_copy_us_avg=0 pageflip_latency_us_avg=%llu pageflip_latency_us_max=%llu atomic_commit_us_avg=%llu gpu_render_us_avg=%llu gpu_render_us_max=%llu gpu_fence_completions=%llu present_fence_completions=%llu\n",
            (unsigned long long)*sample_sequence,
            (unsigned long long)(elapsed_ns / (1000ULL * 1000ULL)),
            (unsigned long long)frame_hz_milli, (unsigned long long)submitted_frames,
            (unsigned long long)average_pageflip_latency_us,
            (unsigned long long)maximum_pageflip_latency_us,
            (unsigned long long)average_atomic_commit_us,
            (unsigned long long)average_gpu_render_us,
            (unsigned long long)maximum_gpu_render_us,
            (unsigned long long)measured_gpu_renders,
            (unsigned long long)measured_gpu_renders);
    } else {
        relay_log(
            "rustos-dvm-display: stats sample_sequence=%llu elapsed_ms=%llu frame_hz_milli=%llu pageflip_completions=%llu relay_cpu_copy_us_avg=0 pageflip_latency_us_avg=%llu pageflip_latency_us_max=%llu atomic_commit_us_avg=%llu\n",
            (unsigned long long)*sample_sequence,
            (unsigned long long)(elapsed_ns / (1000ULL * 1000ULL)),
            (unsigned long long)frame_hz_milli, (unsigned long long)submitted_frames,
            (unsigned long long)average_pageflip_latency_us,
            (unsigned long long)maximum_pageflip_latency_us,
            (unsigned long long)average_atomic_commit_us);
    }
    *last_reported_ns = now;
    *last_pageflip_completions = pageflip_completions;
    *last_atomic_commit_time_ns = atomic_commit_time_ns;
    *last_atomic_commit_measurements = atomic_commit_measurements;
    *last_pageflip_latency_time_ns = display->pageflip_latency_time_ns;
    display->pageflip_latency_max_ns = 0U;
    if (gpu_metrics != NULL) {
        gpu_metrics->last_render_time_ns = gpu_metrics->render_time_ns;
        gpu_metrics->last_render_measurements = gpu_metrics->render_measurements;
        gpu_metrics->render_max_ns = 0U;
    }
    return 0;
}

struct gpu_flip_wait {
    int page_flip_complete;
};

static void gpu_page_flip_completed(int fd, unsigned int sequence, unsigned int seconds,
                                    unsigned int microseconds, void *user_data) {
    struct gpu_flip_wait *wait = user_data;
    (void)fd;
    (void)sequence;
    (void)seconds;
    (void)microseconds;
    if (wait != NULL)
        wait->page_flip_complete = 1;
}

static int wait_sync_file(int fd, uint32_t timeout_us) {
    struct pollfd pollfd = {.fd = fd, .events = POLLIN, .revents = 0};
    int timeout_ms = (int)((timeout_us + 999U) / 1000U);
    int result;
    do {
        result = poll(&pollfd, 1U, timeout_ms);
    } while (result < 0 && errno == EINTR);
    if (result <= 0 || (pollfd.revents & POLLIN) == 0 ||
        (pollfd.revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
        errno = result == 0 ? ETIMEDOUT : EIO;
        return -1;
    }
    return 0;
}

static int add_gpu_plane_properties(const struct kms_display *display,
                                    drmModeAtomicReq *request,
                                    uint32_t framebuffer_id, int in_fence_fd) {
    if (add_plane_properties(display, request, framebuffer_id, 0U) != 0 ||
        drmModeAtomicAddProperty(request, display->primary_plane_id,
                                 display->properties.plane_in_fence_fd,
                                 (uint64_t)(int64_t)in_fence_fd) < 0)
        return -1;
    return 0;
}

static int atomic_gpu_initial_modeset(struct kms_display *display,
                                      struct rustos_gpu_runtime *runtime,
                                      struct rustos_gpu_frame *frame,
                                      int *present_fence_observed) {
    drmModeAtomicReq *request;
    int out_fence_fd = -1;
    int result;
    if (present_fence_observed == NULL) {
        errno = EINVAL;
        return -1;
    }
    *present_fence_observed = 0;
    display->setup_stage = "gpu-prime-render-fence";
    if (wait_sync_file(frame->in_fence_fd, frame->budget_us) != 0) {
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        return -1;
    }
    display->setup_stage = "gpu-prime-mode-blob";
    if (drmModeCreatePropertyBlob(display->fd, &display->mode, sizeof(display->mode),
                                  &display->mode_blob_id) != 0)
        return -1;
    display->setup_stage = "gpu-prime-atomic-request";
    request = drmModeAtomicAlloc();
    if (request == NULL)
        return -1;
    display->setup_stage = "gpu-prime-atomic-commit";
    result = drmModeAtomicAddProperty(request, display->connector_id,
                                      display->properties.connector_crtc_id,
                                      display->crtc_id) < 0 ||
             drmModeAtomicAddProperty(request, display->crtc_id,
                                      display->properties.crtc_mode_id,
                                      display->mode_blob_id) < 0 ||
             drmModeAtomicAddProperty(request, display->crtc_id,
                                      display->properties.crtc_active, 1U) < 0 ||
             drmModeAtomicAddProperty(request, display->crtc_id,
                                      display->properties.crtc_out_fence_ptr,
                                      (uint64_t)(uintptr_t)&out_fence_fd) < 0 ||
             add_gpu_plane_properties(display, request, frame->framebuffer_id,
                                      frame->in_fence_fd) != 0
                 ? -1
                 : drmModeAtomicCommit(display->fd, request,
                                       DRM_MODE_ATOMIC_ALLOW_MODESET, NULL);
    drmModeAtomicFree(request);
    close(frame->in_fence_fd);
    frame->in_fence_fd = -1;
    if (result != 0) {
        if (out_fence_fd >= 0)
            close(out_fence_fd);
        return -1;
    }
    if (out_fence_fd >= 0) {
        display->setup_stage = "gpu-prime-present-fence";
        result = wait_sync_file(out_fence_fd, DISPLAY_PAGEFLIP_TIMEOUT_MS * 1000U);
        close(out_fence_fd);
        if (result != 0)
            return -1;
        *present_fence_observed = 1;
    }
    rustos_gpu_runtime_presented(runtime, frame->output_index);
    display->front_buffer = frame->output_index;
    return 0;
}

static int atomic_gpu_page_flip(struct kms_display *display,
                                struct rustos_gpu_runtime *runtime,
                                struct rustos_gpu_frame *frame,
                                uint64_t *presented_ns, uint64_t *render_time_ns,
                                uint64_t *atomic_commit_time_ns,
                                uint64_t *atomic_commit_measurements) {
    drmModeAtomicReq *request;
    struct gpu_flip_wait wait = {0};
    drmEventContext event = {
        .version = DRM_EVENT_CONTEXT_VERSION,
        .page_flip_handler = gpu_page_flip_completed,
    };
    struct pollfd pollfds[3];
    uint64_t render_completed_ns = 0U;
    uint64_t commit_started_ns = 0U;
    uint64_t commit_completed_ns = 0U;
    uint64_t deadline_ns = 0U;
    int out_fence_fd = -1;
    int result;
    int out_complete = 0;
    int render_complete = 0;
    if (display == NULL || runtime == NULL || frame == NULL || presented_ns == NULL ||
        render_time_ns == NULL || atomic_commit_time_ns == NULL ||
        atomic_commit_measurements == NULL || frame->output_index == display->front_buffer ||
        frame->in_fence_fd < 0) {
        errno = EINVAL;
        return -1;
    }
    display->setup_stage = "gpu-frame-atomic-request";
    request = drmModeAtomicAlloc();
    if (request == NULL) {
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        return -1;
    }
    result = add_gpu_plane_properties(display, request, frame->framebuffer_id,
                                      frame->in_fence_fd);
    if (result == 0 &&
        drmModeAtomicAddProperty(request, display->crtc_id,
                                 display->properties.crtc_out_fence_ptr,
                                 (uint64_t)(uintptr_t)&out_fence_fd) < 0)
        result = -1;
    if (result == 0 && monotonic_time_ns(&commit_started_ns) != 0)
        result = -1;
    display->setup_stage = "gpu-frame-atomic-commit";
    if (result == 0)
        result = drmModeAtomicCommit(display->fd, request,
                                     DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
                                     &wait);
    if (result == 0 && monotonic_time_ns(&commit_completed_ns) != 0)
        result = -1;
    drmModeAtomicFree(request);
    if (result != 0 || out_fence_fd < 0) {
        if (out_fence_fd >= 0)
            close(out_fence_fd);
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        return -1;
    }
    if (UINT64_MAX - commit_started_ns <
        (uint64_t)DISPLAY_PAGEFLIP_TIMEOUT_MS * 1000000ULL) {
        close(out_fence_fd);
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        errno = EOVERFLOW;
        return -1;
    }
    deadline_ns = commit_started_ns +
                  (uint64_t)DISPLAY_PAGEFLIP_TIMEOUT_MS * 1000000ULL;
    display->setup_stage = "gpu-frame-present-fences";
    while (!wait.page_flip_complete || !out_complete || !render_complete) {
        uint64_t now_ns;
        uint64_t remaining_ns;
        int timeout_ms;
        if (monotonic_time_ns(&now_ns) != 0) {
            result = -1;
            goto fail_fences;
        }
        if (now_ns >= deadline_ns) {
            result = 0;
            goto fail_fences;
        }
        remaining_ns = deadline_ns - now_ns;
        timeout_ms = (int)((remaining_ns + 999999ULL) / 1000000ULL);
        if (timeout_ms <= 0)
            timeout_ms = 1;
        pollfds[0].fd = display->fd;
        pollfds[0].events = wait.page_flip_complete ? 0 : POLLIN;
        pollfds[0].revents = 0;
        pollfds[1].fd = out_fence_fd;
        pollfds[1].events = out_complete ? 0 : POLLIN;
        pollfds[1].revents = 0;
        pollfds[2].fd = frame->in_fence_fd;
        pollfds[2].events = render_complete ? 0 : POLLIN;
        pollfds[2].revents = 0;
        result = poll(pollfds, 3U, timeout_ms);
        if (result < 0 && errno == EINTR)
            continue;
        if (result <= 0 ||
            (pollfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
            (pollfds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
            (pollfds[2].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
            if (result > 0)
                errno = EIO;
            goto fail_fences;
        }
        if ((pollfds[0].revents & POLLIN) != 0 && drmHandleEvent(display->fd, &event) != 0) {
            result = -1;
            goto fail_fences;
        }
        if ((pollfds[1].revents & POLLIN) != 0)
            out_complete = 1;
        if ((pollfds[2].revents & POLLIN) != 0) {
            if (monotonic_time_ns(&render_completed_ns) != 0) {
                result = -1;
                goto fail_fences;
            }
            render_complete = 1;
        }
    }
    close(out_fence_fd);
    close(frame->in_fence_fd);
    frame->in_fence_fd = -1;
    display->setup_stage = "gpu-frame-render-budget";
    if (render_completed_ns <= frame->render_started_ns ||
        render_completed_ns - frame->render_started_ns >
            (uint64_t)frame->budget_us * 1000ULL) {
        errno = ETIMEDOUT;
        return -1;
    }
    *render_time_ns = render_completed_ns - frame->render_started_ns;
    if (monotonic_time_ns(presented_ns) != 0)
        return -1;
    if (commit_completed_ns <= commit_started_ns || *presented_ns <= commit_started_ns) {
        errno = EPROTO;
        return -1;
    }
    {
        uint64_t commit_ns = commit_completed_ns - commit_started_ns;
        uint64_t pageflip_ns = *presented_ns - commit_started_ns;
        if (UINT64_MAX - *atomic_commit_time_ns < commit_ns ||
            UINT64_MAX - display->pageflip_latency_time_ns < pageflip_ns ||
            *atomic_commit_measurements == UINT64_MAX) {
            errno = EOVERFLOW;
            return -1;
        }
        *atomic_commit_time_ns += commit_ns;
        (*atomic_commit_measurements)++;
        display->pageflip_latency_time_ns += pageflip_ns;
        if (pageflip_ns > display->pageflip_latency_max_ns)
            display->pageflip_latency_max_ns = pageflip_ns;
    }
    rustos_gpu_runtime_presented(runtime, frame->output_index);
    display->front_buffer = frame->output_index;
    display->setup_stage = "gpu-ready";
    return 0;

fail_fences:
    close(out_fence_fd);
    close(frame->in_fence_fd);
    frame->in_fence_fd = -1;
    if (result == 0)
        errno = ETIMEDOUT;
    else if (errno == 0)
        errno = EIO;
    return -1;
}

static int open_gpu_kms_display(const struct shared_display *shared,
                                struct kms_display *display,
                                struct rustos_gpu_runtime **runtime_out,
                                uint64_t *prime_duration_ns,
                                int *prime_present_fence) {
    struct rustos_gpu_frame bootstrap;
    int source_fds[GPU_ATLAS_SLOT_COUNT] = {-1, -1, -1};
    uint64_t prime_started_ns;
    uint64_t prime_completed_ns;
    const struct rustos_gpu_backend_policy *backend;
    int saved;
    if (runtime_out == NULL || prime_duration_ns == NULL || prime_present_fence == NULL) {
        errno = EINVAL;
        return -1;
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
    display->source_exporter_fd = -1;
    display->front_buffer = NO_SCANOUT_BUFFER;
    display->source_width = shared->header.width;
    display->source_height = shared->header.height;
    display->setup_stage = "gpu-open-card";
    display->fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC | O_NONBLOCK);
    if (display->fd < 0)
        return -1;
    display->setup_stage = "gpu-set-master";
    if (drmSetMaster(display->fd) != 0 ||
        drmSetClientCap(display->fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1U) != 0 ||
        drmSetClientCap(display->fd, DRM_CLIENT_CAP_ATOMIC, 1U) != 0)
        goto fail;
    display->setup_stage = "gpu-kms-target";
    if (select_kms_target(display->fd, &shared->header, &display->connector_id,
                          &display->crtc_id, &display->mode) != 0 ||
        select_primary_plane(display->fd, display->crtc_id,
                             &display->primary_plane_id) != 0 ||
        load_atomic_property_ids(display) != 0)
        goto fail;
    display->setup_stage = "gpu-egl-gbm";
    if (monotonic_time_ns(&prime_started_ns) != 0)
        goto fail;
    if (rustos_gpu_runtime_open(display->fd, shared->header.width,
                                shared->header.height, shared->atlas.atlas_width,
                                shared->atlas.atlas_height,
                                shared->atlas.atlas_stride_bytes, runtime_out) != 0)
        goto fail;
    backend = rustos_gpu_backend_policy(rustos_gpu_runtime_driver(*runtime_out));
    if (backend == NULL) {
        errno = EOPNOTSUPP;
        goto fail_runtime;
    }
    if (backend->source_mode == GPU_SOURCE_DIRECT_DMABUF) {
        struct stat state;
        size_t slot;
        int exporter;
        display->setup_stage = "gpu-dmabuf-exporter";
        exporter = open(RUSTOS_DVM_DMABUF_DEVICE,
                        O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
        if (exporter < 0 || fstat(exporter, &state) != 0 || !S_ISCHR(state.st_mode) ||
            state.st_uid != 0 || (state.st_mode & 077U) != 0) {
            saved = errno == 0 ? EPERM : errno;
            if (exporter >= 0)
                close(exporter);
            errno = saved;
            goto fail_runtime;
        }
        for (slot = 0U; slot < GPU_ATLAS_SLOT_COUNT; slot++) {
            struct rustos_dvm_dmabuf_request request = {
                .slot = (uint32_t)slot,
                .flags = RUSTOS_DVM_DMABUF_EXPORT_ATLAS,
            };
            source_fds[slot] = ioctl(exporter, RUSTOS_DVM_DMABUF_IOCTL_EXPORT,
                                     &request);
            if (source_fds[slot] < 0 ||
                (fcntl(source_fds[slot], F_GETFD) & FD_CLOEXEC) == 0) {
                saved = errno == 0 ? EPROTO : errno;
                close(exporter);
                errno = saved;
                goto fail_runtime;
            }
        }
        display->setup_stage = "gpu-dmabuf-import";
        if (rustos_gpu_runtime_import_dmabuf_sources(
                *runtime_out, source_fds, GPU_ATLAS_SLOT_COUNT) != 0) {
            saved = errno == 0 ? EIO : errno;
		    display->setup_stage = rustos_gpu_runtime_stage(*runtime_out);
            close(exporter);
            errno = saved;
            goto fail_runtime;
        }
        for (slot = 0U; slot < GPU_ATLAS_SLOT_COUNT; slot++) {
            close(source_fds[slot]);
            source_fds[slot] = -1;
        }
        display->source_exporter_fd = exporter;
    }
    display->setup_stage = "gpu-initial-bootstrap";
    if (rustos_gpu_runtime_render_bootstrap(*runtime_out, &bootstrap) != 0) {
        saved = errno == 0 ? EIO : errno;
        display->setup_stage = rustos_gpu_runtime_stage(*runtime_out);
        rustos_gpu_runtime_close(*runtime_out);
        *runtime_out = NULL;
        errno = saved;
        goto fail;
    }
    if (atomic_gpu_initial_modeset(display, *runtime_out, &bootstrap,
                                   prime_present_fence) != 0) {
        saved = errno == 0 ? EIO : errno;
        rustos_gpu_runtime_close(*runtime_out);
        *runtime_out = NULL;
        errno = saved;
        goto fail;
    }
    if (monotonic_time_ns(&prime_completed_ns) != 0) {
        rustos_gpu_runtime_close(*runtime_out);
        *runtime_out = NULL;
        goto fail;
    }
    if (prime_completed_ns <= prime_started_ns ||
        prime_completed_ns - prime_started_ns > GPU_PIPELINE_PRIME_MAX_NS) {
        rustos_gpu_runtime_close(*runtime_out);
        *runtime_out = NULL;
        errno = prime_completed_ns > prime_started_ns ? ETIMEDOUT : EPROTO;
        goto fail;
    }
    *prime_duration_ns = prime_completed_ns - prime_started_ns;
    display->setup_stage = "gpu-ready";
    return 0;
fail_runtime:
    saved = errno == 0 ? EIO : errno;
    {
        size_t slot;
        for (slot = 0U; slot < GPU_ATLAS_SLOT_COUNT; slot++) {
            if (source_fds[slot] >= 0)
                close(source_fds[slot]);
        }
    }
    rustos_gpu_runtime_close(*runtime_out);
    *runtime_out = NULL;
    errno = saved;
    goto fail;
fail:
    saved = errno == 0 ? EIO : errno;
    if (display->source_exporter_fd >= 0) {
        close(display->source_exporter_fd);
        display->source_exporter_fd = -1;
    }
    if (display->mode_blob_id != 0U)
        (void)drmModeDestroyPropertyBlob(display->fd, display->mode_blob_id);
    (void)drmDropMaster(display->fd);
    close(display->fd);
    display->fd = -1;
    errno = saved;
    return -1;
}

static void close_gpu_kms_display(struct kms_display *display,
                                  struct rustos_gpu_runtime *runtime) {
    rustos_gpu_runtime_close(runtime);
    if (display->source_exporter_fd >= 0)
        close(display->source_exporter_fd);
    if (display->fd >= 0) {
        if (display->mode_blob_id != 0U)
            (void)drmModeDestroyPropertyBlob(display->fd, display->mode_blob_id);
        (void)drmDropMaster(display->fd);
        close(display->fd);
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
    display->source_exporter_fd = -1;
}

struct gpu_submission {
    uint32_t slot;
    uint32_t batch_bytes;
    uint64_t generation;
    uint64_t sequence;
    uint32_t damage_count;
    struct rustos_gpu_damage damage[GPU_ATLAS_MAX_DAMAGE_RECTS];
    const uint8_t *atlas_pixels;
    const uint8_t *batch;
};

static int acquire_gpu_source_fence(const struct kms_display *display,
                                    const struct gpu_submission *submission) {
    struct rustos_dvm_acquire_request request;
    int fd;
    if (display == NULL || submission == NULL || display->source_exporter_fd < 0 ||
        submission->slot >= GPU_ATLAS_SLOT_COUNT || submission->batch == NULL ||
        submission->batch_bytes < 48U) {
        errno = EINVAL;
        return -1;
    }
    memset(&request, 0, sizeof(request));
    request.slot = submission->slot;
    request.generation = submission->generation;
    request.sequence = submission->sequence;
    request.acquire_value = read_le64(submission->batch + 40U);
    if (request.acquire_value == 0U) {
        errno = EPROTO;
        return -1;
    }
    fd = ioctl(display->source_exporter_fd, RUSTOS_DVM_DMABUF_IOCTL_ACQUIRE,
               &request);
    if (fd < 0)
        return -1;
    if ((fcntl(fd, F_GETFD) & FD_CLOEXEC) == 0) {
        int saved = errno == 0 ? EPROTO : errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

static const uint8_t *gpu_pixel_pointer(const struct shared_display *shared,
                                        uint64_t absolute_offset, uint64_t bytes) {
    uint64_t relative;
    if (shared == NULL || shared->pixels == NULL || absolute_offset < GUI_POOL_HEADER_BYTES ||
        absolute_offset > UINT64_MAX - bytes)
        return NULL;
    relative = absolute_offset - GUI_POOL_HEADER_BYTES;
    if (relative > shared->pixel_bytes || bytes > shared->pixel_bytes - relative)
        return NULL;
    return shared->pixels + (size_t)relative;
}

static int select_gpu_submission(const struct shared_display *shared,
                                 struct gpu_submission *submission,
                                 int dmabuf_sources) {
    uint64_t selected_sequence = UINT64_MAX;
    uint32_t selected_slot = UINT32_MAX;
    uint32_t slot;
    const uint8_t *record;
    uint64_t command_offset;
    uint64_t atlas_offset;
    uint64_t batch_offset;
    uint32_t damage_count;
    if (shared == NULL || submission == NULL)
        return -1;
    for (slot = 0U; slot < GPU_ATLAS_SLOT_COUNT; slot++) {
        uint64_t invitation = read_le64(shared->base + GPU_ATLAS_INVITATION_OFFSET +
                                        slot * sizeof(uint64_t));
        uint64_t acknowledged = read_le64(shared->base + GPU_ATLAS_COMPLETION_ACK_OFFSET +
                                          slot * sizeof(uint64_t));
        uint64_t completed = read_le64(shared->base + GPU_ATLAS_COMPLETION_SEQUENCE_OFFSET +
                                       slot * sizeof(uint64_t));
        if (invitation != 0U && invitation != acknowledged && invitation != completed &&
            invitation < selected_sequence) {
            selected_sequence = invitation;
            selected_slot = slot;
        }
    }
    if (selected_slot == UINT32_MAX)
        return 0;
    command_offset = shared->atlas.command_offset +
                     (uint64_t)selected_slot * GPU_ATLAS_COMMAND_SLOT_BYTES;
    record = gpu_pixel_pointer(shared, command_offset, GPU_ATLAS_SUBMIT_BYTES);
    if (record == NULL || memcmp(record, GPU_ATLAS_SUBMIT_MAGIC, 8U) != 0 ||
        read_le32(record + 8U) != GPU_ATLAS_VERSION ||
        read_le32(record + 12U) != GPU_ATLAS_SUBMIT_BYTES ||
        read_le32(record + 16U) != selected_slot ||
        read_le32(record + 20U) < 128U ||
        read_le32(record + 20U) > GPU_ATLAS_COMMAND_SLOT_BYTES - GPU_ATLAS_SUBMIT_BYTES ||
        read_le64(record + 24U) == 0U ||
        read_le64(record + 32U) != selected_sequence ||
        read_le32(record + 40U) == 0U ||
        read_le32(record + 44U) != (dmabuf_sources
            ? GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
            : GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY) ||
        read_le64(record + 48U) == 0U ||
        read_le32(record + 56U) > GPU_ATLAS_MAX_DAMAGE_RECTS ||
        !bytes_all_zero(record + 60U, 4U)) {
        errno = EPROTO;
        return -1;
    }
    damage_count = read_le32(record + 56U);
    batch_offset = command_offset + GPU_ATLAS_SUBMIT_BYTES +
                   (uint64_t)damage_count * GPU_ATLAS_DAMAGE_BYTES;
    if (batch_offset > command_offset + GPU_ATLAS_COMMAND_SLOT_BYTES ||
        read_le32(record + 20U) >
            command_offset + GPU_ATLAS_COMMAND_SLOT_BYTES - batch_offset) {
        errno = EPROTO;
        return -1;
    }
    submission->slot = selected_slot;
    submission->batch_bytes = read_le32(record + 20U);
    submission->generation = read_le64(record + 24U);
    submission->sequence = selected_sequence;
    submission->damage_count = damage_count;
    for (slot = 0U; slot < damage_count; slot++) {
        const uint8_t *encoded = gpu_pixel_pointer(
            shared, command_offset + GPU_ATLAS_SUBMIT_BYTES +
                        (uint64_t)slot * GPU_ATLAS_DAMAGE_BYTES,
            GPU_ATLAS_DAMAGE_BYTES);
        uint64_t x_end;
        uint64_t y_end;
        uint32_t prior;
        if (encoded == NULL) {
            errno = EPROTO;
            return -1;
        }
        submission->damage[slot].x = read_le32(encoded);
        submission->damage[slot].y = read_le32(encoded + 4U);
        submission->damage[slot].width = read_le32(encoded + 8U);
        submission->damage[slot].height = read_le32(encoded + 12U);
        x_end = (uint64_t)submission->damage[slot].x + submission->damage[slot].width;
        y_end = (uint64_t)submission->damage[slot].y + submission->damage[slot].height;
        if (submission->damage[slot].width == 0U ||
            submission->damage[slot].height == 0U ||
            x_end > shared->atlas.atlas_width || y_end > shared->atlas.atlas_height) {
            errno = EPROTO;
            return -1;
        }
        for (prior = 0U; prior < slot; prior++) {
            const struct rustos_gpu_damage *a = &submission->damage[prior];
            const struct rustos_gpu_damage *b = &submission->damage[slot];
            if ((uint64_t)a->x < (uint64_t)b->x + b->width &&
                (uint64_t)b->x < (uint64_t)a->x + a->width &&
                (uint64_t)a->y < (uint64_t)b->y + b->height &&
                (uint64_t)b->y < (uint64_t)a->y + a->height) {
                errno = EPROTO;
                return -1;
            }
        }
    }
    /* The GLES runtime binds full-initialization to its new context rather
     * than to the transport sequence, which remains monotonic across a DVM
     * restart and must never be reused. */
    submission->batch = gpu_pixel_pointer(shared, batch_offset, submission->batch_bytes);
    atlas_offset = shared->atlas.atlas_offset +
                   (uint64_t)selected_slot * shared->atlas.atlas_slot_bytes;
    submission->atlas_pixels = gpu_pixel_pointer(shared, atlas_offset,
                                                  shared->atlas.atlas_slot_bytes);
    if (submission->batch == NULL || submission->atlas_pixels == NULL ||
        memcmp(submission->batch, "RSGPU001", 8U) != 0) {
        errno = EPROTO;
        return -1;
    }
    return 1;
}

static int publish_gpu_completion(const struct shared_display *shared,
                                  const struct rustos_gpu_frame *frame,
                                  uint64_t render_time_ns, uint64_t presented_ns,
                                  uint64_t previous_submit_value) {
    char path[PATH_MAX];
    uint8_t completion[GPU_ATLAS_COMPLETION_BYTES] = {0};
    ssize_t bytes;
    int fd;
    if (shared == NULL || frame == NULL || frame->context_id == 0U ||
        frame->context_epoch == 0U || frame->submit_value == 0U ||
        frame->generation == 0U || frame->sequence == 0U || render_time_ns == 0U ||
        presented_ns == 0U || previous_submit_value >= frame->submit_value ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_GPU_COMPLETION_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    memcpy(completion, GPU_ATLAS_COMPLETION_MAGIC, 8U);
    write_le32(completion + 8U, GPU_ATLAS_VERSION);
    write_le32(completion + 12U, GPU_ATLAS_COMPLETION_BYTES);
    if (frame->source_slot >= GPU_ATLAS_SLOT_COUNT ||
        read_le64(shared->base + GPU_ATLAS_INVITATION_OFFSET +
                  frame->source_slot * sizeof(uint64_t)) != frame->sequence) {
        errno = EPROTO;
        return -1;
    }
    write_le32(completion + 16U, frame->source_slot);
    write_le32(completion + 20U, 3U);
    write_le64(completion + 24U, frame->generation);
    write_le64(completion + 32U, frame->sequence);
    memcpy(completion + 64U, GPU_RENDER_COMPLETION_MAGIC, 8U);
    write_le32(completion + 72U, GPU_RENDER_VERSION);
    write_le32(completion + 76U, 64U);
    write_le32(completion + 80U, frame->context_id);
    write_le32(completion + 84U, frame->context_epoch);
    write_le32(completion + 88U, 1U);
    write_le32(completion + 92U, frame->output_index);
    write_le64(completion + 96U, frame->submit_value);
    write_le64(completion + 104U, frame->submit_value);
    write_le64(completion + 112U, render_time_ns);
    write_le64(completion + 120U, frame->submit_value);
    memcpy(completion + 128U, GPU_PRESENT_COMPLETION_MAGIC, 8U);
    write_le32(completion + 136U, GPU_RENDER_VERSION);
    write_le32(completion + 140U, 64U);
    write_le32(completion + 144U, frame->context_id);
    write_le32(completion + 148U, frame->context_epoch);
    write_le32(completion + 152U, frame->output_index);
    write_le64(completion + 160U, frame->submit_value);
    write_le64(completion + 168U, frame->submit_value);
    write_le64(completion + 176U, previous_submit_value);
    write_le64(completion + 184U, presented_ns);
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0)
        return -1;
    bytes = write(fd, completion, sizeof(completion));
    close(fd);
    if (bytes != (ssize_t)sizeof(completion)) {
        if (bytes >= 0)
            errno = EIO;
        return -1;
    }
    return 0;
}

static int publish_gpu_prime(const struct shared_display *shared,
                             uint64_t prime_duration_ns,
                             int dmabuf_sources) {
    char path[PATH_MAX];
    uint8_t completion[GPU_PRIME_COMPLETION_BYTES] = {0};
    uint32_t context_id;
    uint32_t context_epoch;
    uint64_t fence_value;
    ssize_t bytes;
    int fd;
    if (shared == NULL || prime_duration_ns == 0U ||
        prime_duration_ns > GPU_PIPELINE_PRIME_MAX_NS) {
        errno = EINVAL;
        return -1;
    }
    context_id = read_le32(shared->base + GPU_ATLAS_CONTEXT_ID_OFFSET);
    context_epoch = read_le32(shared->base + GPU_ATLAS_CONTEXT_EPOCH_OFFSET);
    fence_value = read_le64(shared->base + GPU_ATLAS_PRIME_FENCE_OFFSET);
    if (context_id == 0U || context_epoch == 0U || fence_value == 0U ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_GPU_PRIME_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EPROTO;
        return -1;
    }
    memcpy(completion, GPU_PRIME_COMPLETION_MAGIC, 8U);
    write_le32(completion + 8U, GPU_PRIME_COMPLETION_VERSION);
    write_le32(completion + 12U, GPU_PRIME_COMPLETION_BYTES);
    write_le32(completion + 16U, context_id);
    write_le32(completion + 20U, context_epoch);
    write_le32(completion + 24U, 1U);
    write_le32(completion + 28U, dmabuf_sources
        ? GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
        : GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY);
    write_le64(completion + 32U, fence_value);
    write_le64(completion + 40U, prime_duration_ns);
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0)
        return -1;
    bytes = write(fd, completion, sizeof(completion));
    close(fd);
    if (bytes != (ssize_t)sizeof(completion)) {
        if (bytes >= 0)
            errno = EIO;
        return -1;
    }
    return 0;
}

static int acknowledge_gpu_host_invitation(const struct shared_display *shared,
                                            uint64_t prime_duration_ns,
                                            int dmabuf_sources) {
    if (publish_gpu_prime(shared, prime_duration_ns, dmabuf_sources) != 0)
        return -1;
    return acknowledge_host_invitation(shared);
}

static int serve_gpu_display(struct shared_display *shared) {
    struct kms_display display;
    struct display_scheduler_guard scheduler = {0};
    struct rustos_gpu_runtime *runtime = NULL;
    uint64_t front_submit_value = 0U;
    uint64_t presented_frames = 0U;
    uint64_t atomic_commit_time_ns = 0U;
    uint64_t atomic_commit_measurements = 0U;
    uint64_t last_reported_ns;
    uint64_t last_pageflip_completions = 0U;
    uint64_t last_atomic_commit_time_ns = 0U;
    uint64_t last_atomic_commit_measurements = 0U;
    uint64_t last_pageflip_latency_time_ns = 0U;
    uint64_t sample_sequence = 0U;
    struct gpu_relay_metrics gpu_metrics = {0U};
    uint64_t prime_duration_ns = 0U;
    int prime_present_fence = 0;
    int peer_ready_sent = 0;
    int peer_ready_confirmed = 0;
    int active_logged = 0;
    int ready_lock = -1;
    if (open_gpu_kms_display(shared, &display, &runtime, &prime_duration_ns,
                             &prime_present_fence) != 0) {
        relay_log("rustos-dvm-display: GPU KMS setup unavailable stage=%s errno=%d\n",
                  display.setup_stage == NULL ? "unknown" : display.setup_stage, errno);
        return -1;
    }
    const int dmabuf_sources = rustos_gpu_runtime_uses_dmabuf_sources(runtime);
    shared->uio_fd = open_uio_interrupt(shared);
    if (shared->uio_fd < 0 || map_gpu_pixel_pool(shared) != 0) {
        relay_log("rustos-dvm-display: GPU read-only pixel mapping unavailable errno=%d\n",
                  errno);
        close_gpu_kms_display(&display, runtime);
        return -1;
    }
    relay_log("rustos-dvm-display: gpu-compositor primed contract=3 driver=%s renderer=%s "
              "source-path=%s zero-copy=%u explicit-fence=1 public-abi=0 bootstrap=local-nonblack prime_us=%llu prime-present=%s\n",
              rustos_gpu_runtime_driver(runtime), rustos_gpu_runtime_renderer(runtime),
              dmabuf_sources ? "dmabuf" : "staged-copy", dmabuf_sources ? 1U : 0U,
              (unsigned long long)((prime_duration_ns + 999U) / 1000U),
              prime_present_fence ? "out-fence" : "blocking-atomic");
    if (monotonic_time_ns(&last_reported_ns) != 0)
        goto fail;
    for (;;) {
        uint32_t event_count;
        ssize_t read_bytes;
        int invitation_pending = 0;
        if (!peer_ready_sent) {
            if (host_invitation_pending(shared, &invitation_pending) != 0)
                break;
            if (invitation_pending) {
                if (acknowledge_gpu_host_invitation(shared, prime_duration_ns,
                                                    dmabuf_sources) != 0)
                    break;
                peer_ready_sent = 1;
            }
        }
        do {
            read_bytes = read(shared->uio_fd, &event_count, sizeof(event_count));
        } while (read_bytes < 0 && errno == EINTR);
        if (read_bytes != (ssize_t)sizeof(event_count) || event_count == 0U)
            break;
        if (!peer_ready_sent) {
            if (acknowledge_gpu_host_invitation(shared, prime_duration_ns,
                                                dmabuf_sources) != 0)
                break;
            peer_ready_sent = 1;
        }
        if (!peer_ready_confirmed && host_confirmed_peer_ready(shared)) {
            if (display_scheduler_enter(&scheduler) != 0) {
                relay_log(
                    "rustos-dvm-display: scheduler unavailable policy=rr priority=%u errno=%d\n",
                    DISPLAY_RELAY_RR_PRIORITY, errno);
                goto fail;
            }
            peer_ready_confirmed = 1;
            relay_log(
                "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n");
            relay_log(
                "rustos-dvm-display: scheduler admitted policy=rr priority=%u rttime_soft_us=%u rttime_hard_us=%u rttime_hard_action=terminate\n",
                DISPLAY_RELAY_RR_PRIORITY, DISPLAY_RELAY_RTTIME_SOFT_US,
                DISPLAY_RELAY_RTTIME_HARD_US);
        }
        for (;;) {
            struct gpu_submission submission;
            struct rustos_gpu_frame frame;
            uint64_t presented_ns;
            uint64_t render_time_ns;
            int acquire_fence_fd = -1;
            int selected = select_gpu_submission(shared, &submission, dmabuf_sources);
            if (selected < 0)
                goto fail;
            if (selected == 0)
                break;
            if (dmabuf_sources) {
                display.setup_stage = "gpu-dmabuf-acquire";
                acquire_fence_fd = acquire_gpu_source_fence(&display, &submission);
                if (acquire_fence_fd < 0)
                    goto fail;
            }
            if (rustos_gpu_runtime_render_batch(runtime, submission.atlas_pixels,
                    (size_t)shared->atlas.atlas_slot_bytes, submission.damage,
                    submission.damage_count, submission.batch,
                    submission.batch_bytes, submission.slot, submission.generation,
                    submission.sequence, acquire_fence_fd, &frame) != 0 ||
                atomic_gpu_page_flip(&display, runtime, &frame, &presented_ns,
                                     &render_time_ns, &atomic_commit_time_ns,
                                     &atomic_commit_measurements) != 0 ||
                publish_gpu_completion(shared, &frame, render_time_ns, presented_ns,
                                       front_submit_value) != 0)
                goto fail;
            front_submit_value = frame.submit_value;
            presented_frames++;
            if (gpu_metrics.render_time_ns > UINT64_MAX - render_time_ns ||
                gpu_metrics.render_measurements == UINT64_MAX) {
                errno = EOVERFLOW;
                goto fail;
            }
            gpu_metrics.render_time_ns += render_time_ns;
            gpu_metrics.render_measurements++;
            if (render_time_ns > gpu_metrics.render_max_ns)
                gpu_metrics.render_max_ns = render_time_ns;
            if (ready_lock < 0 && peer_ready_confirmed) {
                ready_lock = publish_display_ready_lock(dmabuf_sources);
                if (ready_lock < 0)
                    goto fail;
            }
            if (!active_logged && ready_lock >= 0) {
                relay_log(
                    "rustos-dvm-display: active width=%u height=%u stride=%u format=BGRA8888 event=ivshmem-msix-uio irq_count=%u source-path=%s zero-copy=%u gpu-composition=1 explicit-fence=1 scanout_buffers=%u cpu-final-compose=0 staged-damage-copy=%u\n",
                    shared->header.width, shared->header.height,
                    shared->header.stride_bytes, event_count,
                    dmabuf_sources ? "dmabuf" : "staged-copy",
                    dmabuf_sources ? 1U : 0U, SCANOUT_BUFFER_COUNT,
                    dmabuf_sources ? 0U : 1U);
                active_logged = 1;
            }
            if (presented_frames == 1U || presented_frames % GPU_FRAME_LOG_INTERVAL == 0U) {
                relay_log("rustos-dvm-display: gpu-frame sequence=%llu submit=%llu "
                          "output=%u render_us=%llu source-path=%s zero-copy=%u "
                          "gpu-fence=1 present-fence=1\n",
                          (unsigned long long)frame.sequence,
                          (unsigned long long)frame.submit_value, frame.output_index,
                          (unsigned long long)((render_time_ns + 999U) / 1000U),
                          dmabuf_sources ? "dmabuf" : "staged-copy",
                          dmabuf_sources ? 1U : 0U);
            }
            if (report_relay_stats(&display, dmabuf_sources, presented_frames,
                                   atomic_commit_time_ns,
                                   atomic_commit_measurements, &last_reported_ns,
                                   &last_pageflip_completions,
                                   &last_atomic_commit_time_ns,
                                   &last_atomic_commit_measurements,
                                   &last_pageflip_latency_time_ns, &sample_sequence,
                                   &gpu_metrics) != 0)
                goto fail;
        }
        if (report_relay_stats(&display, dmabuf_sources, presented_frames,
                               atomic_commit_time_ns,
                               atomic_commit_measurements, &last_reported_ns,
                               &last_pageflip_completions, &last_atomic_commit_time_ns,
                               &last_atomic_commit_measurements,
                               &last_pageflip_latency_time_ns, &sample_sequence,
                               &gpu_metrics) != 0)
            goto fail;
    }
fail:
    if (ready_lock >= 0) {
        close(ready_lock);
        ready_lock = -1;
    }
    {
        int relay_errno = errno;
        if (display_scheduler_leave(&scheduler) != 0)
            relay_log("rustos-dvm-display: scheduler restore failed errno=%d\n", errno);
        errno = relay_errno;
    }
    relay_log("rustos-dvm-display: gpu-compositor offline frames=%llu stage=%s "
              "gpu-stage=%s errno=%d\n",
              (unsigned long long)presented_frames,
              display.setup_stage == NULL ? "unknown" : display.setup_stage,
              rustos_gpu_runtime_stage(runtime), errno);
    report_host_offline(shared);
    close_gpu_kms_display(&display, runtime);
    return scheduler.fatal ? DISPLAY_SERVE_FATAL : DISPLAY_SERVE_RETRY;
}

static int serve_display(void) {
    struct shared_display shared;
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE);
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
    if (open_shared_display(&shared) != 0) {
        relay_log("rustos-dvm-display: shared aperture unavailable errno=%d\n", errno);
        return -1;
    }
    if (revoke_predecessor_lease(&shared) != 0) {
        relay_log("rustos-dvm-display: predecessor lease revoke failed errno=%d\n", errno);
        close_shared_display(&shared);
        return -1;
    }
    int gpu_result = serve_gpu_display(&shared);
    close_shared_display(&shared);
    return gpu_result;
}

int main(int argc, char **argv) {
    int owner_fd;
    if (argc != 2 || strcmp(argv[1], "serve") != 0) {
        fprintf(stderr, "usage: %s serve\n", argv[0]);
        return EXIT_FAILURE;
    }
    owner_fd = claim_display_process_owner();
    if (owner_fd < 0) {
        relay_log("rustos-dvm-display: process owner unavailable errno=%d\n", errno);
        return EXIT_FAILURE;
    }
    for (;;) {
        int result = serve_display();
        if (result == DISPLAY_SERVE_FATAL) {
            relay_log("rustos-dvm-display: fatal scheduler restore failure; exiting\n");
            close(owner_fd);
            return EXIT_FAILURE;
        }
        if (result != 0) {
            sleep(DISPLAY_RETRY_SECONDS);
        }
    }
}
