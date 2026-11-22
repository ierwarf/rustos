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
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <linux/fs.h>

#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "RustOS DVM block ABI v2 requires a little-endian relay"
#endif

#define DVM_BLOCK_MAGIC "RSDVMBL2"
#define DVM_BLOCK_VERSION 2U
#define DVM_BLOCK_HEADER_BYTES 4096U
#define DVM_BLOCK_HEADER_RECORD_BYTES 192U
#define DVM_BLOCK_RECORD_BYTES 64U
#define DVM_BLOCK_QUEUE_DEPTH 64U
#define DVM_BLOCK_DATA_SLOT_BYTES (64U * 1024U)
#define DVM_BLOCK_LOG_BYTES 1024U
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

_Static_assert(ATOMIC_BOOL_LOCK_FREE == 2,
               "process-shared watchdog gate requires lock-free atomics");
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
#ifndef READY_DIR
#define READY_DIR "/run/rustos-dvm"
#endif
#define BLOCK_EVIDENCE_FILE "block-evidence-v1.env"
#define BLOCK_EVIDENCE_NEXT ".block-evidence-v1.next"
#define BLOCK_WATCHDOG_TIMEOUT_MS 10000
/* Kernel readahead window for the whole-disk node. The default (128 KiB) keeps
 * at most two guest I/Os in flight for a sequential stream of 64 KiB slots. A
 * larger window lets the page cache issue several merged requests
 * concurrently. It is advisory: it changes no ordering, durability, or
 * addressing property of any request. It was introduced against the former
 * emulated ATAPI media (one command at a time) where it measured no gain; it
 * has NOT been re-measured on the queued virtio-blk harness disk, so it is
 * kept only as a bounded, advisory hint until a run compares read-stats. */
#define BLOCK_READAHEAD_BYTES (2U * 1024U * 1024U)
/* One bounded diagnostic line per this many completed reads. */
#define BLOCK_READ_STATS_INTERVAL 256U

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
    uint8_t epoch_signature[64];
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

/* Contiguous byte range of queued reads that the device may start early. */
struct prefetch_range {
    uint64_t offset;
    uint64_t bytes;
};

/* Worker-private read accounting; the supervisor never touches it. */
struct read_stats {
    uint64_t reads;
    uint64_t bytes;
    uint64_t pread_ns;
    uint64_t pread_ns_max;
    uint64_t prefetch_calls;
    uint64_t prefetch_bytes;
    uint64_t prefetch_ns;
};

struct relay_state {
    struct transport transport;
    struct block_device device;
    struct block_header header;
    uint64_t last_request_id;
    uint64_t last_operation_id;
    uint64_t durable_through;
    bool ready_published;
    int watchdog_fd;
    _Atomic bool *quarantined;
    /* Request cursor below which queued reads were already offered to the
     * device. Monotonic, never exceeds the request producer. */
    uint64_t prefetched_through;
    bool prefetch_disabled;
    struct read_stats stats;
};

/* Unacknowledged completions between doorbell rings inside one batch. */
#define COMPLETION_NOTIFY_EVERY 8U

struct watchdog_event {
    uint64_t request_id;
    uint32_t operation;
    uint32_t begin;
    uint64_t request_consumer;
    uint64_t completion_producer;
    uint8_t completion[DVM_BLOCK_RECORD_BYTES];
};

static volatile sig_atomic_t stop_requested;

static void relay_log(const char *format, ...)
{
    char message[DVM_BLOCK_LOG_BYTES];
    va_list arguments;
    int length;
    int saved_errno = errno;

    va_start(arguments, format);
    length = vsnprintf(message, sizeof(message), format, arguments);
    va_end(arguments);
    if (length > 0) {
        size_t bytes = (size_t)length;

        if (bytes >= sizeof(message))
            bytes = sizeof(message) - 1U;
        while (write(STDERR_FILENO, message, bytes) < 0 && errno == EINTR)
            ;
        if (strncmp(message, "rustos-dvm-block: ready ", 24U) == 0) {
            int kmsg_fd = open("/dev/kmsg", O_WRONLY | O_CLOEXEC);
            if (kmsg_fd >= 0) {
                (void)write(kmsg_fd, message, bytes);
                close(kmsg_fd);
            }
        }
    }
    errno = saved_errno;
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
        bytes_are_zero(base + 104U, 64U) ||
        !bytes_are_zero(base + 168U,
                        DVM_BLOCK_HEADER_RECORD_BYTES - 168U)) {
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
    for (size_t index = 0U; index < sizeof(header->epoch_signature); index++)
        header->epoch_signature[index] = base[104U + index];

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
           memcmp(left->epoch_signature, right->epoch_signature,
                  sizeof(left->epoch_signature)) == 0 &&
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

/* Storage controller classes the relay admits. The class is decided from the
 * identity the guest kernel itself published (PCI class code, bound driver and,
 * for virtio, the virtio device type), never from the harness that created the
 * device, so physical AHCI/NVMe passthrough and the paravirtual KVM-harness
 * disk go through one admission. Exactly one admitted controller may exist; the
 * block-size, capacity and read-only facts are then read from its namespace. */
#define PCI_CLASS_STORAGE_SCSI 0x0100U
#define PCI_CLASS_STORAGE_SATA 0x0106U
#define PCI_CLASS_STORAGE_NVM 0x0108U
#define PCI_VENDOR_VIRTIO 0x1af4U
#define VIRTIO_ID_BLOCK 2U

struct controller_identity {
    uint32_t pci_class;        /* 24-bit PCI class code */
    uint32_t pci_vendor;
    char pci_driver[32];       /* driver bound to the PCI function */
    unsigned int virtio_children;
    uint32_t virtio_device;    /* virtio device type of the sole child */
    char virtio_driver[32];    /* driver bound to the sole virtio child */
};

/* Returns the admitted controller driver name, or NULL when the PCI function
 * is not an admitted storage controller class. The returned string is static
 * and is the Linux driver that owns the block namespace. */
static const char *classify_storage_controller(
    const struct controller_identity *identity)
{
    uint32_t class_code = identity->pci_class >> 8;

    if (class_code == PCI_CLASS_STORAGE_SATA &&
        strcmp(identity->pci_driver, "ahci") == 0)
        return "ahci";
    if (class_code == PCI_CLASS_STORAGE_NVM &&
        strcmp(identity->pci_driver, "nvme") == 0)
        return "nvme";
    if (class_code == PCI_CLASS_STORAGE_SCSI &&
        identity->pci_vendor == PCI_VENDOR_VIRTIO &&
        strcmp(identity->pci_driver, "virtio-pci") == 0 &&
        identity->virtio_children == 1U &&
        identity->virtio_device == VIRTIO_ID_BLOCK &&
        strcmp(identity->virtio_driver, "virtio_blk") == 0)
        return "virtio_blk";
    return NULL;
}

static int read_bound_driver(const char *device_path, char *driver,
                             size_t capacity)
{
    char link[PATH_MAX];
    char resolved[PATH_MAX];

    if (snprintf(link, sizeof(link), "%s/driver", device_path) >=
        (int)sizeof(link) ||
        realpath(link, resolved) == NULL)
        return -1;
    if (snprintf(driver, capacity, "%s", path_basename(resolved)) >=
        (int)capacity) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

/* Fill the virtio part of the identity from the `virtioN` children of a PCI
 * function. More than one child, or an unreadable one, leaves the identity
 * unclassifiable rather than guessing which child owns the namespace. */
static void read_virtio_children(const char *pci_path,
                                 struct controller_identity *identity)
{
    char pattern[PATH_MAX];
    glob_t matches;
    size_t index;

    identity->virtio_children = 0U;
    if (snprintf(pattern, sizeof(pattern), "%s/virtio*", pci_path) >=
        (int)sizeof(pattern))
        return;
    memset(&matches, 0, sizeof(matches));
    if (glob(pattern, 0, NULL, &matches) != 0)
        return;
    for (index = 0U; index < matches.gl_pathc; index++) {
        char device_path[PATH_MAX];
        uint64_t device_type;

        if (snprintf(device_path, sizeof(device_path), "%s/device",
                     matches.gl_pathv[index]) >= (int)sizeof(device_path) ||
            read_u64_file(device_path, &device_type) != 0 ||
            device_type > UINT32_MAX)
            continue;
        identity->virtio_children++;
        identity->virtio_device = (uint32_t)device_type;
        if (read_bound_driver(matches.gl_pathv[index],
                              identity->virtio_driver,
                              sizeof(identity->virtio_driver)) != 0)
            identity->virtio_driver[0] = '\0';
    }
    globfree(&matches);
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
        char attribute[PATH_MAX];
        char resolved_controller[PATH_MAX];
        struct controller_identity identity;
        uint64_t value;
        const char *admitted;

        memset(&identity, 0, sizeof(identity));
        if (snprintf(attribute, sizeof(attribute), "%s/class",
                     matches.gl_pathv[index]) >= (int)sizeof(attribute) ||
            read_u64_file(attribute, &value) != 0 || value > 0xffffffU)
            continue;
        identity.pci_class = (uint32_t)value;
        if (snprintf(attribute, sizeof(attribute), "%s/vendor",
                     matches.gl_pathv[index]) >= (int)sizeof(attribute) ||
            read_u64_file(attribute, &value) != 0 || value > 0xffffU)
            continue;
        identity.pci_vendor = (uint32_t)value;
        if (read_bound_driver(matches.gl_pathv[index], identity.pci_driver,
                              sizeof(identity.pci_driver)) != 0 ||
            realpath(matches.gl_pathv[index], resolved_controller) == NULL)
            continue;
        if ((identity.pci_class >> 8) == PCI_CLASS_STORAGE_SCSI)
            read_virtio_children(resolved_controller, &identity);
        admitted = classify_storage_controller(&identity);
        if (admitted == NULL)
            continue;
        found++;
        if (found != 1U) {
            errno = EEXIST;
            goto out;
        }
        if (snprintf(controller, controller_capacity, "%s",
                     resolved_controller) >= (int)controller_capacity ||
            snprintf(driver, driver_capacity, "%s", admitted) >=
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
    const bool header_read_only =
        (header->flags & DVM_BLOCK_FLAG_READ_ONLY) != 0U;

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

    /* The signed header decides the access mode before the device is touched:
     * a read-only transport never holds a writable descriptor, so no request
     * handling bug can write to media the L0 declared immutable, and a
     * writable transport requires a writable descriptor. The kernel then has
     * to agree through BLKROGET below (QEMU `readonly=on` surfaces as the
     * virtio-blk/NVMe/SCSI read-only bit, which sets the disk read-only). */
    device->fd = open(node, (header_read_only ? O_RDONLY : O_RDWR) | O_CLOEXEC);
    if (device->fd < 0 ||
        ioctl(device->fd, BLKGETSIZE64, &capacity_bytes) != 0 ||
        ioctl(device->fd, BLKSSZGET, &logical) != 0 ||
        ioctl(device->fd, BLKPBSZGET, &physical) != 0 ||
        ioctl(device->fd, BLKROGET, &read_only) != 0 ||
        capacity_bytes == 0U || capacity_bytes % 512U != 0U ||
        logical <= 0 || physical <= 0) {
        int saved_errno = errno;

        if (device->fd >= 0)
            close(device->fd);
        device->fd = -1;
        relay_log("rustos-dvm-block: block-device rejected stage=namespace-query device=%s errno=%d\n",
                  device->name, saved_errno);
        errno = saved_errno;
        return -1;
    }
    device->read_only = read_only != 0;
    device->capacity_sectors = capacity_bytes / 512U;
    device->logical_block_size = (uint32_t)logical;
    device->physical_block_size = (uint32_t)physical;

    if (device->capacity_sectors != header->capacity_sectors ||
        device->logical_block_size != header->logical_block_size ||
        device->physical_block_size != header->physical_block_size ||
        device->read_only != header_read_only ||
        ((header->features & DVM_BLOCK_FEATURE_DISCARD) != 0U &&
         (queue_feature_bytes(device->name, "discard_max_bytes",
                              &discard_bytes) != 0 ||
          discard_bytes == 0U)) ||
        ((header->features & DVM_BLOCK_FEATURE_WRITE_ZEROES) != 0U &&
         (queue_feature_bytes(device->name, "write_zeroes_max_bytes",
                              &zero_bytes) != 0 ||
          zero_bytes == 0U))) {
        /* The signed L0 header is the contract; the device the guest kernel
         * discovered is the evidence. Name both sides so a harness/QEMU
         * geometry or read-only disagreement is diagnosable from the log. */
        relay_log("rustos-dvm-block: block-device rejected stage=geometry device=%s"
                  " sectors=%" PRIu64 "/%" PRIu64
                  " logical=%u/%u physical=%u/%u read_only=%d/%d"
                  " (device/header)\n",
                  device->name, device->capacity_sectors,
                  header->capacity_sectors, device->logical_block_size,
                  header->logical_block_size, device->physical_block_size,
                  header->physical_block_size, device->read_only ? 1 : 0,
                  header_read_only ? 1 : 0);
        close(device->fd);
        device->fd = -1;
        errno = EPROTO;
        return -1;
    }
    if (ioctl(device->fd, BLKRASET,
              (unsigned long)(BLOCK_READAHEAD_BYTES / 512U)) != 0)
        relay_log("rustos-dvm-block: readahead window unchanged errno=%d\n",
                  errno);
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

static inline void wc_store_fence(void)
{
#if defined(__x86_64__) || defined(__i386__)
    __asm__ __volatile__("sfence" ::: "memory");
#else
    atomic_thread_fence(memory_order_release);
#endif
}

static uint8_t read_bounce[DVM_BLOCK_DATA_SLOT_BYTES]
    __attribute__((aligned(4096)));
static uint8_t write_bounce[DVM_BLOCK_DATA_SLOT_BYTES]
    __attribute__((aligned(4096)));

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
            if (request->data_len > sizeof(read_bounce)) {
                errno = EINVAL;
                result = -1;
            } else {
                result = pread_full(state->device.fd, read_bounce,
                                    request->data_len, offset);
                if (result == 0) {
                    memcpy((void *)slot, read_bounce, request->data_len);
                    wc_store_fence();
                }
            }
            break;
        case DVM_BLOCK_OP_WRITE:
            if (request->data_len > sizeof(write_bounce)) {
                errno = EINVAL;
                result = -1;
            } else {
                memcpy(write_bounce, (const void *)slot, request->data_len);
                result = pwrite_full(state->device.fd, write_bounce,
                                     request->data_len, offset);
            }
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

static int watchdog_event_send(int fd, const struct watchdog_event *event)
{
    ssize_t count;
    do {
        count = send(fd, event, sizeof(*event), MSG_NOSIGNAL);
    } while (count < 0 && errno == EINTR);
    if (count != (ssize_t)sizeof(*event)) {
        if (count >= 0) errno = EIO;
        return -1;
    }
    return 0;
}

/* Only the supervisor publishes cursors.  A stopped worker cannot hold up
 * timeout or publish after the supervisor transitions to quarantine. */
static int publish_watchdog_completion(struct relay_state *state,
                                       const struct watchdog_event *event)
{
    volatile uint8_t *record;
    if (atomic_load_explicit(state->quarantined, memory_order_acquire)) {
        errno = EIO;
        return -1;
    }
    if (load_cursor(state->transport.base, REQUEST_CONSUMER_OFFSET, __ATOMIC_ACQUIRE) != event->request_consumer ||
        load_cursor(state->transport.base, COMPLETION_PRODUCER_OFFSET, __ATOMIC_ACQUIRE) != event->completion_producer) {
        errno = EPROTO;
        return -1;
    }
    record = state->transport.base + DVM_BLOCK_COMPLETION_RING_OFFSET +
             (event->completion_producer % DVM_BLOCK_QUEUE_DEPTH) * DVM_BLOCK_RECORD_BYTES;
    write_record(record, event->completion);
    wc_store_fence();
    store_cursor(state->transport.base, REQUEST_CONSUMER_OFFSET, event->request_consumer + 1U, __ATOMIC_RELEASE);
    store_cursor(state->transport.base, COMPLETION_PRODUCER_OFFSET, event->completion_producer + 1U, __ATOMIC_RELEASE);
    return 0;
}

static uint64_t monotonic_ns(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
        return 0U;
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

/* Collect the contiguous byte ranges of the read requests published in
 * [first, producer). Scanning stops at the first record that is not a
 * well-formed read, so a write, flush or malformed record is never reordered
 * around or interpreted here; request admission still happens, unchanged, when
 * the drain loop reaches that record. Adjacent reads merge into one range.
 * `*scanned_through` is the first cursor that was not examined. */
static size_t plan_prefetch(const struct relay_state *state, uint64_t first,
                            uint64_t producer,
                            struct prefetch_range ranges[DVM_BLOCK_QUEUE_DEPTH],
                            uint64_t *scanned_through)
{
    size_t count = 0U;
    uint64_t cursor = first;

    if (producer < first || producer - first > DVM_BLOCK_QUEUE_DEPTH) {
        *scanned_through = first;
        return 0U;
    }
    for (; cursor < producer; cursor++) {
        const volatile uint8_t *record =
            state->transport.base + DVM_BLOCK_REQUEST_RING_OFFSET +
            (cursor % DVM_BLOCK_QUEUE_DEPTH) * DVM_BLOCK_RECORD_BYTES;
        struct block_request request;
        off_t offset;

        if (decode_request(record, &request) != 0 ||
            request.operation != DVM_BLOCK_OP_READ ||
            request.generation != state->header.generation ||
            !valid_data_range(&state->header, &request) ||
            checked_offset(&request, &offset) != 0)
            break;
        if (count != 0U &&
            ranges[count - 1U].offset + ranges[count - 1U].bytes ==
                (uint64_t)offset) {
            ranges[count - 1U].bytes += request.data_len;
        } else {
            ranges[count].offset = (uint64_t)offset;
            ranges[count].bytes = request.data_len;
            count++;
        }
    }
    *scanned_through = cursor;
    return count;
}

/* Offer every queued read to the kernel before executing the first one.
 *
 * The relay executes requests one at a time, and RustOS keeps up to sixteen
 * sequential 64 KiB windows queued. Executing them with synchronous pread
 * alone lets the page cache see only the request at hand, so the emulated
 * controller pays its per-command latency once per window. readahead(2) only
 * starts I/O into the page cache; the pread that follows still produces the
 * bytes, in order, with the same error semantics. Failure is not an error: the
 * pread path is complete without it. Runs after the watchdog "begin" event, so
 * a stuck device is bounded by the same deadline as the request itself. */
static void prefetch_pending_reads(struct relay_state *state,
                                   uint64_t request_consumer)
{
    struct prefetch_range ranges[DVM_BLOCK_QUEUE_DEPTH];
    uint64_t first = request_consumer;
    uint64_t producer;
    uint64_t scanned_through;
    uint64_t started;
    size_t count;
    size_t index;

    if (state->prefetch_disabled)
        return;
    /* Reload: RustOS keeps appending while earlier windows execute. Records
     * below an Acquire-loaded producer are fully published. */
    producer = load_cursor(state->transport.base, REQUEST_PRODUCER_OFFSET,
                           __ATOMIC_ACQUIRE);
    if (!valid_cursor_pair(producer, request_consumer))
        return;
    if (first < state->prefetched_through)
        first = state->prefetched_through;
    if (first >= producer)
        return;
    count = plan_prefetch(state, first, producer, ranges, &scanned_through);
    if (scanned_through == first)
        return;
    started = monotonic_ns();
    for (index = 0U; index < count; index++) {
        if (readahead(state->device.fd, (off_t)ranges[index].offset,
                      (size_t)ranges[index].bytes) != 0) {
            relay_log("rustos-dvm-block: readahead disabled errno=%d\n", errno);
            state->prefetch_disabled = true;
            break;
        }
        state->stats.prefetch_calls++;
        state->stats.prefetch_bytes += ranges[index].bytes;
    }
    state->stats.prefetch_ns += monotonic_ns() - started;
    state->prefetched_through = scanned_through;
}


static void account_read(struct relay_state *state, uint32_t bytes,
                         uint64_t elapsed_ns)
{
    struct read_stats *stats = &state->stats;

    stats->reads++;
    stats->bytes += bytes;
    stats->pread_ns += elapsed_ns;
    if (elapsed_ns > stats->pread_ns_max)
        stats->pread_ns_max = elapsed_ns;
    if (stats->reads % BLOCK_READ_STATS_INTERVAL != 0U)
        return;
    relay_log("rustos-dvm-block: read-stats reads=%" PRIu64 " bytes=%" PRIu64
              " pread_us=%" PRIu64 " pread_max_us=%" PRIu64
              " readahead_calls=%" PRIu64 " readahead_bytes=%" PRIu64
              " readahead_us=%" PRIu64 "\n",
              stats->reads, stats->bytes, stats->pread_ns / 1000U,
              stats->pread_ns_max / 1000U, stats->prefetch_calls,
              stats->prefetch_bytes, stats->prefetch_ns / 1000U);
    stats->pread_ns_max = 0U;
}

static int drain_requests(struct relay_state *state)
{
    uint64_t completion_producer;

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
        /* Every earlier batch ended with an acknowledged completion, so the
         * published cursor is exact here. Inside the batch the supervisor
         * publishes completions asynchronously, so the worker counts them
         * itself: it is the only writer of events and the supervisor applies
         * them strictly in order. */
        completion_producer =
            load_cursor(state->transport.base, COMPLETION_PRODUCER_OFFSET,
                        __ATOMIC_ACQUIRE);
        while (request_consumer < request_producer) {
            uint64_t completion_consumer =
                load_cursor(state->transport.base,
                            COMPLETION_CONSUMER_OFFSET,
                            __ATOMIC_ACQUIRE);
            volatile uint8_t *request_record;
            struct block_request request;
            struct block_completion completion;
            struct watchdog_event event = {0};
            uint8_t ack;

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
            event.request_id = request.request_id;
            event.operation = request.operation;
            event.begin = 1U;
            if (atomic_load_explicit(state->quarantined, memory_order_acquire) ||
                watchdog_event_send(state->watchdog_fd, &event) != 0)
                return -1;
            if (request.operation == DVM_BLOCK_OP_READ) {
                uint64_t started;

                prefetch_pending_reads(state, request_consumer);
                started = monotonic_ns();
                (void)execute_request(state, &request, &completion);
                account_read(state, request.data_len, monotonic_ns() - started);
            } else {
                (void)execute_request(state, &request, &completion);
            }
            if (atomic_load_explicit(state->quarantined, memory_order_acquire)) {
                errno = EIO;
                return -1;
            }
            event.begin = (request_consumer + 1U < request_producer) ? 2U : 0U;
            event.request_consumer = request_consumer;
            event.completion_producer = completion_producer;
            encode_completion(event.completion, &completion);
            if (watchdog_event_send(state->watchdog_fd, &event) != 0) {
                errno = EIO;
                return -1;
            }
            /* A completion with more requests queued is not acknowledged: the
             * worker starts the next request while the supervisor publishes
             * this one. Only the last completion of a batch waits, so the
             * worker never goes back to sleep on the doorbell with a
             * completion unpublished. */
            if (event.begin == 0U &&
                (read(state->watchdog_fd, &ack, 1) != 1 || ack != 1U)) {
                errno = EIO;
                return -1;
            }
            request_consumer++;
            completion_producer++;
        }
    }
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
        fsync(evidence_fd) != 0)
        goto out;
    if (close(evidence_fd) != 0) {
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

    if (state->ready_published && state->transport.base != NULL) {
        (void)fetch_and_flags(state->transport.base,
                              ~DVM_BLOCK_FLAG_DVM_READY);
        (void)notify_host(&state->transport);
        state->ready_published = false;
    }
    directory_fd = open(READY_DIR, O_RDONLY | O_DIRECTORY | O_CLOEXEC |
                                      O_NOFOLLOW);
    if (directory_fd >= 0) {
        (void)unlinkat(directory_fd, BLOCK_EVIDENCE_FILE, 0);
        (void)unlinkat(directory_fd, BLOCK_EVIDENCE_NEXT, 0);
        (void)fsync(directory_fd);
        close(directory_fd);
    }
}

/* The supervisor remains alive and retains the lock after a failure. SIGKILL
 * cannot establish that a task in D state has stopped touching the device. */
static void supervise_worker(struct relay_state *state, int event_fd, pid_t worker,
                             unsigned int timeout_ms)
{
    struct watchdog_event active = {0};
    struct timespec deadline = {0};
    unsigned int unnotified = 0U;
    const char *cause = "worker-lost";
    for (;;) {
        struct pollfd pfd = {.fd = event_fd, .events = POLLIN};
        int timeout = -1;
        int result;
        if (active.request_id != 0U) {
            struct timespec now;
            int64_t remaining;
            if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
                cause = "clock-error";
                break;
            }
            remaining = (deadline.tv_sec - now.tv_sec) * INT64_C(1000) +
                        (deadline.tv_nsec - now.tv_nsec + 999999) / 1000000;
            if (remaining <= 0) {
                cause = "io-timeout";
                break;
            }
            timeout = remaining > INT_MAX ? INT_MAX : (int)remaining;
        }
        result = poll(&pfd, 1, timeout);
        if (result < 0 && errno == EINTR && !stop_requested)
            continue;
        if (result == 0) {
            cause = "io-timeout";
            break;
        }
        if (result <= 0 || !(pfd.revents & POLLIN))
            break;
        {
            struct watchdog_event event;
            ssize_t count = read(event_fd, &event, sizeof(event));
            if (count != (ssize_t)sizeof(event) || event.request_id == 0U ||
                event.begin > 2U ||
                (event.begin == 1U && active.request_id != 0U) ||
                (event.begin != 1U && (event.request_id != active.request_id ||
                                       event.operation != active.operation))) {
                cause = "watchdog-protocol";
                break;
            }
            if (event.begin == 1U) {
                active = event;
                if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
                    cause = "clock-error";
                    break;
                }
                deadline.tv_sec += timeout_ms / 1000U;
                deadline.tv_nsec += (long)(timeout_ms % 1000U) * 1000000L;
                if (deadline.tv_nsec >= 1000000000L) {
                    deadline.tv_sec++;
                    deadline.tv_nsec -= 1000000000L;
                }
            } else {
                struct timespec now;
                uint8_t ack = 1U;
                if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
                    now.tv_sec > deadline.tv_sec ||
                    (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec)) {
                    cause = "io-timeout";
                    break;
                }
                if (publish_watchdog_completion(state, &event) != 0) {
                    cause = "completion-publication";
                    break;
                }
                /* The last completion of a batch rings the doorbell and is
                 * acknowledged; earlier ones ring it every NOTIFY_EVERY
                 * completions so RustOS can consume a long batch while it is
                 * still running instead of only at its end. */
                if (event.begin == 0U) {
                    unnotified = 0U;
                    if (notify_host(&state->transport) != 0 ||
                        send(event_fd, &ack, 1, MSG_NOSIGNAL) != 1) {
                        cause = "completion-publication";
                        break;
                    }
                } else if (++unnotified >= COMPLETION_NOTIFY_EVERY) {
                    unnotified = 0U;
                    if (notify_host(&state->transport) != 0) {
                        cause = "completion-publication";
                        break;
                    }
                }
                active.request_id = 0U;
            }
        }
    }
    atomic_store_explicit(state->quarantined, true, memory_order_release);
    withdraw_ready(state);
    relay_log("rustos-dvm-block: quarantine generation=%" PRIu64
              " request_id=%" PRIu64 " operation=%u cause=%s outcome=unknown quiescence=unproven\n",
              state->header.generation, active.request_id, active.operation, cause);
    (void)kill(worker, SIGKILL);
    close(event_fd);
    /* Keep lock and mappings pinned. Hostd must not restart/reassign this
     * generation until independent quiescence has been established. */
    for (;;)
        pause();
}

static int serve(void)
{
    struct sigaction action;
    struct relay_state state;
    int lock_fd = -1;
    int result = 1;
    bool watchdog_worker = false;
    int events[2] = {-1, -1};
    pid_t worker;
    const char *failed_stage = "signal";

    memset(&state, 0, sizeof(state));
    state.transport.uio_fd = -1;
    state.device.fd = -1;
    state.watchdog_fd = -1;
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
        "rustos-dvm-block: ready abi=2 generation=%" PRIu64
        " controller=%s device=%s sectors=%" PRIu64
        " logical=%u physical=%u features=0x%" PRIx64
        " event=ivshmem-msix-uio\n",
        state.header.generation, state.device.controller_driver,
        state.device.name, state.device.capacity_sectors,
        state.device.logical_block_size,
        state.device.physical_block_size, state.header.features);

    failed_stage = "watchdog";
    state.quarantined = mmap(NULL, sizeof(*state.quarantined), PROT_READ | PROT_WRITE,
                             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (state.quarantined == MAP_FAILED) {
        state.quarantined = NULL;
        goto out;
    }
    atomic_init(state.quarantined, false);
    if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, events) != 0)
        goto out;
    worker = fork();
    if (worker < 0)
        goto out;
    if (worker > 0) {
        close(events[1]);
        supervise_worker(&state, events[0], worker, BLOCK_WATCHDOG_TIMEOUT_MS);
    }
    close(events[0]);
    watchdog_worker = true;
    state.watchdog_fd = events[1];
    {
        bool ready_confirmed = false;
        for (;;) {
            uint32_t event_count;
            ssize_t count;

            if (stop_requested)
                break;
            if (drain_requests(&state) != 0)
                goto out;
            if (!ready_confirmed && state.last_request_id != 0U) {
                ready_confirmed = true;
                relay_log(
                    "rustos-dvm-block: ready abi=2 generation=%" PRIu64
                    " controller=%s device=%s sectors=%" PRIu64
                    " logical=%u physical=%u features=0x%" PRIx64
                    " event=ivshmem-msix-uio\n",
                    state.header.generation, state.device.controller_driver,
                    state.device.name, state.device.capacity_sectors,
                    state.device.logical_block_size,
                    state.device.physical_block_size, state.header.features);
            }
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
    }
    result = 0;
out:
    if (result != 0)
        relay_log("rustos-dvm-block: revoked stage=%s errno=%d\n",
                  failed_stage, errno);
    if (lock_fd >= 0 && !watchdog_worker)
        withdraw_ready(&state);
    close_block_device(&state.device);
    close_transport(&state.transport);
    if (lock_fd >= 0)
        close(lock_fd);
    return result;
}

static void put_request(struct relay_state *state, uint64_t cursor,
                        uint64_t generation, uint32_t operation,
                        uint64_t sector, uint32_t length)
{
    uint8_t *record = (uint8_t *)state->transport.base +
                      DVM_BLOCK_REQUEST_RING_OFFSET +
                      (cursor % DVM_BLOCK_QUEUE_DEPTH) * DVM_BLOCK_RECORD_BYTES;

    memset(record, 0, DVM_BLOCK_RECORD_BYTES);
    write_le64(record, generation);
    write_le64(record + 8U, cursor + 1U);
    write_le32(record + 24U, operation);
    write_le32(record + 32U, (uint32_t)(cursor % DVM_BLOCK_QUEUE_DEPTH));
    write_le64(record + 40U, sector);
    write_le32(record + 48U, length);
}

/* Read-ahead planning must merge only adjacent reads, stop at anything that is
 * not a well-formed read of the current generation, and never invent a range
 * the request ring did not publish. */
static int selftest_prefetch_plan(struct relay_state *state)
{
    struct prefetch_range ranges[DVM_BLOCK_QUEUE_DEPTH];
    uint64_t through = 0U;
    size_t count;

    put_request(state, 0U, 7U, DVM_BLOCK_OP_READ, 0U, 4096U);
    put_request(state, 1U, 7U, DVM_BLOCK_OP_READ, 8U, 4096U);
    put_request(state, 2U, 7U, DVM_BLOCK_OP_READ, 16U, 4096U);
    put_request(state, 3U, 7U, DVM_BLOCK_OP_READ, 40U, 4096U);
    put_request(state, 4U, 7U, DVM_BLOCK_OP_WRITE, 56U, 4096U);
    put_request(state, 5U, 7U, DVM_BLOCK_OP_READ, 64U, 4096U);
    count = plan_prefetch(state, 0U, 6U, ranges, &through);
    if (count != 2U || through != 4U || ranges[0].offset != 0U ||
        ranges[0].bytes != 3U * 4096U || ranges[1].offset != 40U * 512U ||
        ranges[1].bytes != 4096U)
        return -1;
    /* A scan that starts after the merged head keeps the same tail ranges. */
    count = plan_prefetch(state, 2U, 4U, ranges, &through);
    if (count != 2U || through != 4U || ranges[0].offset != 16U * 512U ||
        ranges[0].bytes != 4096U)
        return -1;
    /* A wrong-generation, oversized or out-of-capacity record ends the scan. */
    put_request(state, 1U, 9U, DVM_BLOCK_OP_READ, 8U, 4096U);
    count = plan_prefetch(state, 0U, 3U, ranges, &through);
    if (count != 1U || through != 1U || ranges[0].bytes != 4096U)
        return -1;
    put_request(state, 1U, 7U, DVM_BLOCK_OP_READ, 8U,
                DVM_BLOCK_DATA_SLOT_BYTES + 4096U);
    if (plan_prefetch(state, 1U, 2U, ranges, &through) != 0U || through != 1U)
        return -1;
    put_request(state, 1U, 7U, DVM_BLOCK_OP_READ, 4096U, 4096U);
    if (plan_prefetch(state, 1U, 2U, ranges, &through) != 0U || through != 1U)
        return -1;
    /* An implausible cursor span is refused instead of walked. */
    if (plan_prefetch(state, 0U, DVM_BLOCK_QUEUE_DEPTH + 1U, ranges, &through) != 0U ||
        through != 0U)
        return -1;

    /* Issue against a real descriptor: counters advance once per range and the
     * cursor never revisits an examined record. */
    put_request(state, 0U, 7U, DVM_BLOCK_OP_READ, 0U, 4096U);
    put_request(state, 1U, 7U, DVM_BLOCK_OP_READ, 8U, 4096U);
    put_request(state, 2U, 7U, DVM_BLOCK_OP_READ, 16U, 4096U);
    store_cursor(state->transport.base, REQUEST_CONSUMER_OFFSET, 0U,
                 __ATOMIC_RELEASE);
    store_cursor(state->transport.base, REQUEST_PRODUCER_OFFSET, 3U,
                 __ATOMIC_RELEASE);
    state->prefetched_through = 0U;
    prefetch_pending_reads(state, 0U);
    if (state->stats.prefetch_calls != 1U ||
        state->stats.prefetch_bytes != 3U * 4096U ||
        state->prefetched_through != 3U)
        return -1;
    prefetch_pending_reads(state, 0U);
    if (state->stats.prefetch_calls != 1U)
        return -1;
    store_cursor(state->transport.base, REQUEST_PRODUCER_OFFSET, 0U,
                 __ATOMIC_RELEASE);
    state->prefetched_through = 0U;
    memset(&state->stats, 0, sizeof(state->stats));
    return 0;
}

/* Pause a worker after its final quarantine check, then quarantine before
 * it sends the prepared completion. Only the supervisor owns the cursor. */
static int selftest_quarantine_publication(void)
{
    struct relay_state state = {0};
    struct watchdog_event event = {.request_id = 1U};
    int sockets[2] = {-1, -1};
    int status = 0;
    int result = -1;
    pid_t child = -1;
    char byte;
    state.transport.base = mmap(NULL, DVM_BLOCK_APERTURE_BYTES, PROT_READ | PROT_WRITE,
                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    state.quarantined = mmap(NULL, sizeof(*state.quarantined), PROT_READ | PROT_WRITE,
                             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (state.transport.base == MAP_FAILED || state.quarantined == MAP_FAILED)
        goto out;
    atomic_init(state.quarantined, false);
    if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) != 0)
        goto out;
    child = fork();
    if (child < 0) goto out;
    if (child == 0) {
        close(sockets[0]);
        if (atomic_load_explicit(state.quarantined, memory_order_acquire) ||
            send(sockets[1], "R", 1, MSG_NOSIGNAL) != 1 ||
            read(sockets[1], &byte, 1) != 1 ||
            watchdog_event_send(sockets[1], &event) != 0)
            _exit(1);
        _exit(0);
    }
    close(sockets[1]); sockets[1] = -1;
    if (read(sockets[0], &byte, 1) != 1 || byte != 'R') goto out;
    atomic_store_explicit(state.quarantined, true, memory_order_release);
    if (send(sockets[0], "G", 1, MSG_NOSIGNAL) != 1 ||
        read(sockets[0], &event, sizeof(event)) != (ssize_t)sizeof(event) ||
        publish_watchdog_completion(&state, &event) == 0 ||
        load_cursor(state.transport.base, COMPLETION_PRODUCER_OFFSET, __ATOMIC_ACQUIRE) != 0U ||
        load_cursor(state.transport.base, REQUEST_CONSUMER_OFFSET, __ATOMIC_ACQUIRE) != 0U ||
        waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
        goto out;
    child = -1;
    result = 0;
out:
    if (child > 0) { (void)kill(child, SIGKILL); (void)waitpid(child, NULL, 0); }
    if (sockets[0] >= 0) close(sockets[0]);
    if (sockets[1] >= 0) close(sockets[1]);
    if (state.transport.base != NULL && state.transport.base != MAP_FAILED)
        munmap((void *)state.transport.base, DVM_BLOCK_APERTURE_BYTES);
    if (state.quarantined != NULL && state.quarantined != MAP_FAILED)
        munmap(state.quarantined, sizeof(*state.quarantined));
    return result;
}

static void selftest_identity(struct controller_identity *identity,
                              uint32_t pci_class, uint32_t vendor,
                              const char *pci_driver, unsigned int children,
                              uint32_t virtio_device,
                              const char *virtio_driver)
{
    memset(identity, 0, sizeof(*identity));
    identity->pci_class = pci_class;
    identity->pci_vendor = vendor;
    snprintf(identity->pci_driver, sizeof(identity->pci_driver), "%s",
             pci_driver);
    identity->virtio_children = children;
    identity->virtio_device = virtio_device;
    snprintf(identity->virtio_driver, sizeof(identity->virtio_driver), "%s",
             virtio_driver);
}

/* The admitted controller class set is closed: AHCI and NVMe by class code and
 * bound driver, virtio-blk by virtio device type and bound driver. Anything
 * else, including other virtio devices and ambiguous virtio functions, must not
 * classify. */
static int selftest_controller_classes(void)
{
    struct controller_identity identity;
    const char *admitted;

    selftest_identity(&identity, 0x010601U, 0x8086U, "ahci", 0U, 0U, "");
    admitted = classify_storage_controller(&identity);
    if (admitted == NULL || strcmp(admitted, "ahci") != 0)
        return -1;
    selftest_identity(&identity, 0x010802U, 0x144dU, "nvme", 0U, 0U, "");
    admitted = classify_storage_controller(&identity);
    if (admitted == NULL || strcmp(admitted, "nvme") != 0)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1af4U, "virtio-pci", 1U,
                      VIRTIO_ID_BLOCK, "virtio_blk");
    admitted = classify_storage_controller(&identity);
    if (admitted == NULL || strcmp(admitted, "virtio_blk") != 0 ||
        strlen(admitted) >= sizeof(((struct block_device *)0)->controller_driver))
        return -1;

    /* AHCI or NVMe class bound to a different driver is not that class. */
    selftest_identity(&identity, 0x010601U, 0x8086U, "pata_acpi", 0U, 0U, "");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010802U, 0x144dU, "vfio-pci", 0U, 0U, "");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    /* Another virtio device type, a non-virtio vendor, a foreign PCI driver,
     * an unbound or foreign virtio driver, and ambiguous children all fail. */
    selftest_identity(&identity, 0x010000U, 0x1af4U, "virtio-pci", 1U, 8U,
                      "virtio_scsi");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1234U, "virtio-pci", 1U,
                      VIRTIO_ID_BLOCK, "virtio_blk");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1af4U, "vfio-pci", 1U,
                      VIRTIO_ID_BLOCK, "virtio_blk");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1af4U, "virtio-pci", 1U,
                      VIRTIO_ID_BLOCK, "");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1af4U, "virtio-pci", 2U,
                      VIRTIO_ID_BLOCK, "virtio_blk");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x010000U, 0x1af4U, "virtio-pci", 0U, 0U, "");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    /* Legacy IDE and unrelated classes are not admitted. */
    selftest_identity(&identity, 0x010185U, 0x8086U, "ata_piix", 0U, 0U, "");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    selftest_identity(&identity, 0x020000U, 0x1af4U, "virtio-pci", 1U, 1U,
                      "virtio_net");
    if (classify_storage_controller(&identity) != NULL)
        return -1;
    return 0;
}

/* Request validation is parameterised by the signed logical block size: every
 * size the protocol admits works, none is special-cased, and read-only media
 * admits only reads and the media barrier. */
static int selftest_geometry_and_read_only(void)
{
    static const uint32_t block_sizes[] = {512U, 1024U, 2048U, 4096U};
    size_t index;

    for (index = 0U; index < sizeof(block_sizes) / sizeof(block_sizes[0]);
         index++) {
        struct relay_state state;
        struct block_request request;
        uint32_t block = block_sizes[index];
        uint32_t sectors_per_block = block / 512U;

        memset(&state, 0, sizeof(state));
        state.header.queue_depth = DVM_BLOCK_QUEUE_DEPTH;
        state.header.data_slot_bytes = DVM_BLOCK_DATA_SLOT_BYTES;
        state.header.generation = 3U;
        state.header.capacity_sectors = 4096U;
        state.header.logical_block_size = block;
        state.header.physical_block_size = block;
        state.header.features = DVM_BLOCK_FEATURE_FLUSH;
        state.header.flags = DVM_BLOCK_FLAG_READ_ONLY;
        state.device.read_only = true;
        request = (struct block_request){
            .generation = 3U,
            .request_id = 1U,
            .operation = DVM_BLOCK_OP_READ,
            .data_slot = 0U,
            .sector = sectors_per_block,
            .data_len = block,
        };
        if (!valid_request(&state, &request, 0U))
            return -1;
        /* Misaligned length or sector is invalid for this block size. */
        request.data_len = block + (block > 512U ? 512U : 1U);
        if (valid_request(&state, &request, 0U))
            return -1;
        request.data_len = block;
        if (block > 512U) {
            request.sector = 1U;
            if (valid_request(&state, &request, 0U))
                return -1;
            request.sector = sectors_per_block;
        }
        /* Read-only media: the media barrier is admitted... */
        request.operation = DVM_BLOCK_OP_FLUSH;
        request.operation_id = 1U;
        request.sector = 0U;
        request.data_len = 0U;
        if (!valid_request(&state, &request, 0U))
            return -1;
        /* ...but no mutation is, even with a writable-looking feature word. */
        state.header.features |= DVM_BLOCK_FEATURE_FUA |
                                 DVM_BLOCK_FEATURE_DISCARD |
                                 DVM_BLOCK_FEATURE_WRITE_ZEROES;
        request.operation = DVM_BLOCK_OP_WRITE;
        request.flags = 0U;
        request.sector = sectors_per_block;
        request.data_len = block;
        if (valid_request(&state, &request, 0U))
            return -1;
        request.operation = DVM_BLOCK_OP_DISCARD;
        if (valid_request(&state, &request, 0U))
            return -1;
        request.operation = DVM_BLOCK_OP_WRITE_ZEROES;
        if (valid_request(&state, &request, 0U))
            return -1;
        /* The same write is admitted once the device is writable. */
        state.device.read_only = false;
        state.header.flags = 0U;
        request.operation = DVM_BLOCK_OP_WRITE;
        if (!valid_request(&state, &request, 0U))
            return -1;
    }
    return 0;
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

    if (selftest_controller_classes() != 0)
        goto out;
    if (selftest_geometry_and_read_only() != 0)
        goto out;
    if (selftest_prefetch_plan(&state) != 0)
        goto out;
    if (selftest_quarantine_publication() != 0)
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
