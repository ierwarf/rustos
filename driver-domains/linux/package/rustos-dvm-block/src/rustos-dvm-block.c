// SPDX-License-Identifier: MIT
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <inttypes.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <linux/fs.h>

#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "RustOS DVM block ABI v1 requires a little-endian relay"
#endif

#define DVM_BLOCK_MAGIC "RSDVMBL1"
#define DVM_BLOCK_VERSION 1U
#define DVM_BLOCK_HEADER_BYTES 4096U
#define DVM_BLOCK_HEADER_RECORD_BYTES 128U
#define DVM_BLOCK_RECORD_BYTES 64U
#define DVM_BLOCK_QUEUE_DEPTH 64U
#define DVM_BLOCK_DATA_SLOT_BYTES (64U * 1024U)
#define DVM_BLOCK_REQUEST_RING_OFFSET ((uint64_t)DVM_BLOCK_HEADER_BYTES)
#define DVM_BLOCK_COMPLETION_RING_OFFSET \
    (DVM_BLOCK_REQUEST_RING_OFFSET + \
     (uint64_t)DVM_BLOCK_QUEUE_DEPTH * DVM_BLOCK_RECORD_BYTES)
#define DVM_BLOCK_DATA_OFFSET \
    (DVM_BLOCK_COMPLETION_RING_OFFSET + \
     (uint64_t)DVM_BLOCK_QUEUE_DEPTH * DVM_BLOCK_RECORD_BYTES)
#define DVM_BLOCK_USED_BYTES \
    (DVM_BLOCK_DATA_OFFSET + \
     (uint64_t)DVM_BLOCK_QUEUE_DEPTH * DVM_BLOCK_DATA_SLOT_BYTES)
#define DVM_BLOCK_APERTURE_BYTES (8ULL * 1024ULL * 1024ULL)

_Static_assert(DVM_BLOCK_USED_BYTES <= DVM_BLOCK_APERTURE_BYTES,
               "block ring and data slots exceed the fixed PCI BAR");
_Static_assert((DVM_BLOCK_APERTURE_BYTES &
                (DVM_BLOCK_APERTURE_BYTES - 1ULL)) == 0ULL,
               "block aperture must be a power-of-two PCI BAR");

#define DVM_BLOCK_FEATURE_FLUSH (UINT64_C(1) << 0)
#define DVM_BLOCK_FEATURE_DISCARD (UINT64_C(1) << 1)
#define DVM_BLOCK_FEATURE_WRITE_ZEROES (UINT64_C(1) << 2)
#define DVM_BLOCK_FEATURE_FUA (UINT64_C(1) << 3)
#define DVM_BLOCK_FEATURE_WRITEBACK (UINT64_C(1) << 4)
#define DVM_BLOCK_KNOWN_FEATURES UINT64_C(0x1f)

#define DVM_BLOCK_FLAG_RUSTOS_READY (UINT32_C(1) << 0)
#define DVM_BLOCK_FLAG_DVM_READY (UINT32_C(1) << 1)
#define DVM_BLOCK_FLAG_READ_ONLY (UINT32_C(1) << 2)
#define DVM_BLOCK_KNOWN_FLAGS UINT32_C(0x7)

#define DVM_BLOCK_REQUEST_FLAG_FUA (UINT32_C(1) << 0)
#define DVM_BLOCK_REQUEST_FLAG_UNMAP (UINT32_C(1) << 1)
#define DVM_BLOCK_REQUEST_KNOWN_FLAGS UINT32_C(0x3)

#define DVM_BLOCK_OP_READ 0U
#define DVM_BLOCK_OP_WRITE 1U
#define DVM_BLOCK_OP_FLUSH 4U
#define DVM_BLOCK_OP_DISCARD 11U
#define DVM_BLOCK_OP_WRITE_ZEROES 13U

#define DVM_BLOCK_STATUS_SUCCESS 0U
#define DVM_BLOCK_STATUS_IO_ERROR 1U
#define DVM_BLOCK_STATUS_UNSUPPORTED 2U

#define HEADER_FLAGS_OFFSET 64U
#define REQUEST_PRODUCER_OFFSET 72U
#define REQUEST_CONSUMER_OFFSET 80U
#define COMPLETION_PRODUCER_OFFSET 88U
#define COMPLETION_CONSUMER_OFFSET 96U
#define UIO_NAME "rustos-dvm-block"
#define READY_DIR "/run/rustos-dvm"
#define BLOCK_EVIDENCE_FILE "block-evidence-v1.env"
#define BLOCK_EVIDENCE_NEXT ".block-evidence-v1.next"

struct block_header {
    uint64_t region_bytes;
    uint32_t queue_depth;
    uint32_t data_slot_bytes;
    uint64_t features;
    uint64_t generation;
    uint64_t capacity_sectors;
    uint32_t logical_block_size;
    uint32_t physical_block_size;
    uint32_t flags;
    uint64_t request_producer;
    uint64_t request_consumer;
    uint64_t completion_producer;
    uint64_t completion_consumer;
};

struct block_request {
    uint64_t generation;
    uint64_t request_id;
    uint64_t operation_id;
    uint32_t operation;
    uint32_t flags;
    uint32_t data_slot;
    uint64_t sector;
    uint32_t data_len;
};

struct block_completion {
    uint64_t generation;
    uint64_t request_id;
    uint64_t operation_id;
    uint32_t status;
    uint32_t data_slot;
    uint32_t completed_bytes;
    uint64_t durable_through_operation_id;
};

struct block_device {
    int fd;
    bool read_only;
    uint64_t capacity_sectors;
    uint32_t logical_block_size;
    uint32_t physical_block_size;
    char name[NAME_MAX + 1U];
    char controller_driver[16];
    char controller_bdf[16];
    char pci_vendor[5];
    char pci_device[5];
};

struct transport {
    int uio_fd;
    volatile uint8_t *base;
    size_t bytes;
    char uio_name[32];
};

struct relay_state {
    struct transport transport;
    struct block_device device;
    struct block_header header;
    uint64_t last_request_id;
    uint64_t last_operation_id;
    uint64_t durable_through;
    bool ready_published;
};

static volatile sig_atomic_t stop_requested;

static void relay_log(const char *format, ...)
{
    va_list arguments;

    va_start(arguments, format);
    (void)vfprintf(stderr, format, arguments);
    va_end(arguments);
}

static void handle_signal(int signal_number)
{
    (void)signal_number;
    stop_requested = 1;
}

static uint32_t read_le32(const volatile uint8_t *bytes)
{
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static uint64_t read_le64(const volatile uint8_t *bytes)
{
    return (uint64_t)read_le32(bytes) |
           ((uint64_t)read_le32(bytes + 4U) << 32);
}

static void write_le32(uint8_t *bytes, uint32_t value)
{
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
    bytes[2] = (uint8_t)(value >> 16);
    bytes[3] = (uint8_t)(value >> 24);
}

static void write_le64(uint8_t *bytes, uint64_t value)
{
    write_le32(bytes, (uint32_t)value);
    write_le32(bytes + 4U, (uint32_t)(value >> 32));
}

static uint64_t load_cursor(const volatile uint8_t *base, size_t offset,
                            int ordering)
{
    const volatile uint64_t *cursor =
        (const volatile uint64_t *)(const void *)(base + offset);

    return __atomic_load_n(cursor, ordering);
}

static void store_cursor(volatile uint8_t *base, size_t offset, uint64_t value,
                         int ordering)
{
    volatile uint64_t *cursor =
        (volatile uint64_t *)(void *)(base + offset);

    __atomic_store_n(cursor, value, ordering);
}

static uint32_t load_flags(const volatile uint8_t *base)
{
    const volatile uint32_t *flags =
        (const volatile uint32_t *)(const void *)(base + HEADER_FLAGS_OFFSET);

    return __atomic_load_n(flags, __ATOMIC_ACQUIRE);
}

static uint32_t fetch_or_flags(volatile uint8_t *base, uint32_t flags)
{
    volatile uint32_t *field =
        (volatile uint32_t *)(void *)(base + HEADER_FLAGS_OFFSET);

    return __atomic_fetch_or(field, flags, __ATOMIC_ACQ_REL);
}

static uint32_t fetch_and_flags(volatile uint8_t *base, uint32_t flags)
{
    volatile uint32_t *field =
        (volatile uint32_t *)(void *)(base + HEADER_FLAGS_OFFSET);

    return __atomic_fetch_and(field, flags, __ATOMIC_ACQ_REL);
}

static bool bytes_are_zero(const volatile uint8_t *bytes, size_t count)
{
    size_t index;

    for (index = 0U; index < count; index++) {
        if (bytes[index] != 0U)
            return false;
    }
    return true;
}

static bool valid_block_size(uint32_t bytes)
{
    return bytes >= 512U && (bytes & (bytes - 1U)) == 0U &&
           bytes % 512U == 0U;
}

static bool valid_cursor_pair(uint64_t producer, uint64_t consumer)
{
    return producer >= consumer &&
           producer - consumer <= DVM_BLOCK_QUEUE_DEPTH;
}

static int read_header(const volatile uint8_t *base, size_t mapped_bytes,
                       struct block_header *header)
{
    if (base == NULL || header == NULL ||
        mapped_bytes != DVM_BLOCK_APERTURE_BYTES ||
        memcmp((const void *)base, DVM_BLOCK_MAGIC, 8U) != 0 ||
        read_le32(base + 8U) != DVM_BLOCK_VERSION ||
        read_le32(base + 12U) != DVM_BLOCK_HEADER_BYTES ||
        !bytes_are_zero(base + 68U, 4U) ||
        !bytes_are_zero(base + 104U,
                        DVM_BLOCK_HEADER_RECORD_BYTES - 104U)) {
        errno = EPROTO;
        return -1;
    }

    header->region_bytes = read_le64(base + 16U);
    header->queue_depth = read_le32(base + 24U);
    header->data_slot_bytes = read_le32(base + 28U);
    header->features = read_le64(base + 32U);
    header->generation = read_le64(base + 40U);
    header->capacity_sectors = read_le64(base + 48U);
    header->logical_block_size = read_le32(base + 56U);
    header->physical_block_size = read_le32(base + 60U);
    header->flags = load_flags(base);
    header->request_consumer =
        load_cursor(base, REQUEST_CONSUMER_OFFSET, __ATOMIC_ACQUIRE);
    header->request_producer =
        load_cursor(base, REQUEST_PRODUCER_OFFSET, __ATOMIC_ACQUIRE);
    header->completion_consumer =
        load_cursor(base, COMPLETION_CONSUMER_OFFSET, __ATOMIC_ACQUIRE);
    header->completion_producer =
        load_cursor(base, COMPLETION_PRODUCER_OFFSET, __ATOMIC_ACQUIRE);

    if (header->region_bytes != DVM_BLOCK_APERTURE_BYTES ||
        header->queue_depth != DVM_BLOCK_QUEUE_DEPTH ||
        header->data_slot_bytes != DVM_BLOCK_DATA_SLOT_BYTES ||
        (header->features & ~DVM_BLOCK_KNOWN_FEATURES) != 0U ||
        (header->features & DVM_BLOCK_FEATURE_FLUSH) == 0U ||
        header->generation == 0U || header->capacity_sectors == 0U ||
        !valid_block_size(header->logical_block_size) ||
        !valid_block_size(header->physical_block_size) ||
        header->physical_block_size < header->logical_block_size ||
        header->physical_block_size % header->logical_block_size != 0U ||
        (header->flags & ~DVM_BLOCK_KNOWN_FLAGS) != 0U ||
        ((header->flags & DVM_BLOCK_FLAG_DVM_READY) != 0U &&
         (header->flags & DVM_BLOCK_FLAG_RUSTOS_READY) == 0U) ||
        !valid_cursor_pair(header->request_producer,
                           header->request_consumer) ||
        !valid_cursor_pair(header->completion_producer,
                           header->completion_consumer)) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static bool same_immutable_header(const struct block_header *left,
                                  const struct block_header *right)
{
    return left->region_bytes == right->region_bytes &&
           left->queue_depth == right->queue_depth &&
           left->data_slot_bytes == right->data_slot_bytes &&
           left->features == right->features &&
           left->generation == right->generation &&
           left->capacity_sectors == right->capacity_sectors &&
           left->logical_block_size == right->logical_block_size &&
           left->physical_block_size == right->physical_block_size &&
           (left->flags & DVM_BLOCK_FLAG_READ_ONLY) ==
               (right->flags & DVM_BLOCK_FLAG_READ_ONLY);
}

static int read_text_file(const char *path, char *buffer, size_t capacity)
{
    ssize_t count;
    int fd;

    if (capacity < 2U) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return -1;
    count = read(fd, buffer, capacity - 1U);
    close(fd);
    if (count <= 0 || (size_t)count >= capacity) {
        errno = EPROTO;
        return -1;
    }
    while (count > 0 &&
           (buffer[count - 1] == '\n' || buffer[count - 1] == '\r' ||
            buffer[count - 1] == ' ' || buffer[count - 1] == '\t'))
        count--;
    buffer[count] = '\0';
    return count == 0 ? -1 : 0;
}

static int read_u64_file(const char *path, uint64_t *value)
{
    char text[64];
    char *end = NULL;
    unsigned long long parsed;

    if (read_text_file(path, text, sizeof(text)) != 0)
        return -1;
    errno = 0;
    parsed = strtoull(text, &end, 0);
    if (errno != 0 || end == text || *end != '\0') {
        errno = EPROTO;
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int find_uio(char *name, size_t name_capacity, size_t *map_bytes)
{
    glob_t matches;
    char selected[32] = "";
    uint64_t selected_map_bytes = 0U;
    size_t index;
    int result = -1;

    memset(&matches, 0, sizeof(matches));
    if (glob("/sys/class/uio/uio*", 0, NULL, &matches) != 0) {
        errno = ENODEV;
        return -1;
    }
    for (index = 0U; index < matches.gl_pathc; index++) {
        char path[PATH_MAX];
        char value[64];
        const char *candidate = strrchr(matches.gl_pathv[index], '/');

        candidate = candidate == NULL ? matches.gl_pathv[index] : candidate + 1;
        if (snprintf(path, sizeof(path), "%s/name",
                     matches.gl_pathv[index]) >= (int)sizeof(path) ||
            read_text_file(path, value, sizeof(value)) != 0 ||
            strcmp(value, UIO_NAME) != 0)
            continue;
        if (selected[0] != '\0') {
            errno = EEXIST;
            goto out;
        }
        if (snprintf(selected, sizeof(selected), "%s", candidate) >=
            (int)sizeof(selected)) {
            errno = ENAMETOOLONG;
            goto out;
        }
    }
    if (selected[0] == '\0') {
        errno = ENODEV;
        goto out;
    }
    {
        char size_path[PATH_MAX];

        if (snprintf(size_path, sizeof(size_path),
                     "/sys/class/uio/%s/maps/map0/size", selected) >=
                (int)sizeof(size_path) ||
            read_u64_file(size_path, &selected_map_bytes) != 0 ||
            selected_map_bytes != DVM_BLOCK_APERTURE_BYTES ||
            selected_map_bytes > SIZE_MAX) {
            errno = EPROTO;
            goto out;
        }
        *map_bytes = (size_t)selected_map_bytes;
    }
    if (snprintf(name, name_capacity, "%s", selected) >=
        (int)name_capacity) {
        errno = ENAMETOOLONG;
        goto out;
    }
    result = 0;
out:
    globfree(&matches);
    return result;
}

static int open_transport(struct transport *transport)
{
    char device[PATH_MAX];
    size_t map_bytes = 0U;

    memset(transport, 0, sizeof(*transport));
    transport->uio_fd = -1;
    if (find_uio(transport->uio_name, sizeof(transport->uio_name),
                 &map_bytes) != 0 ||
        snprintf(device, sizeof(device), "/dev/%s", transport->uio_name) >=
            (int)sizeof(device))
        return -1;
    transport->uio_fd = open(device, O_RDWR | O_CLOEXEC);
    if (transport->uio_fd < 0)
        return -1;
    transport->base =
        mmap(NULL, map_bytes, PROT_READ | PROT_WRITE, MAP_SHARED,
             transport->uio_fd, 0);
    if (transport->base == MAP_FAILED) {
        transport->base = NULL;
        close(transport->uio_fd);
        transport->uio_fd = -1;
        return -1;
    }
    transport->bytes = map_bytes;
    return 0;
}

static void close_transport(struct transport *transport)
{
    if (transport->base != NULL)
        (void)munmap((void *)transport->base, transport->bytes);
    if (transport->uio_fd >= 0)
        close(transport->uio_fd);
    memset(transport, 0, sizeof(*transport));
    transport->uio_fd = -1;
}

static int notify_host(const struct transport *transport)
{
    int32_t notify = 1;
    ssize_t count;

    do {
        count = write(transport->uio_fd, &notify, sizeof(notify));
    } while (count < 0 && errno == EINTR && !stop_requested);
    if (count != (ssize_t)sizeof(notify)) {
        if (count >= 0)
            errno = EIO;
        return -1;
    }
    return 0;
}

static const char *path_basename(const char *path)
{
    const char *separator = strrchr(path, '/');

    return separator == NULL ? path : separator + 1;
}

static bool path_is_descendant(const char *path, const char *ancestor)
{
    size_t length = strlen(ancestor);

    return strncmp(path, ancestor, length) == 0 &&
           (path[length] == '\0' || path[length] == '/');
}

static int find_storage_controller(char *controller, size_t controller_capacity,
                                   char *driver, size_t driver_capacity)
{
    glob_t matches;
    size_t index;
    unsigned int found = 0U;
    int result = -1;

    memset(&matches, 0, sizeof(matches));
    if (glob("/sys/bus/pci/devices/*", 0, NULL, &matches) != 0) {
        errno = ENODEV;
        return -1;
    }
    for (index = 0U; index < matches.gl_pathc; index++) {
        char class_path[PATH_MAX];
        char driver_path[PATH_MAX];
        char class_name[32];
        char resolved_controller[PATH_MAX];
        char resolved_driver[PATH_MAX];
        const char *driver_name;

        if (snprintf(class_path, sizeof(class_path), "%s/class",
                     matches.gl_pathv[index]) >= (int)sizeof(class_path) ||
            snprintf(driver_path, sizeof(driver_path), "%s/driver",
                     matches.gl_pathv[index]) >= (int)sizeof(driver_path) ||
            read_text_file(class_path, class_name, sizeof(class_name)) != 0)
            continue;
        if (strncmp(class_name, "0x0106", 6U) != 0 &&
            strncmp(class_name, "0x0108", 6U) != 0)
            continue;
        if (realpath(matches.gl_pathv[index], resolved_controller) == NULL ||
            realpath(driver_path, resolved_driver) == NULL)
            continue;
        driver_name = path_basename(resolved_driver);
        if ((strncmp(class_name, "0x0106", 6U) == 0 &&
             strcmp(driver_name, "ahci") != 0) ||
            (strncmp(class_name, "0x0108", 6U) == 0 &&
             strcmp(driver_name, "nvme") != 0))
            continue;
        found++;
        if (found != 1U) {
            errno = EEXIST;
            goto out;
        }
        if (snprintf(controller, controller_capacity, "%s",
                     resolved_controller) >= (int)controller_capacity ||
            snprintf(driver, driver_capacity, "%s", driver_name) >=
                (int)driver_capacity) {
            errno = ENAMETOOLONG;
            goto out;
        }
    }
    if (found != 1U) {
        errno = ENODEV;
        goto out;
    }
    result = 0;
out:
    globfree(&matches);
    return result;
}

static int find_block_name(const char *controller, char *name,
                           size_t name_capacity)
{
    glob_t matches;
    size_t index;
    unsigned int found = 0U;
    int result = -1;

    memset(&matches, 0, sizeof(matches));
    if (glob("/sys/class/block/*", 0, NULL, &matches) != 0) {
        errno = ENODEV;
        return -1;
    }
    for (index = 0U; index < matches.gl_pathc; index++) {
        char device_link[PATH_MAX];
        char partition_path[PATH_MAX];
        char resolved[PATH_MAX];
        char node_path[PATH_MAX];
        struct stat status;
        const char *candidate = path_basename(matches.gl_pathv[index]);

        if (strncmp(candidate, "loop", 4U) == 0 ||
            strncmp(candidate, "ram", 3U) == 0 ||
            strncmp(candidate, "dm-", 3U) == 0)
            continue;
        if (snprintf(partition_path, sizeof(partition_path), "%s/partition",
                     matches.gl_pathv[index]) >=
                (int)sizeof(partition_path) ||
            access(partition_path, F_OK) == 0 ||
            snprintf(device_link, sizeof(device_link), "%s/device",
                     matches.gl_pathv[index]) >= (int)sizeof(device_link) ||
            realpath(device_link, resolved) == NULL ||
            !path_is_descendant(resolved, controller) ||
            snprintf(node_path, sizeof(node_path), "/dev/%s", candidate) >=
                (int)sizeof(node_path) ||
            stat(node_path, &status) != 0 || !S_ISBLK(status.st_mode))
            continue;
        found++;
        if (found != 1U) {
            errno = EEXIST;
            goto out;
        }
        if (snprintf(name, name_capacity, "%s", candidate) >=
            (int)name_capacity) {
            errno = ENAMETOOLONG;
            goto out;
        }
    }
    if (found != 1U) {
        errno = ENODEV;
        goto out;
    }
    result = 0;
out:
    globfree(&matches);
    return result;
}

static int queue_feature_bytes(const char *name, const char *feature,
                               uint64_t *bytes)
{
    char path[PATH_MAX];

    if (snprintf(path, sizeof(path), "/sys/class/block/%s/queue/%s", name,
                 feature) >= (int)sizeof(path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return read_u64_file(path, bytes);
}

static int controller_pci_id(const char *controller, const char *attribute,
                             char value[5])
{
    char path[PATH_MAX];
    char text[16];
    size_t index;

    if (snprintf(path, sizeof(path), "%s/%s", controller, attribute) >=
        (int)sizeof(path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    if (read_text_file(path, text, sizeof(text)) != 0 ||
        strlen(text) != 6U || text[0] != '0' || text[1] != 'x') {
        errno = EPROTO;
        return -1;
    }
    for (index = 0U; index < 4U; index++) {
        char byte = text[index + 2U];
        if (!((byte >= '0' && byte <= '9') ||
              (byte >= 'a' && byte <= 'f'))) {
            errno = EPROTO;
            return -1;
        }
        value[index] = byte;
    }
    value[4] = '\0';
    return 0;
}

static int open_block_device(struct block_device *device,
                             const struct block_header *header)
{
    char controller[PATH_MAX];
    char node[PATH_MAX];
    uint64_t capacity_bytes;
    uint64_t discard_bytes = 0U;
    uint64_t zero_bytes = 0U;
    int logical;
    int physical;
    int read_only;
    bool opened_write = true;

    memset(device, 0, sizeof(*device));
    device->fd = -1;
    if (find_storage_controller(controller, sizeof(controller),
                                device->controller_driver,
                                sizeof(device->controller_driver)) != 0) {
        relay_log("rustos-dvm-block: block-device rejected stage=controller errno=%d\n",
                  errno);
        return -1;
    }
    if (snprintf(device->controller_bdf, sizeof(device->controller_bdf), "%s",
                 path_basename(controller)) >=
        (int)sizeof(device->controller_bdf)) {
        errno = ENAMETOOLONG;
        relay_log("rustos-dvm-block: block-device rejected stage=controller-bdf errno=%d\n",
                  errno);
        return -1;
    }
    if (controller_pci_id(controller, "vendor", device->pci_vendor) != 0 ||
        controller_pci_id(controller, "device", device->pci_device) != 0) {
        relay_log("rustos-dvm-block: block-device rejected stage=controller-id errno=%d\n",
                  errno);
        return -1;
    }
    if (find_block_name(controller, device->name, sizeof(device->name)) != 0) {
        relay_log("rustos-dvm-block: block-device rejected stage=namespace errno=%d\n",
                  errno);
        return -1;
    }
    if (snprintf(node, sizeof(node), "/dev/%s", device->name) >=
        (int)sizeof(node)) {
        errno = ENAMETOOLONG;
        relay_log("rustos-dvm-block: block-device rejected stage=node-path errno=%d\n",
                  errno);
        return -1;
    }

    device->fd = open(node, O_RDWR | O_CLOEXEC);
    if (device->fd < 0) {
        opened_write = false;
        device->fd = open(node, O_RDONLY | O_CLOEXEC);
    }
    if (device->fd < 0 ||
        ioctl(device->fd, BLKGETSIZE64, &capacity_bytes) != 0 ||
        ioctl(device->fd, BLKSSZGET, &logical) != 0 ||
        ioctl(device->fd, BLKPBSZGET, &physical) != 0 ||
        ioctl(device->fd, BLKROGET, &read_only) != 0 ||
        capacity_bytes == 0U || capacity_bytes % 512U != 0U ||
        logical <= 0 || physical <= 0) {
        if (device->fd >= 0)
            close(device->fd);
        device->fd = -1;
        return -1;
    }
    device->read_only = read_only != 0;
    device->capacity_sectors = capacity_bytes / 512U;
    device->logical_block_size = (uint32_t)logical;
    device->physical_block_size = (uint32_t)physical;

    if (device->capacity_sectors != header->capacity_sectors ||
        device->logical_block_size != header->logical_block_size ||
        device->physical_block_size != header->physical_block_size ||
        (!device->read_only && !opened_write) ||
        device->read_only !=
            ((header->flags & DVM_BLOCK_FLAG_READ_ONLY) != 0U) ||
        ((header->features & DVM_BLOCK_FEATURE_DISCARD) != 0U &&
         (queue_feature_bytes(device->name, "discard_max_bytes",
                              &discard_bytes) != 0 ||
          discard_bytes == 0U)) ||
        ((header->features & DVM_BLOCK_FEATURE_WRITE_ZEROES) != 0U &&
         (queue_feature_bytes(device->name, "write_zeroes_max_bytes",
                              &zero_bytes) != 0 ||
          zero_bytes == 0U))) {
        close(device->fd);
        device->fd = -1;
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static void close_block_device(struct block_device *device)
{
    if (device->fd >= 0)
        close(device->fd);
    memset(device, 0, sizeof(*device));
    device->fd = -1;
}

static int decode_request(const volatile uint8_t *record,
                          struct block_request *request)
{
    if (!bytes_are_zero(record + 36U, 4U) ||
        !bytes_are_zero(record + 52U, 12U)) {
        errno = EPROTO;
        return -1;
    }
    request->generation = read_le64(record);
    request->request_id = read_le64(record + 8U);
    request->operation_id = read_le64(record + 16U);
    request->operation = read_le32(record + 24U);
    request->flags = read_le32(record + 28U);
    request->data_slot = read_le32(record + 32U);
    request->sector = read_le64(record + 40U);
    request->data_len = read_le32(record + 48U);
    return 0;
}

static bool valid_data_range(const struct block_header *header,
                             const struct block_request *request)
{
    uint64_t sectors;

    if (request->data_len == 0U ||
        request->data_len > header->data_slot_bytes ||
        request->data_len % header->logical_block_size != 0U ||
        request->data_slot >= header->queue_depth ||
        request->sector % (header->logical_block_size / 512U) != 0U)
        return false;
    sectors = request->data_len / 512U;
    return request->sector <= header->capacity_sectors &&
           sectors <= header->capacity_sectors - request->sector;
}

static bool valid_request(const struct relay_state *state,
                          const struct block_request *request,
                          uint64_t sequence)
{
    bool range_valid = valid_data_range(&state->header, request);

    if (request->generation != state->header.generation ||
        request->request_id == 0U ||
        request->request_id != state->last_request_id + 1U ||
        request->flags & ~DVM_BLOCK_REQUEST_KNOWN_FLAGS ||
        request->data_slot !=
            (uint32_t)(sequence % DVM_BLOCK_QUEUE_DEPTH))
        return false;

    switch (request->operation) {
    case DVM_BLOCK_OP_READ:
        return request->operation_id == 0U && request->flags == 0U &&
               range_valid;
    case DVM_BLOCK_OP_WRITE:
        return !state->device.read_only && request->operation_id != 0U &&
               request->operation_id == state->last_operation_id + 1U &&
               (request->flags & DVM_BLOCK_REQUEST_FLAG_UNMAP) == 0U &&
               ((request->flags & DVM_BLOCK_REQUEST_FLAG_FUA) == 0U ||
                (state->header.features & DVM_BLOCK_FEATURE_FUA) != 0U) &&
               range_valid;
    case DVM_BLOCK_OP_FLUSH:
        return request->operation_id != 0U &&
               request->operation_id == state->last_operation_id + 1U &&
               request->flags == 0U && request->sector == 0U &&
               request->data_len == 0U &&
               (state->header.features & DVM_BLOCK_FEATURE_FLUSH) != 0U;
    case DVM_BLOCK_OP_DISCARD:
        return !state->device.read_only && request->operation_id != 0U &&
               request->operation_id == state->last_operation_id + 1U &&
               request->flags == 0U &&
               (state->header.features & DVM_BLOCK_FEATURE_DISCARD) != 0U &&
               range_valid;
    case DVM_BLOCK_OP_WRITE_ZEROES:
        return !state->device.read_only && request->operation_id != 0U &&
               request->operation_id == state->last_operation_id + 1U &&
               (request->flags & DVM_BLOCK_REQUEST_FLAG_FUA) == 0U &&
               (state->header.features &
                DVM_BLOCK_FEATURE_WRITE_ZEROES) != 0U &&
               range_valid;
    default:
        return false;
    }
}

static int checked_offset(const struct block_request *request, off_t *offset)
{
    uint64_t bytes;

    if (request->sector > UINT64_MAX / 512U) {
        errno = EOVERFLOW;
        return -1;
    }
    bytes = request->sector * 512U;
    if (bytes > (uint64_t)INT64_MAX ||
        request->data_len > (uint64_t)INT64_MAX - bytes) {
        errno = EOVERFLOW;
        return -1;
    }
    *offset = (off_t)bytes;
    return 0;
}

static int pread_full(int fd, volatile uint8_t *buffer, size_t bytes,
                      off_t offset)
{
    size_t done = 0U;

    while (done < bytes) {
        ssize_t count =
            pread(fd, (void *)(buffer + done), bytes - done,
                  offset + (off_t)done);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0) {
            if (count == 0)
                errno = EIO;
            return -1;
        }
        done += (size_t)count;
    }
    return 0;
}

static int pwrite_full(int fd, const volatile uint8_t *buffer, size_t bytes,
                       off_t offset)
{
    size_t done = 0U;

    while (done < bytes) {
        ssize_t count =
            pwrite(fd, (const void *)(buffer + done), bytes - done,
                   offset + (off_t)done);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0) {
            if (count == 0)
                errno = EIO;
            return -1;
        }
        done += (size_t)count;
    }
    return 0;
}

static int execute_request(struct relay_state *state,
                           const struct block_request *request,
                           struct block_completion *completion)
{
    volatile uint8_t *slot =
        state->transport.base + DVM_BLOCK_DATA_OFFSET +
        (uint64_t)request->data_slot * DVM_BLOCK_DATA_SLOT_BYTES;
    uint64_t range[2];
    off_t offset = 0;
    int result = 0;

    memset(completion, 0, sizeof(*completion));
    completion->generation = request->generation;
    completion->request_id = request->request_id;
    completion->operation_id = request->operation_id;
    completion->data_slot = request->data_slot;
    completion->status = DVM_BLOCK_STATUS_SUCCESS;

    if (request->operation != DVM_BLOCK_OP_FLUSH &&
        checked_offset(request, &offset) != 0)
        result = -1;
    if (result == 0) {
        switch (request->operation) {
        case DVM_BLOCK_OP_READ:
            result = pread_full(state->device.fd, slot, request->data_len,
                                offset);
            break;
        case DVM_BLOCK_OP_WRITE:
            result = pwrite_full(state->device.fd, slot, request->data_len,
                                 offset);
            if (result == 0 &&
                ((request->flags & DVM_BLOCK_REQUEST_FLAG_FUA) != 0U ||
                 (state->header.features &
                  DVM_BLOCK_FEATURE_WRITEBACK) == 0U))
                result = fdatasync(state->device.fd);
            break;
        case DVM_BLOCK_OP_FLUSH:
            result = fdatasync(state->device.fd);
            break;
        case DVM_BLOCK_OP_DISCARD:
            range[0] = (uint64_t)offset;
            range[1] = request->data_len;
            result = ioctl(state->device.fd, BLKDISCARD, &range);
            break;
        case DVM_BLOCK_OP_WRITE_ZEROES:
            range[0] = (uint64_t)offset;
            range[1] = request->data_len;
            result = ioctl(state->device.fd, BLKZEROOUT, &range);
            break;
        default:
            errno = EOPNOTSUPP;
            result = -1;
            break;
        }
    }

    if (result != 0) {
        completion->status =
            errno == EOPNOTSUPP || errno == ENOTTY
                ? DVM_BLOCK_STATUS_UNSUPPORTED
                : DVM_BLOCK_STATUS_IO_ERROR;
        completion->completed_bytes = 0U;
        completion->durable_through_operation_id = 0U;
        return 0;
    }

    if (request->operation == DVM_BLOCK_OP_READ ||
        request->operation == DVM_BLOCK_OP_WRITE)
        completion->completed_bytes = request->data_len;
    if (request->operation == DVM_BLOCK_OP_FLUSH ||
        (request->operation == DVM_BLOCK_OP_WRITE &&
         ((request->flags & DVM_BLOCK_REQUEST_FLAG_FUA) != 0U ||
          (state->header.features & DVM_BLOCK_FEATURE_WRITEBACK) == 0U))) {
        state->durable_through = request->operation_id;
        completion->durable_through_operation_id = request->operation_id;
    } else if (request->operation != DVM_BLOCK_OP_READ) {
        completion->durable_through_operation_id = state->durable_through;
    }
    return 0;
}

static void encode_completion(uint8_t *record,
                              const struct block_completion *completion)
{
    memset(record, 0, DVM_BLOCK_RECORD_BYTES);
    write_le64(record, completion->generation);
    write_le64(record + 8U, completion->request_id);
    write_le64(record + 16U, completion->operation_id);
    write_le32(record + 24U, completion->status);
    write_le32(record + 28U, completion->data_slot);
    write_le32(record + 32U, completion->completed_bytes);
    write_le64(record + 40U,
               completion->durable_through_operation_id);
}

static void write_record(volatile uint8_t *destination,
                         const uint8_t *source)
{
    size_t index;

    for (index = 0U; index < DVM_BLOCK_RECORD_BYTES; index++)
        destination[index] = source[index];
}

static int verify_live_header(struct relay_state *state)
{
    struct block_header current;

    if (read_header(state->transport.base, state->transport.bytes,
                    &current) != 0 ||
        !same_immutable_header(&state->header, &current) ||
        (current.flags &
         (DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY)) !=
            (DVM_BLOCK_FLAG_RUSTOS_READY |
             DVM_BLOCK_FLAG_DVM_READY)) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int drain_requests(struct relay_state *state)
{
    unsigned int completed = 0U;

    if (verify_live_header(state) != 0)
        return -1;
    for (;;) {
        uint64_t request_consumer =
            load_cursor(state->transport.base, REQUEST_CONSUMER_OFFSET,
                        __ATOMIC_ACQUIRE);
        uint64_t request_producer =
            load_cursor(state->transport.base, REQUEST_PRODUCER_OFFSET,
                        __ATOMIC_ACQUIRE);

        if (!valid_cursor_pair(request_producer, request_consumer)) {
            errno = EPROTO;
            return -1;
        }
        if (request_consumer == request_producer)
            break;
        while (request_consumer < request_producer) {
            uint64_t completion_consumer =
                load_cursor(state->transport.base,
                            COMPLETION_CONSUMER_OFFSET,
                            __ATOMIC_ACQUIRE);
            uint64_t completion_producer =
                load_cursor(state->transport.base,
                            COMPLETION_PRODUCER_OFFSET,
                            __ATOMIC_ACQUIRE);
            volatile uint8_t *request_record;
            volatile uint8_t *completion_record;
            struct block_request request;
            struct block_completion completion;
            uint8_t encoded[DVM_BLOCK_RECORD_BYTES];

            if (!valid_cursor_pair(completion_producer,
                                   completion_consumer) ||
                completion_producer - completion_consumer >=
                    DVM_BLOCK_QUEUE_DEPTH) {
                errno = EPROTO;
                return -1;
            }
            request_record =
                state->transport.base + DVM_BLOCK_REQUEST_RING_OFFSET +
                (request_consumer % DVM_BLOCK_QUEUE_DEPTH) *
                    DVM_BLOCK_RECORD_BYTES;
            if (decode_request(request_record, &request) != 0 ||
                !valid_request(state, &request, request_consumer)) {
                errno = EPROTO;
                return -1;
            }
            state->last_request_id = request.request_id;
            if (request.operation != DVM_BLOCK_OP_READ)
                state->last_operation_id = request.operation_id;
            (void)execute_request(state, &request, &completion);
            encode_completion(encoded, &completion);
            completion_record =
                state->transport.base +
                DVM_BLOCK_COMPLETION_RING_OFFSET +
                (completion_producer % DVM_BLOCK_QUEUE_DEPTH) *
                    DVM_BLOCK_RECORD_BYTES;
            write_record(completion_record, encoded);
            store_cursor(state->transport.base, REQUEST_CONSUMER_OFFSET,
                         request_consumer + 1U, __ATOMIC_RELEASE);
            store_cursor(state->transport.base,
                         COMPLETION_PRODUCER_OFFSET,
                         completion_producer + 1U, __ATOMIC_RELEASE);
            request_consumer++;
            completed++;
        }
    }
    if (completed != 0U && notify_host(&state->transport) != 0)
        return -1;
    return 0;
}

static int acquire_relay_lock(void)
{
    int fd;

    if (mkdir("/run/rustos-dvm", 0700) != 0 && errno != EEXIST)
        return -1;
    fd = open("/run/rustos-dvm/block.lock",
              O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (fd < 0)
        return -1;
    if (flock(fd, LOCK_EX | LOCK_NB) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int publish_ready(struct relay_state *state)
{
    uint32_t previous;

    if (state->header.request_producer != 0U ||
        state->header.request_consumer != 0U ||
        state->header.completion_producer != 0U ||
        state->header.completion_consumer != 0U ||
        (state->header.flags & DVM_BLOCK_FLAG_DVM_READY) != 0U) {
        errno = ESTALE;
        return -1;
    }
    previous = fetch_or_flags(state->transport.base,
                              DVM_BLOCK_FLAG_DVM_READY);
    if ((previous & DVM_BLOCK_FLAG_RUSTOS_READY) == 0U ||
        (previous & DVM_BLOCK_FLAG_DVM_READY) != 0U) {
        (void)fetch_and_flags(state->transport.base,
                              ~DVM_BLOCK_FLAG_DVM_READY);
        errno = EPROTO;
        return -1;
    }
    state->ready_published = true;
    state->header.flags |= DVM_BLOCK_FLAG_DVM_READY;
    return notify_host(&state->transport);
}

static int publish_block_evidence(const struct relay_state *state)
{
    int directory_fd = -1;
    int evidence_fd = -1;
    int result = -1;

    if (mkdir(READY_DIR, 0700) != 0 && errno != EEXIST)
        return -1;
    directory_fd = open(READY_DIR, O_RDONLY | O_DIRECTORY | O_CLOEXEC |
                                      O_NOFOLLOW);
    if (directory_fd < 0)
        return -1;
    (void)unlinkat(directory_fd, BLOCK_EVIDENCE_NEXT, 0);
    evidence_fd = openat(directory_fd, BLOCK_EVIDENCE_NEXT,
                         O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
                         0600);
    if (evidence_fd < 0)
        goto out;
    if (dprintf(
            evidence_fd,
            "BLOCK_EVIDENCE_SCHEMA=1\nGENERATION=%" PRIu64
            "\nDRIVER=%s\nPCI_VENDOR=%s\nPCI_DEVICE=%s\nGUEST_PCI_BDF=%s"
            "\nBLOCK_NAME=%s\nCAPACITY_SECTORS=%" PRIu64
            "\nLOGICAL_BLOCK_SIZE=%u\nPHYSICAL_BLOCK_SIZE=%u"
            "\nFEATURES_HEX=%016" PRIx64 "\nREAD_ONLY=%s\n",
            state->header.generation, state->device.controller_driver,
            state->device.pci_vendor, state->device.pci_device,
            state->device.controller_bdf, state->device.name,
            state->device.capacity_sectors, state->device.logical_block_size,
            state->device.physical_block_size, state->header.features,
            state->device.read_only ? "yes" : "no") < 0 ||
        fsync(evidence_fd) != 0 ||
        close(evidence_fd) != 0) {
        evidence_fd = -1;
        goto out;
    }
    evidence_fd = -1;
    if (renameat(directory_fd, BLOCK_EVIDENCE_NEXT, directory_fd,
                 BLOCK_EVIDENCE_FILE) != 0 ||
        fsync(directory_fd) != 0)
        goto out;
    result = 0;
out:
    if (evidence_fd >= 0)
        close(evidence_fd);
    if (result != 0)
        (void)unlinkat(directory_fd, BLOCK_EVIDENCE_NEXT, 0);
    if (directory_fd >= 0)
        close(directory_fd);
    return result;
}

static int wait_for_rustos_ready(struct relay_state *state)
{
    struct pollfd descriptor = {
        .fd = state->transport.uio_fd,
        .events = POLLIN,
    };
    struct timespec started;

    if (clock_gettime(CLOCK_MONOTONIC, &started) != 0)
        return -1;
    for (;;) {
        struct block_header current;
        struct timespec now;
        int64_t elapsed_ms;
        int remaining_ms;
        int result;

        if (read_header(state->transport.base, state->transport.bytes,
                        &current) != 0 ||
            !same_immutable_header(&state->header, &current) ||
            (current.flags & DVM_BLOCK_FLAG_DVM_READY) != 0U) {
            errno = EPROTO;
            return -1;
        }
        if ((current.flags & DVM_BLOCK_FLAG_RUSTOS_READY) != 0U) {
            state->header.flags = current.flags;
            return 0;
        }
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
            return -1;
        elapsed_ms =
            (int64_t)(now.tv_sec - started.tv_sec) * INT64_C(1000) +
            (int64_t)(now.tv_nsec - started.tv_nsec) / INT64_C(1000000);
        if (elapsed_ms >= INT64_C(30000)) {
            errno = ETIMEDOUT;
            return -1;
        }
        remaining_ms = 30000 - (int)elapsed_ms;
        descriptor.revents = 0;
        result = poll(&descriptor, 1U, remaining_ms);
        if (result < 0 && errno == EINTR) {
            if (stop_requested)
                return -1;
            continue;
        }
        if (result < 0)
            return -1;
        if (result == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if ((descriptor.revents & POLLIN) == 0) {
            errno = EIO;
            return -1;
        }
        {
            uint32_t event_count;
            ssize_t count;

            do {
                count = read(state->transport.uio_fd, &event_count,
                             sizeof(event_count));
            } while (count < 0 && errno == EINTR && !stop_requested);
            if (count != (ssize_t)sizeof(event_count) ||
                event_count == 0U) {
                if (count >= 0)
                    errno = EIO;
                return -1;
            }
        }
    }
}

static void withdraw_ready(struct relay_state *state)
{
    int directory_fd;

    directory_fd = open(READY_DIR, O_RDONLY | O_DIRECTORY | O_CLOEXEC |
                                      O_NOFOLLOW);
    if (directory_fd >= 0) {
        (void)unlinkat(directory_fd, BLOCK_EVIDENCE_FILE, 0);
        (void)unlinkat(directory_fd, BLOCK_EVIDENCE_NEXT, 0);
        (void)fsync(directory_fd);
        close(directory_fd);
    }
    if (!state->ready_published || state->transport.base == NULL)
        return;
    (void)fetch_and_flags(state->transport.base,
                          ~DVM_BLOCK_FLAG_DVM_READY);
    (void)notify_host(&state->transport);
    state->ready_published = false;
}

static int serve(void)
{
    struct sigaction action;
    struct relay_state state;
    int lock_fd = -1;
    int result = 1;
    const char *failed_stage = "signal";

    memset(&state, 0, sizeof(state));
    state.transport.uio_fd = -1;
    state.device.fd = -1;
    memset(&action, 0, sizeof(action));
    action.sa_handler = handle_signal;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL) != 0 ||
        sigaction(SIGINT, &action, NULL) != 0)
        goto out;

    failed_stage = "lock";
    lock_fd = acquire_relay_lock();
    if (lock_fd < 0)
        goto out;
    failed_stage = "transport";
    if (open_transport(&state.transport) != 0)
        goto out;
    failed_stage = "header";
    if (read_header(state.transport.base, state.transport.bytes,
                    &state.header) != 0)
        goto out;
    failed_stage = "block-device";
    if (open_block_device(&state.device, &state.header) != 0)
        goto out;
    failed_stage = "rustos-ready";
    if (wait_for_rustos_ready(&state) != 0)
        goto out;
    failed_stage = "publish-ready";
    if (publish_ready(&state) != 0)
        goto out;
    failed_stage = "publish-evidence";
    if (publish_block_evidence(&state) != 0)
        goto out;

    failed_stage = "request-loop";
    relay_log(
        "rustos-dvm-block: ready abi=1 generation=%" PRIu64
        " controller=%s device=%s sectors=%" PRIu64
        " logical=%u physical=%u features=0x%" PRIx64
        " event=ivshmem-msix-uio\n",
        state.header.generation, state.device.controller_driver,
        state.device.name, state.device.capacity_sectors,
        state.device.logical_block_size,
        state.device.physical_block_size, state.header.features);

    for (;;) {
        uint32_t event_count;
        ssize_t count;

        if (stop_requested)
            break;
        if (drain_requests(&state) != 0)
            goto out;
        do {
            count = read(state.transport.uio_fd, &event_count,
                         sizeof(event_count));
        } while (count < 0 && errno == EINTR && !stop_requested);
        if (stop_requested)
            break;
        if (count != (ssize_t)sizeof(event_count) || event_count == 0U) {
            if (count >= 0)
                errno = EIO;
            goto out;
        }
    }
    result = 0;
out:
    if (result != 0)
        relay_log("rustos-dvm-block: revoked stage=%s errno=%d\n",
                  failed_stage, errno);
    withdraw_ready(&state);
    close_block_device(&state.device);
    close_transport(&state.transport);
    if (lock_fd >= 0)
        close(lock_fd);
    return result;
}

static int selftest(void)
{
    struct relay_state state;
    struct block_request write_request;
    struct block_request read_request;
    struct block_request flush_request;
    struct block_completion completion;
    volatile uint8_t *write_slot;
    volatile uint8_t *read_slot;
    uint8_t expected[4096];
    uint8_t actual[4096];
    FILE *backing = NULL;
    size_t index;
    int result = 1;

    memset(&state, 0, sizeof(state));
    state.transport.base = calloc(1U, DVM_BLOCK_APERTURE_BYTES);
    state.transport.bytes = DVM_BLOCK_APERTURE_BYTES;
    state.device.fd = -1;
    if (state.transport.base == NULL)
        return 1;
    backing = tmpfile();
    if (backing == NULL ||
        ftruncate(fileno(backing), 128U * 512U) != 0)
        goto out;
    state.device.fd = fileno(backing);
    state.header.region_bytes = DVM_BLOCK_APERTURE_BYTES;
    state.header.queue_depth = DVM_BLOCK_QUEUE_DEPTH;
    state.header.data_slot_bytes = DVM_BLOCK_DATA_SLOT_BYTES;
    state.header.features =
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA;
    state.header.generation = 7U;
    state.header.capacity_sectors = 128U;
    state.header.logical_block_size = 4096U;
    state.header.physical_block_size = 4096U;
    state.header.flags =
        DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;

    for (index = 0U; index < sizeof(expected); index++)
        expected[index] = (uint8_t)(index ^ 0x5aU);
    write_slot = state.transport.base + DVM_BLOCK_DATA_OFFSET;
    memcpy((void *)write_slot, expected, sizeof(expected));
    write_request = (struct block_request){
        .generation = 7U,
        .request_id = 1U,
        .operation_id = 1U,
        .operation = DVM_BLOCK_OP_WRITE,
        .flags = DVM_BLOCK_REQUEST_FLAG_FUA,
        .data_slot = 0U,
        .sector = 8U,
        .data_len = sizeof(expected),
    };
    if (!valid_request(&state, &write_request, 0U))
        goto out;
    state.last_request_id = write_request.request_id;
    state.last_operation_id = write_request.operation_id;
    if (execute_request(&state, &write_request, &completion) != 0 ||
        completion.status != DVM_BLOCK_STATUS_SUCCESS ||
        completion.completed_bytes != sizeof(expected) ||
        completion.durable_through_operation_id != 1U ||
        pread(state.device.fd, actual, sizeof(actual), 4096) !=
            (ssize_t)sizeof(actual) ||
        memcmp(actual, expected, sizeof(actual)) != 0)
        goto out;

    read_slot =
        state.transport.base + DVM_BLOCK_DATA_OFFSET +
        DVM_BLOCK_DATA_SLOT_BYTES;
    memset((void *)read_slot, 0, sizeof(expected));
    read_request = (struct block_request){
        .generation = 7U,
        .request_id = 2U,
        .operation_id = 0U,
        .operation = DVM_BLOCK_OP_READ,
        .flags = 0U,
        .data_slot = 1U,
        .sector = 8U,
        .data_len = sizeof(expected),
    };
    if (!valid_request(&state, &read_request, 1U))
        goto out;
    state.last_request_id = read_request.request_id;
    if (execute_request(&state, &read_request, &completion) != 0 ||
        completion.status != DVM_BLOCK_STATUS_SUCCESS ||
        completion.durable_through_operation_id != 0U ||
        memcmp((const void *)read_slot, expected, sizeof(expected)) != 0)
        goto out;

    flush_request = (struct block_request){
        .generation = 7U,
        .request_id = 3U,
        .operation_id = 2U,
        .operation = DVM_BLOCK_OP_FLUSH,
        .flags = 0U,
        .data_slot = 2U,
        .sector = 0U,
        .data_len = 0U,
    };
    if (!valid_request(&state, &flush_request, 2U))
        goto out;
    state.last_request_id = flush_request.request_id;
    state.last_operation_id = flush_request.operation_id;
    if (execute_request(&state, &flush_request, &completion) != 0 ||
        completion.status != DVM_BLOCK_STATUS_SUCCESS ||
        completion.completed_bytes != 0U ||
        completion.durable_through_operation_id != 2U)
        goto out;

    result = 0;
out:
    if (backing != NULL)
        fclose(backing);
    free((void *)state.transport.base);
    if (result == 0)
        relay_log("rustos-dvm-block: selftest passed\n");
    return result;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "serve") == 0)
        return serve();
    if (argc == 2 && strcmp(argv[1], "selftest") == 0)
        return selftest();
    {
        fprintf(stderr, "usage: %s {serve|selftest}\n", argv[0]);
        return 2;
    }
}
