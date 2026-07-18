// SPDX-License-Identifier: MIT
// DVM-owned DRM/KMS consumer for the fixed RustOS ivshmem display contract.
//
// This process deliberately has no host-control, input, or device-management
// protocol. It reads only the module-validated, cacheable read-only pixel pool;
// control stays in the kernel module. Each immutable, page-aligned snapshot is
// exported as a read-only DMA-BUF and submitted directly through DRM/KMS.

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
#include <sys/ioctl.h>
#include <sys/file.h>
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

#define IVSHMEM_VENDOR_ID "0x1af4"
#define IVSHMEM_DEVICE_ID "0x1110"
#define IVSHMEM_RESOURCE_INDEX 2U
#define RUSTOS_IVSHMEM_UIO_NAME "rustos-dvm-ivshmem-uio"
#define RUSTOS_DVM_HOST_INVITED_ATTRIBUTE "rustos_dvm_host_invited"
#define RUSTOS_DVM_DISPLAY_READY_ATTRIBUTE "rustos_dvm_display_ready"
#define RUSTOS_DVM_DISPLAY_CONTROL_ATTRIBUTE "rustos_dvm_display_control"
#define RUSTOS_DVM_DISPLAY_OFFLINE_ATTRIBUTE "rustos_dvm_display_offline"
#define RUSTOS_DVM_GPU_PRIME_ATTRIBUTE "rustos_dvm_gpu_prime"
#define RUSTOS_DVM_GPU_COMPLETION_ATTRIBUTE "rustos_dvm_gpu_completion"
#define RUSTOS_DVM_DMABUF_DEVICE "/dev/rustos-dvm-display-dmabuf"
#define RUSTOS_DVM_DISPLAY_READY_LOCK "/run/rustos-dvm/display-ready.lock"
#define RUSTOS_DVM_DISPLAY_EVIDENCE "/run/rustos-dvm/display-evidence-v1.env"
#define RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP "/run/rustos-dvm/display-evidence-v1.env.tmp"
#define RUSTOS_DVM_DMABUF_IOCTL_EXPORT _IOW('R', 0x41, struct rustos_dvm_dmabuf_request)
#define IVSHMEM_UIO_VECTOR_BYTES 4U
#define GUI_POOL_MAGIC "RSGUI002"
#define GUI_POOL_VERSION 2U
#define GUI_POOL_HEADER_BYTES 4096U
#define GUI_POOL_RECORD_BYTES 64U
#define GUI_POOL_SLOT_COUNT 3U
#define GUI_POOL_HOST_RECORD_OFFSET 64U
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
#define GUI_MESSAGE_MAGIC "RSGUI001"
#define GUI_MESSAGE_VERSION 1U
#define GUI_MESSAGE_KIND_PRESENT 1U
#define SCANOUT_BUFFER_COUNT 3U
#define NO_SCANOUT_BUFFER UINT32_MAX

/*
 * Multiple DVM services share the serial tty. stdio may split one formatted
 * line into several writes, allowing another process to splice bytes into a
 * machine-checked readiness or timing record. Emit every relay record through
 * one bounded write so log corruption cannot create a false failure/success.
 */
static void relay_log(const char *format, ...) __attribute__((format(printf, 1, 2)));

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
#define GUI_MESSAGE_KIND_RELEASE 2U
#define GUI_DAMAGE_FULL 1U
#define DISPLAY_BYTES_PER_PIXEL 4U
#define GUI_POOL_MAX_REGION_BYTES (128U * 1024U * 1024U)
#define DISPLAY_STATS_INTERVAL_NS (1000ULL * 1000ULL * 1000ULL)
#define GPU_FRAME_LOG_INTERVAL 120U
#define DISPLAY_RELAY_RR_PRIORITY 9
#define DISPLAY_RELAY_RTTIME_SOFT_US 50000U
#define DISPLAY_RELAY_RTTIME_HARD_US 100000U

struct display_scheduler_guard {
    int active;
    int saved_policy;
    struct sched_param saved_param;
    struct rlimit saved_rttime;
};

static int display_scheduler_leave(struct display_scheduler_guard *guard) {
    int failed = 0;
    int saved_errno = 0;

    if (!guard->active)
        return 0;
    if (sched_setscheduler(0, guard->saved_policy, &guard->saved_param) != 0) {
        failed = 1;
        saved_errno = errno;
    }
    if (setrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0 && !failed) {
        failed = 1;
        saved_errno = errno;
    }
    guard->active = 0;
    if (failed) {
        errno = saved_errno;
        return -1;
    }
    return 0;
}

/*
 * The authenticated display relay is the only latency-critical thread in this
 * process. Admit it below the input relay's priority and bound continuous CPU
 * time so a wedged Mesa/DRM path cannot starve DVM recovery or control work.
 */
static int display_scheduler_enter(struct display_scheduler_guard *guard) {
    struct rlimit bounded_rttime = {
        .rlim_cur = DISPLAY_RELAY_RTTIME_SOFT_US,
        .rlim_max = DISPLAY_RELAY_RTTIME_HARD_US,
    };
    struct sched_param realtime = {.sched_priority = DISPLAY_RELAY_RR_PRIORITY};
    struct sched_param observed;
    int observed_policy;
    int saved_errno;

    memset(guard, 0, sizeof(*guard));
    guard->saved_policy = sched_getscheduler(0);
    if (guard->saved_policy < 0 || sched_getparam(0, &guard->saved_param) != 0 ||
        getrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0)
        return -1;
    if (setrlimit(RLIMIT_RTTIME, &bounded_rttime) != 0)
        return -1;
    if (sched_setscheduler(0, SCHED_RR, &realtime) != 0) {
        saved_errno = errno;
        (void)setrlimit(RLIMIT_RTTIME, &guard->saved_rttime);
        errno = saved_errno;
        return -1;
    }
    guard->active = 1;
    observed_policy = sched_getscheduler(0);
    if (observed_policy != SCHED_RR || sched_getparam(0, &observed) != 0 ||
        observed.sched_priority != DISPLAY_RELAY_RR_PRIORITY) {
        saved_errno = errno != 0 ? errno : EINVAL;
        (void)display_scheduler_leave(guard);
        errno = saved_errno;
        return -1;
    }
    return 0;
}
#define DISPLAY_PAGEFLIP_TIMEOUT_MS 100
#define DISPLAY_PAGEFLIP_TIMEOUT_NS \
    ((uint64_t)DISPLAY_PAGEFLIP_TIMEOUT_MS * 1000ULL * 1000ULL)
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

struct display_damage {
    uint32_t x;
    uint32_t y;
    uint32_t width;
    uint32_t height;
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

struct scanout_buffer {
    uint32_t handle;
    uint32_t pitch;
    uint64_t bytes;
    uint32_t framebuffer_id;
    uint8_t *map;
    int imported;
};

struct rustos_dvm_dmabuf_request {
    uint32_t slot;
    uint32_t flags;
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

struct pending_scanout {
    int active;
    int page_flip_complete;
    int switch_complete;
    uint32_t slot;
    uint32_t target_buffer;
    uint32_t release_slot;
    uint64_t generation;
    uint64_t release_generation;
    uint64_t submitted_ns;
    struct display_damage damage;
};

struct kms_display {
    int fd;
    const char *setup_stage;
    uint32_t connector_id;
    uint32_t crtc_id;
    uint32_t primary_plane_id;
    uint32_t mode_blob_id;
    drmModeModeInfo mode;
    struct atomic_property_ids properties;
    struct scanout_buffer scanout[SCANOUT_BUFFER_COUNT];
    struct scanout_buffer bootstrap;
    uint64_t buffer_generation[SCANOUT_BUFFER_COUNT];
    uint32_t front_buffer;
    uint32_t source_width;
    uint32_t source_height;
    uint64_t pageflip_latency_time_ns;
    uint64_t pageflip_latency_max_ns;
    struct pending_scanout pending;
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

static int read_present_record(const struct shared_display *shared, uint32_t expected_slot,
                               struct display_damage *damage, uint64_t *generation) {
    uint8_t record[GUI_POOL_RECORD_BYTES];
    const volatile uint8_t *source = shared->base + GUI_POOL_HOST_RECORD_OFFSET +
                                     expected_slot * GUI_POOL_RECORD_BYTES;
    uint64_t x_end;
    uint64_t y_end;
    unsigned int index;
    for (index = 0U; index < sizeof(record); index++) {
        record[index] = source[index];
    }
    if (memcmp(record, GUI_MESSAGE_MAGIC, 8U) != 0 ||
        read_le32(record + 8U) != GUI_MESSAGE_VERSION ||
        read_le32(record + 12U) != GUI_MESSAGE_KIND_PRESENT ||
        read_le32(record + 16U) != expected_slot || !bytes_all_zero(record + 20U, 4U) ||
        read_le64(record + 24U) == 0U || (read_le64(record + 24U) & 1U) != 0U ||
        !bytes_all_zero(record + 56U, 8U)) {
        return 0;
    }
    damage->x = read_le32(record + 32U);
    damage->y = read_le32(record + 36U);
    damage->width = read_le32(record + 40U);
    damage->height = read_le32(record + 44U);
    damage->flags = read_le32(record + 48U);
    if (damage->flags == GUI_DAMAGE_FULL) {
        if (damage->x != 0U || damage->y != 0U || damage->width != 0U || damage->height != 0U) {
            return 0;
        }
    } else if (damage->flags == 0U && damage->width != 0U && damage->height != 0U) {
        x_end = (uint64_t)damage->x + damage->width;
        y_end = (uint64_t)damage->y + damage->height;
        if (x_end > shared->header.width || y_end > shared->header.height) {
            return 0;
        }
    } else {
        return 0;
    }
    *generation = read_le64(record + 24U);
    return 1;
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

static int wait_for_relay_event(int uio_fd, int drm_fd, int timeout_ms, uint32_t *event_count,
                                int *uio_ready, int *drm_ready) {
    struct pollfd pollfds[2] = {
        {.fd = uio_fd, .events = POLLIN, .revents = 0},
        {.fd = drm_fd, .events = POLLIN, .revents = 0},
    };
    uint32_t count;
    int ready;
    ssize_t bytes;
    if (event_count == NULL || uio_ready == NULL || drm_ready == NULL) {
        errno = EINVAL;
        return -1;
    }
    do {
        ready = poll(pollfds, 2U, timeout_ms);
    } while (ready < 0 && errno == EINTR);
    if (ready <= 0 || (pollfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
        (pollfds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
        if (ready == 0) {
            errno = ETIMEDOUT;
        }
        return -1;
    }
    *uio_ready = (pollfds[0].revents & POLLIN) != 0;
    *drm_ready = (pollfds[1].revents & POLLIN) != 0;
    *event_count = 0U;
    if (*uio_ready) {
        bytes = read(uio_fd, &count, IVSHMEM_UIO_VECTOR_BYTES);
        if (bytes != IVSHMEM_UIO_VECTOR_BYTES) {
            if (bytes >= 0) {
                errno = EIO;
            }
            return -1;
        }
        if (count == 0U) {
            errno = EIO;
            return -1;
        }
        *event_count = count;
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

static void report_host_offline(const struct shared_display *shared) {
    char path[PATH_MAX];
    static const char offline[] = "offline\n";
    int fd;
    if (shared == NULL ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_DISPLAY_OFFLINE_ATTRIBUTE) >= (int)sizeof(path)) {
        return;
    }
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return;
    }
    (void)write(fd, offline, sizeof(offline) - 1U);
    close(fd);
}

static int host_confirmed_peer_ready(const struct shared_display *shared) {
    uint64_t invitation = read_le64(shared->base + GUI_POOL_INVITATION_OFFSET);
    uint64_t confirmed = read_le64(shared->base + GUI_POOL_READY_CONFIRMATION_OFFSET);
    return invitation != 0U && (invitation & 1U) == 0U && invitation == confirmed;
}

static int release_surface(const struct shared_display *shared, uint32_t slot,
                           uint64_t generation) {
    char path[PATH_MAX];
    uint8_t record[GUI_POOL_RECORD_BYTES] = {0};
    ssize_t bytes;
    int fd;
    if (shared == NULL || slot >= GUI_POOL_SLOT_COUNT || generation == 0U ||
        (generation & 1U) != 0U ||
        snprintf(path, sizeof(path), "/sys/bus/pci/devices/%s/%s", shared->pci_bdf,
                 RUSTOS_DVM_DISPLAY_CONTROL_ATTRIBUTE) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    memcpy(record, GUI_MESSAGE_MAGIC, 8U);
    write_le32(record + 8U, GUI_MESSAGE_VERSION);
    write_le32(record + 12U, GUI_MESSAGE_KIND_RELEASE);
    write_le32(record + 16U, slot);
    write_le64(record + 24U, generation);
    write_le32(record + 48U, GUI_DAMAGE_FULL);
    fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    bytes = write(fd, record, sizeof(record));
    close(fd);
    if (bytes != (ssize_t)sizeof(record)) {
        if (bytes >= 0) {
            errno = EIO;
        }
        return -1;
    }
    return 0;
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
        drmModeEncoder *encoder;
        int mode_index;
        if (connector == NULL) {
            continue;
        }
        if (connector->connection != DRM_MODE_CONNECTED || connector->count_modes == 0 ||
            connector->encoder_id == 0U) {
            drmModeFreeConnector(connector);
            continue;
        }
        encoder = drmModeGetEncoder(fd, connector->encoder_id);
        if (encoder == NULL) {
            drmModeFreeConnector(connector);
            continue;
        }
        uint32_t candidate_crtc = select_crtc(resources, encoder);
        if (candidate_crtc != 0U && fallback_connector == 0U) {
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
        drmModeFreeEncoder(encoder);
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

static int atomic_initial_modeset(struct kms_display *display) {
    drmModeAtomicReq *request;
    int result;
    if (drmModeCreatePropertyBlob(display->fd, &display->mode, sizeof(display->mode),
                                  &display->mode_blob_id) != 0) {
        return -1;
    }
    request = drmModeAtomicAlloc();
    if (request == NULL) {
        return -1;
    }
    result = drmModeAtomicAddProperty(request, display->connector_id,
                                      display->properties.connector_crtc_id, display->crtc_id) < 0 ||
             drmModeAtomicAddProperty(request, display->crtc_id, display->properties.crtc_mode_id,
                                      display->mode_blob_id) < 0 ||
             drmModeAtomicAddProperty(request, display->crtc_id, display->properties.crtc_active,
                                      1U) < 0 ||
             add_plane_properties(display, request, display->bootstrap.framebuffer_id, 0U) != 0
                 ? -1
                 : drmModeAtomicCommit(display->fd, request, DRM_MODE_ATOMIC_ALLOW_MODESET, NULL);
    drmModeAtomicFree(request);
    return result;
}

static void destroy_scanout_buffer(int fd, struct scanout_buffer *buffer) {
    struct drm_mode_destroy_dumb destroy = {.handle = buffer->handle};
    if (buffer->map != NULL) {
        munmap(buffer->map, (size_t)buffer->bytes);
    }
    if (buffer->framebuffer_id != 0U) {
        (void)drmModeRmFB(fd, buffer->framebuffer_id);
    }
    if (buffer->handle != 0U && buffer->imported) {
        (void)drmCloseBufferHandle(fd, buffer->handle);
    } else if (buffer->handle != 0U) {
        (void)drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    }
    memset(buffer, 0, sizeof(*buffer));
}

static int create_direct_scanout_buffer(int drm_fd, int dmabuf_device, uint32_t slot,
                                        const struct gui_pool_header *header,
                                        struct scanout_buffer *buffer) {
    struct rustos_dvm_dmabuf_request request = {.slot = slot, .flags = 0U};
    uint32_t handles[4] = {0U};
    uint32_t pitches[4] = {0U};
    uint32_t offsets[4] = {0U};
    int prime_fd;

    memset(buffer, 0, sizeof(*buffer));
    prime_fd = ioctl(dmabuf_device, RUSTOS_DVM_DMABUF_IOCTL_EXPORT, &request);
    if (prime_fd < 0)
        return -1;
    if (drmPrimeFDToHandle(drm_fd, prime_fd, &buffer->handle) != 0) {
        close(prime_fd);
        return -1;
    }
    close(prime_fd);
    buffer->imported = 1;
    buffer->pitch = header->stride_bytes;
    buffer->bytes = header->slot_bytes;
    handles[0] = buffer->handle;
    pitches[0] = buffer->pitch;
    if (drmModeAddFB2(drm_fd, header->width, header->height, DRM_FORMAT_XRGB8888, handles,
                      pitches, offsets, &buffer->framebuffer_id, 0U) != 0) {
        destroy_scanout_buffer(drm_fd, buffer);
        return -1;
    }
    return 0;
}

static int create_scanout_buffer(int fd, uint32_t width, uint32_t height,
                                 struct scanout_buffer *buffer) {
    struct drm_mode_create_dumb create = {
        .width = width,
        .height = height,
        .bpp = 32U,
    };
    struct drm_mode_map_dumb map = {0};
    uint32_t handles[4] = {0};
    uint32_t pitches[4] = {0};
    uint32_t offsets[4] = {0};
    if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) != 0) {
        return -1;
    }
    buffer->handle = create.handle;
    buffer->pitch = create.pitch;
    buffer->bytes = create.size;
    if (buffer->pitch < width * DISPLAY_BYTES_PER_PIXEL ||
        create.size < (uint64_t)width * height * DISPLAY_BYTES_PER_PIXEL) {
        errno = EOVERFLOW;
        destroy_scanout_buffer(fd, buffer);
        return -1;
    }
    handles[0] = buffer->handle;
    pitches[0] = buffer->pitch;
    if (drmModeAddFB2(fd, width, height, DRM_FORMAT_XRGB8888, handles, pitches,
                      offsets, &buffer->framebuffer_id, 0U) != 0) {
        if (drmModeAddFB(fd, width, height, 24U, 32U, buffer->pitch, buffer->handle,
                         &buffer->framebuffer_id) != 0) {
            destroy_scanout_buffer(fd, buffer);
            return -1;
        }
    }
    map.handle = buffer->handle;
    if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) != 0) {
        destroy_scanout_buffer(fd, buffer);
        return -1;
    }
    buffer->map = mmap(NULL, (size_t)buffer->bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                       (off_t)map.offset);
    if (buffer->map == MAP_FAILED) {
        buffer->map = NULL;
        destroy_scanout_buffer(fd, buffer);
        return -1;
    }
    return 0;
}

static int open_kms_display(const struct gui_pool_header *header, struct kms_display *display) {
    uint32_t buffer_index;
    uint64_t prime_capability = 0U;
    int dmabuf_device;
    int saved;
    memset(display, 0, sizeof(*display));
    display->fd = -1;
    display->setup_stage = "open-card";
    display->fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC | O_NONBLOCK);
    if (display->fd < 0) {
        return -1;
    }
    display->front_buffer = NO_SCANOUT_BUFFER;
    display->source_width = header->width;
    display->source_height = header->height;
    display->pending.target_buffer = NO_SCANOUT_BUFFER;
    display->pending.release_slot = NO_SCANOUT_BUFFER;
    display->setup_stage = "set-master";
    if (drmSetMaster(display->fd) != 0)
        goto fail_fd;
    display->setup_stage = "universal-planes";
    if (drmSetClientCap(display->fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1U) != 0)
        goto fail_fd;
    display->setup_stage = "atomic-capability";
    if (drmSetClientCap(display->fd, DRM_CLIENT_CAP_ATOMIC, 1U) != 0)
        goto fail_fd;
    display->setup_stage = "kms-target";
    if (select_kms_target(display->fd, header, &display->connector_id, &display->crtc_id,
                          &display->mode) != 0)
        goto fail_fd;
    display->setup_stage = "primary-plane";
    if (select_primary_plane(display->fd, display->crtc_id, &display->primary_plane_id) != 0)
        goto fail_fd;
    display->setup_stage = "atomic-properties";
    if (load_atomic_property_ids(display) != 0)
        goto fail_fd;
    display->setup_stage = "prime-capability";
    if (drmGetCap(display->fd, DRM_CAP_PRIME, &prime_capability) != 0)
        goto fail_fd;
    if ((prime_capability & DRM_PRIME_CAP_IMPORT) == 0U) {
        errno = EOPNOTSUPP;
        goto fail_fd;
    }
    display->setup_stage = "open-dmabuf-exporter";
    dmabuf_device = open(RUSTOS_DVM_DMABUF_DEVICE, O_RDONLY | O_CLOEXEC);
    if (dmabuf_device < 0)
        goto fail_fd;
    display->setup_stage = "bootstrap-buffer";
    if (create_scanout_buffer(display->fd, header->width, header->height,
                              &display->bootstrap) != 0) {
        saved = errno == 0 ? EIO : errno;
        close(dmabuf_device);
        close(display->fd);
        display->fd = -1;
        errno = saved;
        return -1;
    }
    display->setup_stage = "direct-dmabuf-import";
    for (buffer_index = 0U; buffer_index < SCANOUT_BUFFER_COUNT; buffer_index++) {
        if (create_direct_scanout_buffer(display->fd, dmabuf_device, buffer_index, header,
                                         &display->scanout[buffer_index]) != 0) {
            saved = errno == 0 ? EIO : errno;
            while (buffer_index > 0U) {
                buffer_index--;
                destroy_scanout_buffer(display->fd, &display->scanout[buffer_index]);
            }
            destroy_scanout_buffer(display->fd, &display->bootstrap);
            close(dmabuf_device);
            close(display->fd);
            display->fd = -1;
            errno = saved;
            return -1;
        }
    }
    close(dmabuf_device);
    display->setup_stage = "initial-atomic-modeset";
    if (atomic_initial_modeset(display) != 0) {
        saved = errno == 0 ? EIO : errno;
        for (buffer_index = 0U; buffer_index < SCANOUT_BUFFER_COUNT; buffer_index++) {
            destroy_scanout_buffer(display->fd, &display->scanout[buffer_index]);
        }
        destroy_scanout_buffer(display->fd, &display->bootstrap);
        if (display->mode_blob_id != 0U) {
            (void)drmModeDestroyPropertyBlob(display->fd, display->mode_blob_id);
            display->mode_blob_id = 0U;
        }
        close(display->fd);
        display->fd = -1;
        errno = saved;
        return -1;
    }
    display->setup_stage = "ready";
    return 0;

fail_fd:
    saved = errno == 0 ? EIO : errno;
    close(display->fd);
    display->fd = -1;
    errno = saved;
    return -1;
}

static void close_kms_display(struct kms_display *display) {
    uint32_t buffer_index;
    if (display->fd >= 0) {
        if (display->mode_blob_id != 0U) {
            (void)drmModeDestroyPropertyBlob(display->fd, display->mode_blob_id);
        }
        for (buffer_index = 0U; buffer_index < SCANOUT_BUFFER_COUNT; buffer_index++) {
            destroy_scanout_buffer(display->fd, &display->scanout[buffer_index]);
        }
        destroy_scanout_buffer(display->fd, &display->bootstrap);
        (void)drmDropMaster(display->fd);
        close(display->fd);
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
}

static int monotonic_time_ns(uint64_t *value) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0) {
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000ULL * 1000ULL * 1000ULL + (uint64_t)now.tv_nsec;
    return 0;
}

static int pageflip_wait_timeout_ms(const struct kms_display *display, int *timeout_ms) {
    uint64_t now;
    uint64_t elapsed_ns;
    uint64_t remaining_ms;
    if (display == NULL || timeout_ms == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (!display->pending.active) {
        *timeout_ms = -1;
        return 0;
    }
    if (display->pending.submitted_ns == 0U || monotonic_time_ns(&now) != 0 ||
        now < display->pending.submitted_ns) {
        errno = EPROTO;
        return -1;
    }
    elapsed_ns = now - display->pending.submitted_ns;
    if (elapsed_ns >= DISPLAY_PAGEFLIP_TIMEOUT_NS) {
        errno = ETIMEDOUT;
        return -1;
    }
    remaining_ms =
        (DISPLAY_PAGEFLIP_TIMEOUT_NS - elapsed_ns + 1000ULL * 1000ULL - 1U) /
        (1000ULL * 1000ULL);
    if (remaining_ms == 0U || remaining_ms > (uint64_t)INT_MAX) {
        errno = EOVERFLOW;
        return -1;
    }
    *timeout_ms = (int)remaining_ms;
    return 0;
}

static int publish_display_ready_lock(void) {
    static const char state[] =
        "DISPLAY_RELAY_SCHEMA=1\nSTATE=ready\nMODE=dmabuf-direct-scanout\n";
    int fd = open(RUSTOS_DVM_DISPLAY_READY_LOCK,
                  O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0 || flock(fd, LOCK_EX | LOCK_NB) != 0 || ftruncate(fd, 0) != 0 ||
        write(fd, state, sizeof(state) - 1U) != (ssize_t)(sizeof(state) - 1U) || fsync(fd) != 0) {
        int saved = errno;
        if (fd >= 0)
            close(fd);
        errno = saved;
        return -1;
    }
    return fd;
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

static int publish_display_evidence(const struct kms_display *display, uint64_t sample_sequence,
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
        "DISPLAY_EVIDENCE_SCHEMA=1\nSAMPLE_SEQUENCE=%llu\nSAMPLE_MONOTONIC_NS=%llu\n"
        "WINDOW_NS=%llu\nPAGEFLIP_COMPLETIONS=%llu\nFRAME_HZ_MILLI=%llu\n"
        "CPU_COPY_US_AVG=0\nPAGEFLIP_LATENCY_US_AVG=%llu\n"
        "PAGEFLIP_LATENCY_US_MAX=%llu\nATOMIC_COMMIT_US_AVG=%llu\n"
        "CONNECTOR_ID=%u\nMODE_WIDTH=%u\nMODE_HEIGHT=%u\nDIRECT_SCANOUT=yes\n",
        (unsigned long long)sample_sequence, (unsigned long long)sample_monotonic_ns,
        (unsigned long long)window_ns, (unsigned long long)pageflip_completions,
        (unsigned long long)frame_hz_milli, (unsigned long long)pageflip_latency_us_avg,
        (unsigned long long)pageflip_latency_us_max,
        (unsigned long long)atomic_commit_us_avg, display->connector_id,
        display->mode.hdisplay, display->mode.vdisplay);
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

static int report_relay_stats(struct kms_display *display, uint64_t pageflip_completions,
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
    if (publish_display_evidence(display, *sample_sequence + 1U, now, elapsed_ns,
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

static int select_next_or_newest_present(
    const struct shared_display *shared,
    const uint64_t released_generation[GUI_POOL_SLOT_COUNT], uint64_t displayed_generation,
    uint32_t *selected_slot, uint64_t *selected_generation,
    struct display_damage *selected_damage) {
    uint32_t slot;
    int found = 0;
    for (slot = 0U; slot < GUI_POOL_SLOT_COUNT; slot++) {
        struct display_damage damage;
        uint64_t generation;
        int present = read_present_record(shared, slot, &damage, &generation);
        if (present < 0) {
            return -1;
        }
        if (present == 0 || generation <= released_generation[slot] ||
            (displayed_generation != 0U && generation <= displayed_generation)) {
            continue;
        }
        /*
         * Preserve an immediately preceding snapshot when it is available.
         * Selecting an older unconsumed record after a newer scanout was the
         * previous relay bug: it regressed the displayed generation, defeated
         * bounded damage, and forced a full 1600x900 copy on the next flip.
         * The oldest stale record is released by select_stale_present instead.
         */
        if (displayed_generation != 0U && generation - displayed_generation == 2U) {
            *selected_slot = slot;
            *selected_generation = generation;
            *selected_damage = damage;
            return 1;
        }
        if (!found || generation > *selected_generation) {
            *selected_slot = slot;
            *selected_generation = generation;
            *selected_damage = damage;
            found = 1;
        }
    }
    return found;
}

/*
 * A three-slot producer may publish several frames before the DVM consumes
 * the first one. After the newest frame reaches scanout, older READY records
 * are stale capacity, not future work. Release one stale record at a time
 * through the same module-mediated control path so those slots cannot become
 * a permanent two-slot leak.
 */
static int select_stale_present(const struct shared_display *shared,
                                const uint64_t released_generation[GUI_POOL_SLOT_COUNT],
                                uint64_t displayed_generation, uint32_t protected_slot,
                                uint32_t *selected_slot,
                                uint64_t *selected_generation) {
    uint32_t slot;
    uint64_t oldest_generation = 0U;
    int found = 0;
    if (displayed_generation == 0U) {
        return 0;
    }
    for (slot = 0U; slot < GUI_POOL_SLOT_COUNT; slot++) {
        struct display_damage damage;
        uint64_t generation;
        int present = read_present_record(shared, slot, &damage, &generation);
        if (present < 0) {
            return -1;
        }
        if (slot == protected_slot || present == 0 || generation <= released_generation[slot] ||
            generation > displayed_generation) {
            continue;
        }
        if (!found || generation < oldest_generation) {
            found = 1;
            oldest_generation = generation;
            *selected_slot = slot;
            *selected_generation = generation;
        }
    }
    return found;
}

static int has_incremental_damage(const struct shared_display *shared,
                                  const struct kms_display *display,
                                  struct display_damage damage, uint64_t generation,
                                  uint64_t base_generation) {
    return display->mode.hdisplay == shared->header.width &&
           display->mode.vdisplay == shared->header.height && damage.flags == 0U &&
           base_generation != 0U && generation > base_generation &&
           generation - base_generation == 2U;
}

static void page_flip_completed(int fd, unsigned int sequence, unsigned int seconds,
                                unsigned int microseconds, void *user_data) {
    struct kms_display *display = user_data;
    uint64_t completed_ns;
    uint64_t latency_ns;
    (void)fd;
    (void)sequence;
    (void)seconds;
    (void)microseconds;
    if (display != NULL && display->pending.active && !display->pending.page_flip_complete &&
        display->pending.submitted_ns != 0U && monotonic_time_ns(&completed_ns) == 0 &&
        completed_ns > display->pending.submitted_ns) {
        latency_ns = completed_ns - display->pending.submitted_ns;
        if (UINT64_MAX - display->pageflip_latency_time_ns < latency_ns)
            display->pageflip_latency_time_ns = UINT64_MAX;
        else
            display->pageflip_latency_time_ns += latency_ns;
        if (latency_ns > display->pageflip_latency_max_ns)
            display->pageflip_latency_max_ns = latency_ns;
        display->pending.page_flip_complete = 1;
    }
}

static int drain_page_flip_event(struct kms_display *display) {
    drmEventContext events = {
        .version = DRM_EVENT_CONTEXT_VERSION,
        .page_flip_handler = page_flip_completed,
    };
    return drmHandleEvent(display->fd, &events);
}

static int create_damage_blob(const struct shared_display *shared,
                              const struct kms_display *display,
                              struct display_damage damage, uint64_t generation,
                              uint64_t base_generation, uint32_t *damage_blob_id) {
    struct drm_mode_rect clip = {
        .x1 = 0,
        .y1 = 0,
        .x2 = (int32_t)display->source_width,
        .y2 = (int32_t)display->source_height,
    };
    if (damage_blob_id == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (has_incremental_damage(shared, display, damage, generation, base_generation)) {
        clip.x1 = (int32_t)damage.x;
        clip.y1 = (int32_t)damage.y;
        clip.x2 = (int32_t)(damage.x + damage.width);
        clip.y2 = (int32_t)(damage.y + damage.height);
    }
    if (drmModeCreatePropertyBlob(display->fd, &clip, sizeof(clip), damage_blob_id) != 0) {
        return -1;
    }
    return 0;
}

static int submit_atomic_page_flip(struct kms_display *display, uint32_t buffer_index,
                                   uint32_t slot, uint64_t generation,
                                   struct display_damage damage, uint64_t base_generation,
                                   const struct shared_display *shared,
                                   uint64_t *atomic_commit_time_ns,
                                   uint64_t *atomic_commit_measurements) {
    drmModeAtomicReq *request;
    uint32_t damage_blob_id = 0U;
    uint64_t commit_started_ns = 0U;
    uint64_t completed_ns = 0U;
    int result;
    if (atomic_commit_time_ns == NULL || atomic_commit_measurements == NULL ||
        display->pending.active ||
        buffer_index >= SCANOUT_BUFFER_COUNT ||
        buffer_index == display->front_buffer) {
        errno = atomic_commit_time_ns == NULL || atomic_commit_measurements == NULL ? EINVAL
                                                                                   : EBUSY;
        return -1;
    }
    if (create_damage_blob(shared, display, damage, generation, base_generation,
                           &damage_blob_id) != 0) {
        return -1;
    }
    request = drmModeAtomicAlloc();
    if (request == NULL) {
        (void)drmModeDestroyPropertyBlob(display->fd, damage_blob_id);
        return -1;
    }
    result = add_plane_properties(display, request, display->scanout[buffer_index].framebuffer_id,
                                  damage_blob_id);
    if (result == 0 && monotonic_time_ns(&commit_started_ns) != 0)
        result = -1;
    if (result == 0) {
        result = drmModeAtomicCommit(display->fd, request,
                                     DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
                                     display);
    }
    drmModeAtomicFree(request);
    (void)drmModeDestroyPropertyBlob(display->fd, damage_blob_id);
    if (result != 0) {
        return -1;
    }
    if (commit_started_ns != 0U && monotonic_time_ns(&completed_ns) == 0 &&
        completed_ns > commit_started_ns) {
        uint64_t elapsed_ns = completed_ns - commit_started_ns;
        if (UINT64_MAX - *atomic_commit_time_ns < elapsed_ns)
            *atomic_commit_time_ns = UINT64_MAX;
        else
            *atomic_commit_time_ns += elapsed_ns;
        if (*atomic_commit_measurements != UINT64_MAX)
            (*atomic_commit_measurements)++;
    }
    display->pending.active = 1;
    display->pending.page_flip_complete = 0;
    display->pending.switch_complete = 0;
    display->pending.slot = slot;
    display->pending.target_buffer = buffer_index;
    display->pending.release_slot = display->front_buffer;
    display->pending.generation = generation;
    display->pending.release_generation =
        display->front_buffer == NO_SCANOUT_BUFFER
            ? 0U
            : display->buffer_generation[display->front_buffer];
    display->pending.submitted_ns = commit_started_ns;
    display->pending.damage = damage;
    return 0;
}

static int start_scanout(const struct shared_display *shared, struct kms_display *display,
                         uint32_t slot, uint64_t generation, struct display_damage damage,
                         uint64_t displayed_generation, uint64_t *atomic_commit_time_ns,
                         uint64_t *atomic_commit_measurements) {
    uint32_t buffer_index = slot;
    if (slot >= SCANOUT_BUFFER_COUNT || slot == display->front_buffer) {
        errno = EBUSY;
        return -1;
    }
    if (submit_atomic_page_flip(display, buffer_index, slot, generation, damage,
                                displayed_generation, shared, atomic_commit_time_ns,
                                atomic_commit_measurements) != 0) {
        return -1;
    }
    display->buffer_generation[buffer_index] = generation;
    return 0;
}

static int complete_scanout(const struct shared_display *shared, struct kms_display *display,
                            uint64_t released_generation[GUI_POOL_SLOT_COUNT],
                            uint64_t *displayed_generation) {
    int release_result;
    if (!display->pending.active || !display->pending.page_flip_complete) {
        return 0;
    }
    if (!display->pending.switch_complete) {
        display->front_buffer = display->pending.target_buffer;
        *displayed_generation = display->pending.generation;
        display->pending.switch_complete = 1;
    }
    if (display->pending.release_slot != NO_SCANOUT_BUFFER) {
        release_result = release_surface(shared, display->pending.release_slot,
                                         display->pending.release_generation);
        if (release_result != 0) {
            if (errno == EAGAIN) {
                return 0;
            }
            return -1;
        }
        released_generation[display->pending.release_slot] =
            display->pending.release_generation;
    }
    display->pending.active = 0;
    display->pending.page_flip_complete = 0;
    display->pending.switch_complete = 0;
    display->pending.target_buffer = NO_SCANOUT_BUFFER;
    display->pending.release_slot = NO_SCANOUT_BUFFER;
    display->pending.release_generation = 0U;
    display->pending.submitted_ns = 0U;
    return 1;
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
    struct pollfd pollfds[2];
    uint64_t completed_ns;
    uint64_t commit_started_ns = 0U;
    uint64_t commit_completed_ns = 0U;
    int out_fence_fd = -1;
    int result;
    int out_complete = 0;
    if (display == NULL || runtime == NULL || frame == NULL || presented_ns == NULL ||
        render_time_ns == NULL || atomic_commit_time_ns == NULL ||
        atomic_commit_measurements == NULL || frame->output_index == display->front_buffer) {
        errno = EINVAL;
        return -1;
    }
    display->setup_stage = "gpu-frame-render-fence";
    if (wait_sync_file(frame->in_fence_fd, frame->budget_us) != 0) {
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        return -1;
    }
    display->setup_stage = "gpu-frame-render-budget";
    if (monotonic_time_ns(&completed_ns) != 0 || completed_ns <= frame->render_started_ns ||
        completed_ns - frame->render_started_ns > (uint64_t)frame->budget_us * 1000ULL) {
        close(frame->in_fence_fd);
        frame->in_fence_fd = -1;
        errno = ETIMEDOUT;
        return -1;
    }
    *render_time_ns = completed_ns - frame->render_started_ns;
    display->setup_stage = "gpu-frame-atomic-request";
    request = drmModeAtomicAlloc();
    if (request == NULL)
        return -1;
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
    close(frame->in_fence_fd);
    frame->in_fence_fd = -1;
    if (result != 0 || out_fence_fd < 0) {
        if (out_fence_fd >= 0)
            close(out_fence_fd);
        return -1;
    }
    display->setup_stage = "gpu-frame-present-fences";
    while (!wait.page_flip_complete || !out_complete) {
        pollfds[0].fd = display->fd;
        pollfds[0].events = wait.page_flip_complete ? 0 : POLLIN;
        pollfds[0].revents = 0;
        pollfds[1].fd = out_fence_fd;
        pollfds[1].events = out_complete ? 0 : POLLIN;
        pollfds[1].revents = 0;
        do {
            result = poll(pollfds, 2U, DISPLAY_PAGEFLIP_TIMEOUT_MS);
        } while (result < 0 && errno == EINTR);
        if (result <= 0 ||
            (pollfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
            (pollfds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
            close(out_fence_fd);
            errno = result == 0 ? ETIMEDOUT : EIO;
            return -1;
        }
        if ((pollfds[0].revents & POLLIN) != 0 && drmHandleEvent(display->fd, &event) != 0) {
            close(out_fence_fd);
            return -1;
        }
        if ((pollfds[1].revents & POLLIN) != 0)
            out_complete = 1;
    }
    close(out_fence_fd);
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
}

static int open_gpu_kms_display(const struct shared_display *shared,
                                struct kms_display *display,
                                struct rustos_gpu_runtime **runtime_out,
                                uint64_t *prime_duration_ns,
                                int *prime_present_fence) {
    struct rustos_gpu_frame black;
    uint64_t prime_started_ns;
    uint64_t prime_completed_ns;
    int saved;
    if (runtime_out == NULL || prime_duration_ns == NULL || prime_present_fence == NULL) {
        errno = EINVAL;
        return -1;
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
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
    display->setup_stage = "gpu-initial-black";
    if (rustos_gpu_runtime_render_prime(*runtime_out, &black) != 0) {
        saved = errno == 0 ? EIO : errno;
        display->setup_stage = rustos_gpu_runtime_stage(*runtime_out);
        rustos_gpu_runtime_close(*runtime_out);
        *runtime_out = NULL;
        errno = saved;
        goto fail;
    }
    if (atomic_gpu_initial_modeset(display, *runtime_out, &black,
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
fail:
    saved = errno == 0 ? EIO : errno;
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
    if (display->fd >= 0) {
        if (display->mode_blob_id != 0U)
            (void)drmModeDestroyPropertyBlob(display->fd, display->mode_blob_id);
        (void)drmDropMaster(display->fd);
        close(display->fd);
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
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
                                 struct gpu_submission *submission) {
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
        read_le32(record + 40U) == 0U || read_le32(record + 44U) != 1U ||
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
                             uint64_t prime_duration_ns) {
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
    write_le32(completion + 8U, GPU_RENDER_VERSION);
    write_le32(completion + 12U, GPU_PRIME_COMPLETION_BYTES);
    write_le32(completion + 16U, context_id);
    write_le32(completion + 20U, context_epoch);
    write_le32(completion + 24U, 1U);
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
                                            uint64_t prime_duration_ns) {
    if (publish_gpu_prime(shared, prime_duration_ns) != 0)
        return -1;
    return acknowledge_host_invitation(shared);
}

static int serve_gpu_display(struct shared_display *shared) {
    struct kms_display display;
    struct display_scheduler_guard scheduler = {0};
    struct rustos_gpu_runtime *runtime = NULL;
    uint64_t legacy_front_generation = 0U;
    uint32_t legacy_front_slot = UINT32_MAX;
    uint64_t released_generation[GUI_POOL_SLOT_COUNT] = {0U};
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
    shared->uio_fd = open_uio_interrupt(shared);
    if (shared->uio_fd < 0 || map_gpu_pixel_pool(shared) != 0) {
        relay_log("rustos-dvm-display: GPU read-only pixel mapping unavailable errno=%d\n",
                  errno);
        close_gpu_kms_display(&display, runtime);
        return -1;
    }
    relay_log("rustos-dvm-display: gpu-compositor primed contract=3 driver=%s renderer=%s "
              "source-path=staged-copy zero-copy=0 explicit-fence=1 public-abi=0 prime_us=%llu prime-present=%s\n",
              rustos_gpu_runtime_driver(runtime), rustos_gpu_runtime_renderer(runtime),
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
                if (acknowledge_gpu_host_invitation(shared, prime_duration_ns) != 0)
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
            if (acknowledge_gpu_host_invitation(shared, prime_duration_ns) != 0)
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
                "rustos-dvm-display: scheduler admitted policy=rr priority=%u rttime_soft_us=%u rttime_hard_us=%u\n",
                DISPLAY_RELAY_RR_PRIORITY, DISPLAY_RELAY_RTTIME_SOFT_US,
                DISPLAY_RELAY_RTTIME_HARD_US);
        }
        for (;;) {
            struct gpu_submission submission;
            struct rustos_gpu_frame frame;
            uint64_t presented_ns;
            uint64_t render_time_ns;
            int selected = select_gpu_submission(shared, &submission);
            if (selected < 0)
                goto fail;
            if (selected == 0)
                break;
            if (rustos_gpu_runtime_render_batch(runtime, submission.atlas_pixels,
                    (size_t)shared->atlas.atlas_slot_bytes, submission.damage,
                    submission.damage_count, submission.batch,
                    submission.batch_bytes, submission.slot, submission.generation,
                    submission.sequence, &frame) != 0 ||
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
            if (legacy_front_slot != UINT32_MAX) {
                if (release_surface(shared, legacy_front_slot,
                                    legacy_front_generation) != 0)
                    goto fail;
                released_generation[legacy_front_slot] = legacy_front_generation;
                legacy_front_slot = UINT32_MAX;
                legacy_front_generation = 0U;
            }
            if (ready_lock < 0 && peer_ready_confirmed) {
                ready_lock = publish_display_ready_lock();
                if (ready_lock < 0)
                    goto fail;
            }
            if (!active_logged && ready_lock >= 0) {
                relay_log(
                    "rustos-dvm-display: active width=%u height=%u stride=%u format=BGRA8888 event=ivshmem-msix-uio irq_count=%u source-path=staged-copy zero-copy=0 gpu-composition=1 explicit-fence=1 scanout_buffers=%u cpu-final-compose=0 staged-damage-copy=1\n",
                    shared->header.width, shared->header.height,
                    shared->header.stride_bytes, event_count, SCANOUT_BUFFER_COUNT);
                active_logged = 1;
            }
            if (presented_frames == 1U || presented_frames % GPU_FRAME_LOG_INTERVAL == 0U) {
                relay_log("rustos-dvm-display: gpu-frame sequence=%llu submit=%llu "
                          "output=%u render_us=%llu source-path=staged-copy zero-copy=0 "
                          "gpu-fence=1 present-fence=1\n",
                          (unsigned long long)frame.sequence,
                          (unsigned long long)frame.submit_value, frame.output_index,
                          (unsigned long long)((render_time_ns + 999U) / 1000U));
            }
            if (report_relay_stats(&display, presented_frames, atomic_commit_time_ns,
                                   atomic_commit_measurements, &last_reported_ns,
                                   &last_pageflip_completions,
                                   &last_atomic_commit_time_ns,
                                   &last_atomic_commit_measurements,
                                   &last_pageflip_latency_time_ns, &sample_sequence,
                                   &gpu_metrics) != 0)
                goto fail;
        }
        if (front_submit_value == 0U) {
            struct display_damage damage;
            struct rustos_gpu_frame frame;
            uint32_t slot = 0U;
            uint64_t generation = 0U;
            uint64_t presented_ns;
            uint64_t render_time_ns;
            int selected = select_next_or_newest_present(shared, released_generation,
                                                          legacy_front_generation, &slot,
                                                          &generation, &damage);
            if (selected < 0)
                goto fail;
            if (selected > 0) {
                const uint8_t *pixels = gpu_pixel_pointer(shared,
                    GUI_POOL_HEADER_BYTES + (uint64_t)slot * shared->header.slot_bytes,
                    shared->header.slot_bytes);
                if (pixels == NULL ||
                    rustos_gpu_runtime_render_legacy(runtime, pixels, shared->header.width,
                        shared->header.height, shared->header.stride_bytes, &frame) != 0 ||
                    atomic_gpu_page_flip(&display, runtime, &frame, &presented_ns,
                                         &render_time_ns, &atomic_commit_time_ns,
                                         &atomic_commit_measurements) != 0)
                    goto fail;
                if (legacy_front_slot != UINT32_MAX &&
                    release_surface(shared, legacy_front_slot,
                                    legacy_front_generation) != 0)
                    goto fail;
                if (legacy_front_slot != UINT32_MAX)
                    released_generation[legacy_front_slot] = legacy_front_generation;
                legacy_front_slot = slot;
                legacy_front_generation = generation;
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
                    ready_lock = publish_display_ready_lock();
                    if (ready_lock < 0)
                        goto fail;
                }
            }
        }
        if (report_relay_stats(&display, presented_frames, atomic_commit_time_ns,
                               atomic_commit_measurements, &last_reported_ns,
                               &last_pageflip_completions, &last_atomic_commit_time_ns,
                               &last_atomic_commit_measurements,
                               &last_pageflip_latency_time_ns, &sample_sequence,
                               &gpu_metrics) != 0)
            goto fail;
    }
fail:
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
    if (ready_lock >= 0)
        close(ready_lock);
    report_host_offline(shared);
    close_gpu_kms_display(&display, runtime);
    return -1;
}

static int serve_display(void) {
    struct shared_display shared;
    struct kms_display display;
    uint64_t released_generation[GUI_POOL_SLOT_COUNT] = {0U};
    uint64_t displayed_generation = 0U;
    uint64_t last_reported_ns;
    uint64_t pageflip_completions = 0U;
    uint64_t last_pageflip_completions = 0U;
    uint64_t atomic_commit_time_ns = 0U;
    uint64_t last_atomic_commit_time_ns = 0U;
    uint64_t atomic_commit_measurements = 0U;
    uint64_t last_atomic_commit_measurements = 0U;
    uint64_t last_pageflip_latency_time_ns = 0U;
    uint64_t sample_sequence = 0U;
    uint64_t interrupt_count = 0U;
    int active_after_interrupt = 0;
    int peer_ready_sent = 0;
    int peer_ready_confirmed = 0;
    int display_ready_lock = -1;
    (void)unlink(RUSTOS_DVM_DISPLAY_READY_LOCK);
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE);
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
    if (open_shared_display(&shared) != 0) {
        relay_log("rustos-dvm-display: shared aperture unavailable errno=%d\n", errno);
        return -1;
    }
    if (shared.atlas.atlas_slot_bytes != 0U) {
        int gpu_result = serve_gpu_display(&shared);
        close_shared_display(&shared);
        return gpu_result;
    }
    if (open_kms_display(&shared.header, &display) != 0) {
        relay_log("rustos-dvm-display: KMS setup unavailable stage=%s errno=%d\n",
                  display.setup_stage == NULL ? "unknown" : display.setup_stage, errno);
        close_shared_display(&shared);
        return -1;
    }
    shared.uio_fd = open_uio_interrupt(&shared);
    if (shared.uio_fd < 0) {
        relay_log("rustos-dvm-display: ivshmem UIO interrupt unavailable errno=%d\n", errno);
        close_kms_display(&display);
        close_shared_display(&shared);
        return -1;
    }
    if (monotonic_time_ns(&last_reported_ns) != 0) {
        relay_log("rustos-dvm-display: monotonic clock unavailable errno=%d\n", errno);
        close_kms_display(&display);
        close_shared_display(&shared);
        return -1;
    }
    for (;;) {
        int fatal = 0;
        int invitation_pending = 0;
        int uio_ready = 0;
        int drm_ready = 0;
        int pageflip_timeout_ms;
        int completed;
        uint32_t uio_event_count;
        /*
         * A readable aperture is not a presentation event.  In particular,
         * generic UIO may expose one without an IRQ source.  Consume the
         * dedicated MSI-X UIO event before inspecting or submitting any
         * generation so the initial scanout cannot fabricate relay health.
         */
        if (!peer_ready_sent &&
            host_invitation_pending(&shared, &invitation_pending) != 0) {
            errno = EPROTO;
            break;
        }
        if (invitation_pending) {
            if (acknowledge_host_invitation(&shared) != 0) {
                errno = EPROTO;
                break;
            }
            peer_ready_sent = 1;
            relay_log("rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n");
            /*
             * An invitation recorded before /dev/uio was opened has no
             * userspace event count. Wait for a subsequent real doorbell
             * before accepting or reporting a displayed generation.
             */
        }
        if (pageflip_wait_timeout_ms(&display, &pageflip_timeout_ms) != 0 ||
            wait_for_relay_event(shared.uio_fd, display.fd, pageflip_timeout_ms,
                                 &uio_event_count, &uio_ready, &drm_ready) != 0) {
            break;
        }
        if (drm_ready && drain_page_flip_event(&display) != 0) {
            break;
        }
        if (uio_ready) {
            interrupt_count = uio_event_count;
            if (!peer_ready_sent) {
                if (acknowledge_host_invitation(&shared) != 0) {
                    errno = EPROTO;
                    break;
                }
                peer_ready_sent = 1;
                relay_log("rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n");
            }
            if (!peer_ready_confirmed && host_confirmed_peer_ready(&shared)) {
                peer_ready_confirmed = 1;
                relay_log(
                    "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n");
            }
        }
        completed =
            complete_scanout(&shared, &display, released_generation, &displayed_generation);
        if (completed < 0) {
            fatal = 1;
        } else if (completed > 0) {
            pageflip_completions++;
            if (interrupt_count != 0U && peer_ready_confirmed && active_after_interrupt == 0) {
                display_ready_lock = publish_display_ready_lock();
                if (display_ready_lock < 0) {
                    fatal = 1;
                    break;
                }
                relay_log(
                    "rustos-dvm-display: active width=%u height=%u stride=%u format=BGRA8888 event=ivshmem-msix-uio irq_count=%llu dmabuf-direct-scanout=%ux%u atomic-pageflip-fence=1 scanout_buffers=%u cpu-copy=0\n",
                    shared.header.width, shared.header.height, shared.header.stride_bytes,
                    (unsigned long long)interrupt_count, display.mode.hdisplay,
                    display.mode.vdisplay, SCANOUT_BUFFER_COUNT);
                active_after_interrupt = 1;
            }
        }
        if (!fatal && !display.pending.active) {
            struct display_damage damage;
            uint32_t slot = 0U;
            uint64_t generation = 0U;
            int stale;
            int release_result;
            int selected;

            selected = select_next_or_newest_present(&shared, released_generation,
                                                      displayed_generation, &slot, &generation,
                                                      &damage);
            if (selected < 0) {
                fatal = 1;
            } else if (selected == 0) {
                /*
                 * Never spend the only wakeup for a fresh visible frame on
                 * stale-capacity cleanup. The former stale-first ordering
                 * divided steady-state presentation throughput by the three
                 * pool slots. Cleanup is safe only when no generation newer
                 * than the displayed frame is waiting.
                 */
                stale = select_stale_present(&shared, released_generation, displayed_generation,
                                             display.front_buffer, &slot, &generation);
                if (stale < 0) {
                    fatal = 1;
                } else if (stale > 0) {
                    release_result = release_surface(&shared, slot, generation);
                    if (release_result != 0 && errno != EAGAIN) {
                        fatal = 1;
                    }
                    if (release_result == 0) {
                        released_generation[slot] = generation;
                    }
                    if (fatal != 0) {
                        break;
                    }
                    if (report_relay_stats(
                            &display, pageflip_completions, atomic_commit_time_ns,
                            atomic_commit_measurements,
                            &last_reported_ns, &last_pageflip_completions,
                            &last_atomic_commit_time_ns, &last_atomic_commit_measurements,
                            &last_pageflip_latency_time_ns, &sample_sequence, NULL) != 0) {
                        fatal = 1;
                    }
                }
            }
            if (selected > 0 && fatal == 0) {
                if (start_scanout(&shared, &display, slot, generation, damage,
                                  displayed_generation, &atomic_commit_time_ns,
                                  &atomic_commit_measurements) != 0 &&
                    errno != EBUSY) {
                    fatal = 1;
                }
            }
        }
        if (fatal != 0) {
            break;
        }
        if (report_relay_stats(&display, pageflip_completions, atomic_commit_time_ns,
                               atomic_commit_measurements,
                               &last_reported_ns, &last_pageflip_completions,
                               &last_atomic_commit_time_ns, &last_atomic_commit_measurements,
                               &last_pageflip_latency_time_ns, &sample_sequence, NULL) != 0) {
            break;
        }
    }
    relay_log("rustos-dvm-display: relay stopped errno=%d\n", errno);
    /* A failed relay can only revoke host availability. The next successful
     * instance must consume a new host invitation and send a fresh ready. */
    report_host_offline(&shared);
    if (display_ready_lock >= 0)
        close(display_ready_lock);
    (void)unlink(RUSTOS_DVM_DISPLAY_READY_LOCK);
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE);
    (void)unlink(RUSTOS_DVM_DISPLAY_EVIDENCE_TEMP);
    close_kms_display(&display);
    close_shared_display(&shared);
    return -1;
}

int main(int argc, char **argv) {
    if (argc != 2 || strcmp(argv[1], "serve") != 0) {
        fprintf(stderr, "usage: %s serve\n", argv[0]);
        return EXIT_FAILURE;
    }
    for (;;) {
        if (serve_display() != 0) {
            sleep(DISPLAY_RETRY_SECONDS);
        }
    }
}
