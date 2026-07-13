// SPDX-License-Identifier: MIT
// DVM-owned DRM/KMS consumer for the fixed RustOS ivshmem display contract.
//
// This process deliberately has no host-control, input, or device-management
// protocol. It reads only the host-created aperture and displays stable frames
// through Linux's own DRM driver with a private double-buffered scanout.

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define IVSHMEM_VENDOR_ID "0x1af4"
#define IVSHMEM_DEVICE_ID "0x1110"
#define IVSHMEM_RESOURCE_INDEX 2U
#define DISPLAY_MAGIC "RSDVMFB1"
#define DISPLAY_HEADER_BYTES 4096U
#define DISPLAY_RECORD_BYTES 64U
#define DISPLAY_BYTES_PER_PIXEL 4U
#define DISPLAY_PIXEL_FORMAT_BGRA8888 1U
#define DISPLAY_FLAG_READY 1U
#define DISPLAY_GENERATION_OFFSET 56U
#define DISPLAY_MAX_REGION_BYTES (64U * 1024U * 1024U)
#define DISPLAY_FRAME_INTERVAL_NS (40L * 1000L * 1000L)
#define DISPLAY_RETRY_SECONDS 1U

struct display_header {
    uint64_t region_bytes;
    uint64_t frame_bytes;
    uint64_t generation;
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
    uint32_t flags;
};

struct shared_display {
    int fd;
    volatile uint8_t *base;
    size_t bytes;
    struct display_header header;
};

struct scanout_buffer {
    uint32_t handle;
    uint32_t pitch;
    uint64_t bytes;
    uint32_t framebuffer_id;
    uint8_t *map;
};

struct kms_display {
    int fd;
    uint32_t connector_id;
    uint32_t crtc_id;
    drmModeModeInfo mode;
    struct scanout_buffer buffers[2];
    unsigned int front_index;
    unsigned int pending_index;
    int page_flip_pending;
};

static int parse_display_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                struct display_header *header);

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

static int ivshmem_bar_path(char *path, size_t path_size, size_t *bar_bytes) {
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
        struct display_header header;

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
        if (index != IVSHMEM_RESOURCE_INDEX + 1U || end < start || end - start + 1U < DISPLAY_HEADER_BYTES) {
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
        mapping = mmap(NULL, (size_t)(end - start + 1U), PROT_READ, MAP_SHARED, fd, 0);
        close(fd);
        if (mapping == MAP_FAILED) {
            continue;
        }
        if (parse_display_header(mapping, (size_t)(end - start + 1U), &header) != 0) {
            munmap((void *)mapping, (size_t)(end - start + 1U));
            continue;
        }
        munmap((void *)mapping, (size_t)(end - start + 1U));
        *bar_bytes = (size_t)(end - start + 1U);
        closedir(directory);
        return 0;
    }
    closedir(directory);
    errno = ENODEV;
    return -1;
}

static int parse_display_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                struct display_header *header) {
    uint64_t required;
    if (mapped_bytes < DISPLAY_RECORD_BYTES || memcmp((const void *)bytes, DISPLAY_MAGIC, 8U) != 0 ||
        read_le32(bytes + 8U) != 1U || read_le32(bytes + 12U) != DISPLAY_HEADER_BYTES ||
        read_le32(bytes + 36U) != DISPLAY_BYTES_PER_PIXEL ||
        read_le32(bytes + 40U) != DISPLAY_PIXEL_FORMAT_BGRA8888) {
        errno = EPROTO;
        return -1;
    }
    header->region_bytes = read_le64(bytes + 16U);
    header->width = read_le32(bytes + 24U);
    header->height = read_le32(bytes + 28U);
    header->stride_bytes = read_le32(bytes + 32U);
    header->flags = read_le32(bytes + 44U);
    header->frame_bytes = read_le64(bytes + 48U);
    header->generation = read_le64(bytes + DISPLAY_GENERATION_OFFSET);
    if (header->region_bytes > mapped_bytes || header->region_bytes > DISPLAY_MAX_REGION_BYTES ||
        header->width == 0U || header->width > UINT32_MAX / DISPLAY_BYTES_PER_PIXEL ||
        header->height == 0U || header->generation == 0U ||
        header->flags != DISPLAY_FLAG_READY ||
        header->stride_bytes < header->width * DISPLAY_BYTES_PER_PIXEL ||
        header->stride_bytes % DISPLAY_BYTES_PER_PIXEL != 0U ||
        header->frame_bytes != (uint64_t)header->stride_bytes * header->height) {
        errno = EPROTO;
        return -1;
    }
    required = DISPLAY_HEADER_BYTES + header->frame_bytes;
    if (required < header->frame_bytes || required > header->region_bytes) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int open_shared_display(struct shared_display *shared) {
    char path[PATH_MAX];
    size_t bar_bytes;
    memset(shared, 0, sizeof(*shared));
    shared->fd = -1;
    if (ivshmem_bar_path(path, sizeof(path), &bar_bytes) != 0) {
        return -1;
    }
    shared->fd = open(path, O_RDONLY | O_CLOEXEC);
    if (shared->fd < 0) {
        return -1;
    }
    shared->base = mmap(NULL, bar_bytes, PROT_READ, MAP_SHARED, shared->fd, 0);
    if (shared->base == MAP_FAILED) {
        shared->base = NULL;
        close(shared->fd);
        shared->fd = -1;
        return -1;
    }
    shared->bytes = bar_bytes;
    if (parse_display_header(shared->base, shared->bytes, &shared->header) != 0) {
        munmap((void *)shared->base, shared->bytes);
        close(shared->fd);
        shared->base = NULL;
        shared->fd = -1;
        return -1;
    }
    return 0;
}

static void close_shared_display(struct shared_display *shared) {
    if (shared->base != NULL) {
        munmap((void *)shared->base, shared->bytes);
    }
    if (shared->fd >= 0) {
        close(shared->fd);
    }
    memset(shared, 0, sizeof(*shared));
    shared->fd = -1;
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

static int select_kms_target(int fd, const struct display_header *header, uint32_t *connector_id,
                             uint32_t *crtc_id, drmModeModeInfo *mode) {
    drmModeRes *resources = drmModeGetResources(fd);
    int result = -1;
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
        *crtc_id = select_crtc(resources, encoder);
        for (mode_index = 0; mode_index < connector->count_modes; mode_index++) {
            if ((uint32_t)connector->modes[mode_index].hdisplay == header->width &&
                (uint32_t)connector->modes[mode_index].vdisplay == header->height) {
                *connector_id = connector->connector_id;
                *mode = connector->modes[mode_index];
                result = *crtc_id == 0U ? -1 : 0;
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
    if (result != 0) {
        errno = ENODEV;
    }
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
    if (buffer->handle != 0U) {
        (void)drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    }
    memset(buffer, 0, sizeof(*buffer));
}

static int create_scanout_buffer(int fd, const struct display_header *header,
                                 struct scanout_buffer *buffer) {
    struct drm_mode_create_dumb create = {
        .width = header->width,
        .height = header->height,
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
    if (buffer->pitch < header->stride_bytes || create.size < header->frame_bytes) {
        errno = EOVERFLOW;
        destroy_scanout_buffer(fd, buffer);
        return -1;
    }
    handles[0] = buffer->handle;
    pitches[0] = buffer->pitch;
    if (drmModeAddFB2(fd, header->width, header->height, DRM_FORMAT_XRGB8888, handles, pitches,
                      offsets, &buffer->framebuffer_id, 0U) != 0) {
        if (drmModeAddFB(fd, header->width, header->height, 24U, 32U, buffer->pitch, buffer->handle,
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

static void page_flip_handler(int fd, unsigned int frame, unsigned int seconds,
                              unsigned int microseconds, void *user_data) {
    struct kms_display *display = user_data;
    (void)fd;
    (void)frame;
    (void)seconds;
    (void)microseconds;
    display->front_index = display->pending_index;
    display->page_flip_pending = 0;
}

static int open_kms_display(const struct display_header *header, struct kms_display *display) {
    unsigned int index;
    uint32_t connector_list[1];
    memset(display, 0, sizeof(*display));
    display->fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (display->fd < 0) {
        return -1;
    }
    if (drmSetMaster(display->fd) != 0 ||
        select_kms_target(display->fd, header, &display->connector_id, &display->crtc_id,
                          &display->mode) != 0) {
        close(display->fd);
        display->fd = -1;
        return -1;
    }
    for (index = 0; index < 2U; index++) {
        if (create_scanout_buffer(display->fd, header, &display->buffers[index]) != 0) {
            while (index > 0U) {
                index--;
                destroy_scanout_buffer(display->fd, &display->buffers[index]);
            }
            close(display->fd);
            display->fd = -1;
            return -1;
        }
    }
    connector_list[0] = display->connector_id;
    if (drmModeSetCrtc(display->fd, display->crtc_id, display->buffers[0].framebuffer_id, 0U, 0U,
                       connector_list, 1, &display->mode) != 0) {
        destroy_scanout_buffer(display->fd, &display->buffers[1]);
        destroy_scanout_buffer(display->fd, &display->buffers[0]);
        close(display->fd);
        display->fd = -1;
        return -1;
    }
    return 0;
}

static void close_kms_display(struct kms_display *display) {
    if (display->fd >= 0) {
        destroy_scanout_buffer(display->fd, &display->buffers[1]);
        destroy_scanout_buffer(display->fd, &display->buffers[0]);
        (void)drmDropMaster(display->fd);
        close(display->fd);
    }
    memset(display, 0, sizeof(*display));
    display->fd = -1;
}

static int drain_kms_events(struct kms_display *display) {
    struct pollfd pollfd = {.fd = display->fd, .events = POLLIN};
    drmEventContext context = {
        .version = DRM_EVENT_CONTEXT_VERSION,
        .page_flip_handler = page_flip_handler,
    };
    while (poll(&pollfd, 1, 0) > 0) {
        if ((pollfd.revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
            drmHandleEvent(display->fd, &context) != 0) {
            return -1;
        }
        pollfd.revents = 0;
    }
    return 0;
}

static int copy_stable_frame(const struct shared_display *shared, struct kms_display *display,
                             uint64_t *generation) {
    const volatile uint8_t *pixels = shared->base + DISPLAY_HEADER_BYTES;
    struct scanout_buffer *back = &display->buffers[display->front_index ^ 1U];
    uint64_t before = read_le64(shared->base + DISPLAY_GENERATION_OFFSET);
    unsigned int row;
    if (before == 0U || (before & 1U) != 0U) {
        return 0;
    }
    for (row = 0; row < shared->header.height; row++) {
        const volatile uint8_t *source = pixels + (size_t)row * shared->header.stride_bytes;
        memcpy(back->map + (size_t)row * back->pitch, (const void *)source,
               shared->header.stride_bytes);
    }
    __sync_synchronize();
    if (read_le64(shared->base + DISPLAY_GENERATION_OFFSET) != before) {
        return 0;
    }
    if (drmModePageFlip(display->fd, display->crtc_id, back->framebuffer_id,
                        DRM_MODE_PAGE_FLIP_EVENT, display) != 0) {
        return -1;
    }
    display->pending_index = display->front_index ^ 1U;
    display->page_flip_pending = 1;
    *generation = before;
    return 1;
}

static int serve_display(void) {
    struct shared_display shared;
    struct kms_display display;
    struct timespec interval = {.tv_sec = 0, .tv_nsec = DISPLAY_FRAME_INTERVAL_NS};
    uint64_t displayed_generation = 0U;
    unsigned int presented_frames = 0U;
    if (open_shared_display(&shared) != 0) {
        fprintf(stderr, "rustos-dvm-display: shared aperture unavailable errno=%d\n", errno);
        return -1;
    }
    if (open_kms_display(&shared.header, &display) != 0) {
        fprintf(stderr, "rustos-dvm-display: KMS setup unavailable errno=%d\n", errno);
        close_shared_display(&shared);
        return -1;
    }
    for (;;) {
        int copied;
        if (drain_kms_events(&display) != 0) {
            break;
        }
        if (!display.page_flip_pending &&
            read_le64(shared.base + DISPLAY_GENERATION_OFFSET) != displayed_generation) {
            copied = copy_stable_frame(&shared, &display, &displayed_generation);
            if (copied < 0) {
                break;
            }
            if (copied > 0) {
                presented_frames++;
                if (presented_frames == 1U) {
                    fprintf(stderr,
                            "rustos-dvm-display: active width=%u height=%u stride=%u format=BGRA8888 double-buffered\n",
                            shared.header.width, shared.header.height, shared.header.stride_bytes);
                    fflush(stderr);
                }
            }
        }
        (void)nanosleep(&interval, NULL);
    }
    fprintf(stderr, "rustos-dvm-display: relay stopped errno=%d\n", errno);
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
