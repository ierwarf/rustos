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
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/file.h>
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
#define RUSTOS_IVSHMEM_UIO_NAME "rustos-dvm-ivshmem-uio"
#define RUSTOS_DVM_HOST_INVITED_ATTRIBUTE "rustos_dvm_host_invited"
#define RUSTOS_DVM_DISPLAY_READY_ATTRIBUTE "rustos_dvm_display_ready"
#define RUSTOS_DVM_DISPLAY_CONTROL_ATTRIBUTE "rustos_dvm_display_control"
#define RUSTOS_DVM_DISPLAY_OFFLINE_ATTRIBUTE "rustos_dvm_display_offline"
#define RUSTOS_DVM_DMABUF_DEVICE "/dev/rustos-dvm-display-dmabuf"
#define RUSTOS_DVM_DISPLAY_READY_LOCK "/run/rustos-dvm/display-ready.lock"
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
#define GUI_POOL_MAX_REGION_BYTES (64U * 1024U * 1024U)
#define DISPLAY_STATS_INTERVAL_NS (1000ULL * 1000ULL * 1000ULL)
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
    size_t bytes;
    size_t pixel_bytes;
    char pci_bdf[32];
    struct gui_pool_header header;
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
    struct pending_scanout pending;
};

static int parse_gui_pool_header(const volatile uint8_t *bytes, size_t mapped_bytes,
                                 size_t aperture_bytes, struct gui_pool_header *header);

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
    shared->pixel_bytes = bar_bytes - GUI_POOL_HEADER_BYTES;
    if (parse_gui_pool_header(shared->base, shared->bytes, bar_bytes, &shared->header) != 0) {
        munmap((void *)shared->base, shared->bytes);
        close(shared->fd);
        shared->base = NULL;
        shared->fd = -1;
        return -1;
    }
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

static int wait_for_relay_event(int uio_fd, int drm_fd, uint32_t *event_count,
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
        ready = poll(pollfds, 2U, -1);
    } while (ready < 0 && errno == EINTR);
    if (ready <= 0 || (pollfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
        (pollfds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
        if (ready == 0) {
            errno = EIO;
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
    if (properties->connector_crtc_id == 0U || properties->crtc_mode_id == 0U ||
        properties->crtc_active == 0U || properties->plane_fb_id == 0U ||
        properties->plane_crtc_id == 0U || properties->plane_src_x == 0U ||
        properties->plane_src_y == 0U || properties->plane_src_w == 0U ||
        properties->plane_src_h == 0U || properties->plane_crtc_x == 0U ||
        properties->plane_crtc_y == 0U || properties->plane_crtc_w == 0U ||
        properties->plane_crtc_h == 0U || properties->plane_fb_damage_clips == 0U) {
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

static void report_relay_stats(uint64_t pageflip_completions, uint64_t atomic_commit_time_ns,
                               uint64_t *last_reported_ns, uint64_t *last_pageflip_completions,
                               uint64_t *last_atomic_commit_time_ns) {
    uint64_t now;
    uint64_t elapsed_ns;
    uint64_t submitted_frames;
    uint64_t frame_hz_milli;
    uint64_t average_atomic_commit_us;
    if (monotonic_time_ns(&now) != 0 || now <= *last_reported_ns ||
        now - *last_reported_ns < DISPLAY_STATS_INTERVAL_NS) {
        return;
    }
    elapsed_ns = now - *last_reported_ns;
    submitted_frames = pageflip_completions - *last_pageflip_completions;
    if (submitted_frames != 0U) {
        frame_hz_milli = (submitted_frames * 1000ULL * 1000ULL * 1000ULL * 1000ULL) /
                         elapsed_ns;
        average_atomic_commit_us =
            ((atomic_commit_time_ns - *last_atomic_commit_time_ns) / submitted_frames) / 1000ULL;
        relay_log(
            "rustos-dvm-display: stats elapsed_ms=%llu frame_hz_milli=%llu pageflip_completions=%llu relay_cpu_copy_us_avg=0 atomic_commit_us_avg=%llu\n",
            (unsigned long long)(elapsed_ns / (1000ULL * 1000ULL)),
            (unsigned long long)frame_hz_milli, (unsigned long long)submitted_frames,
            (unsigned long long)average_atomic_commit_us);
    }
    *last_reported_ns = now;
    *last_pageflip_completions = pageflip_completions;
    *last_atomic_commit_time_ns = atomic_commit_time_ns;
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
    (void)fd;
    (void)sequence;
    (void)seconds;
    (void)microseconds;
    if (display != NULL && display->pending.active) {
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
                                   uint64_t *atomic_commit_time_ns) {
    drmModeAtomicReq *request;
    uint32_t damage_blob_id = 0U;
    uint64_t commit_started_ns = 0U;
    uint64_t completed_ns = 0U;
    int result;
    if (atomic_commit_time_ns == NULL || display->pending.active ||
        buffer_index >= SCANOUT_BUFFER_COUNT ||
        buffer_index == display->front_buffer) {
        errno = atomic_commit_time_ns == NULL ? EINVAL : EBUSY;
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
    if (result == 0) {
        (void)monotonic_time_ns(&commit_started_ns);
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
        *atomic_commit_time_ns += completed_ns - commit_started_ns;
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
    display->pending.damage = damage;
    return 0;
}

static int start_scanout(const struct shared_display *shared, struct kms_display *display,
                         uint32_t slot, uint64_t generation, struct display_damage damage,
                         uint64_t displayed_generation, uint64_t *atomic_commit_time_ns) {
    uint32_t buffer_index = slot;
    if (slot >= SCANOUT_BUFFER_COUNT || slot == display->front_buffer) {
        errno = EBUSY;
        return -1;
    }
    if (submit_atomic_page_flip(display, buffer_index, slot, generation, damage,
                                displayed_generation, shared, atomic_commit_time_ns) != 0) {
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
    return 1;
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
    uint64_t interrupt_count = 0U;
    int active_after_interrupt = 0;
    int peer_ready_sent = 0;
    int peer_ready_confirmed = 0;
    int display_ready_lock = -1;
    (void)unlink(RUSTOS_DVM_DISPLAY_READY_LOCK);
    if (open_shared_display(&shared) != 0) {
        relay_log("rustos-dvm-display: shared aperture unavailable errno=%d\n", errno);
        return -1;
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
        if (wait_for_relay_event(shared.uio_fd, display.fd, &uio_event_count, &uio_ready,
                                 &drm_ready) != 0) {
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
                    report_relay_stats(pageflip_completions, atomic_commit_time_ns,
                                       &last_reported_ns, &last_pageflip_completions,
                                       &last_atomic_commit_time_ns);
                }
            }
            if (selected > 0 && fatal == 0) {
                if (start_scanout(&shared, &display, slot, generation, damage,
                                  displayed_generation, &atomic_commit_time_ns) != 0 &&
                    errno != EBUSY) {
                    fatal = 1;
                }
            }
        }
        if (fatal != 0) {
            break;
        }
        report_relay_stats(pageflip_completions, atomic_commit_time_ns, &last_reported_ns,
                           &last_pageflip_completions, &last_atomic_commit_time_ns);
    }
    relay_log("rustos-dvm-display: relay stopped errno=%d\n", errno);
    /* A failed relay can only revoke host availability. The next successful
     * instance must consume a new host invitation and send a fresh ready. */
    report_host_offline(&shared);
    if (display_ready_lock >= 0)
        close(display_ready_lock);
    (void)unlink(RUSTOS_DVM_DISPLAY_READY_LOCK);
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
