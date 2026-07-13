// SPDX-License-Identifier: MIT
// Bounded host-authenticated KVM-vsock control agent for the RustOS Linux DVM.

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <linux/vm_sockets.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define CONTROL_FILE "/usr/share/rustos-dvm/control-plane-v1.env"
#define READY_DIR "/run/rustos-dvm"
#define READY_FILE READY_DIR "/ready"
#define CONTROL_PORT 40500U
#define MAX_FRAME 4096U
#define HOST_CID VMADDR_CID_HOST
#define INPUT_EVENT_LIMIT 64U
#define INPUT_BITS_BYTES ((KEY_MAX / 8U) + 1U)
#define POINTER_BUTTON_MASK 0x1fU
#define INPUT_SELFTEST_CMDLINE "rustos.dvm.input-selftest=1"
#define INPUT_SELFTEST_NAME "RustOS DVM input selftest"
#define INPUT_SELFTEST_CYCLES 800U
#define INPUT_SELFTEST_POLL_MS 25

enum input_device_kind {
    INPUT_DEVICE_KEYBOARD,
    INPUT_DEVICE_POINTER,
};

struct pointer_state {
    int16_t dx;
    int16_t dy;
    int16_t wheel_vertical;
    int16_t wheel_horizontal;
    uint8_t buttons;
    int pending;
};

struct input_selftest {
    int uinput_fd;
    unsigned int cycles_remaining;
    int enabled;
    int armed;
};

struct control_contract {
    char schema[16];
    char protocol[32];
    char state[32];
    char transport[32];
    char authentication[32];
    char capabilities[64];
};

static void die(const char *message) {
    fprintf(stderr, "rustos-dvm-agent: %s\n", message);
    exit(EXIT_FAILURE);
}

static int cmdline_has_option(const char *option) {
    FILE *file = fopen("/proc/cmdline", "re");
    char buffer[4096];
    size_t option_len = strlen(option);
    size_t bytes;
    char *cursor;

    if (file == NULL || option_len == 0 || option_len >= sizeof(buffer)) {
        if (file != NULL) {
            fclose(file);
        }
        return 0;
    }
    bytes = fread(buffer, 1, sizeof(buffer) - 1U, file);
    fclose(file);
    buffer[bytes] = '\0';
    cursor = buffer;
    while (*cursor != '\0') {
        char *end = cursor;
        while (*end != '\0' && *end != ' ' && *end != '\n' && *end != '\t') {
            end++;
        }
        if ((size_t)(end - cursor) == option_len && memcmp(cursor, option, option_len) == 0) {
            return 1;
        }
        cursor = end;
        while (*cursor == ' ' || *cursor == '\n' || *cursor == '\t') {
            cursor++;
        }
    }
    return 0;
}

static int write_input_event(int fd, uint16_t type, uint16_t code, int32_t value) {
    struct input_event event;
    ssize_t written;

    memset(&event, 0, sizeof(event));
    event.type = type;
    event.code = code;
    event.value = value;
    written = write(fd, &event, sizeof(event));
    return written == (ssize_t)sizeof(event) ? 0 : -1;
}

static void input_selftest_destroy(struct input_selftest *selftest) {
    if (selftest->uinput_fd >= 0) {
        (void)ioctl(selftest->uinput_fd, UI_DEV_DESTROY);
        close(selftest->uinput_fd);
    }
    selftest->uinput_fd = -1;
}

static int input_selftest_start(struct input_selftest *selftest) {
    struct uinput_setup setup;
    struct timespec settle = {.tv_sec = 0, .tv_nsec = 50 * 1000 * 1000};
    int fd;

    memset(selftest, 0, sizeof(*selftest));
    selftest->uinput_fd = -1;
    if (!cmdline_has_option(INPUT_SELFTEST_CMDLINE)) {
        return 0;
    }
    fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0 || ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0 || ioctl(fd, UI_SET_EVBIT, EV_REL) < 0 ||
        ioctl(fd, UI_SET_EVBIT, EV_SYN) < 0 || ioctl(fd, UI_SET_KEYBIT, KEY_A) < 0 ||
        ioctl(fd, UI_SET_KEYBIT, KEY_Z) < 0 || ioctl(fd, UI_SET_KEYBIT, KEY_SPACE) < 0 ||
        ioctl(fd, UI_SET_KEYBIT, BTN_LEFT) < 0 || ioctl(fd, UI_SET_RELBIT, REL_X) < 0 ||
        ioctl(fd, UI_SET_RELBIT, REL_Y) < 0 || ioctl(fd, UI_SET_RELBIT, REL_WHEEL) < 0) {
        if (fd >= 0) {
            close(fd);
        }
        return -1;
    }
    memset(&setup, 0, sizeof(setup));
    snprintf(setup.name, sizeof(setup.name), "%s", INPUT_SELFTEST_NAME);
    setup.id.bustype = BUS_VIRTUAL;
    setup.id.vendor = 0x5255;
    setup.id.product = 0x4456;
    setup.id.version = 1;
    if (ioctl(fd, UI_DEV_SETUP, &setup) < 0 || ioctl(fd, UI_DEV_CREATE) < 0) {
        close(fd);
        return -1;
    }
    (void)nanosleep(&settle, NULL);
    selftest->uinput_fd = fd;
    selftest->enabled = 1;
    fprintf(stderr, "rustos-dvm-agent: input selftest evdev ready\n");
    fflush(stderr);
    return 0;
}

static int input_selftest_emit_cycle(struct input_selftest *selftest) {
    int fd = selftest->uinput_fd;

    if (!selftest->armed || selftest->cycles_remaining == 0) {
        return 0;
    }
    if (write_input_event(fd, EV_KEY, KEY_A, 1) != 0 ||
        write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0 ||
        write_input_event(fd, EV_KEY, KEY_A, 0) != 0 ||
        write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0 ||
        write_input_event(fd, EV_REL, REL_X, 3) != 0 ||
        write_input_event(fd, EV_REL, REL_Y, -2) != 0 ||
        write_input_event(fd, EV_REL, REL_WHEEL, 1) != 0 ||
        write_input_event(fd, EV_KEY, BTN_LEFT, 1) != 0 ||
        write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0 ||
        write_input_event(fd, EV_KEY, BTN_LEFT, 0) != 0 ||
        write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0) {
        return -1;
    }
    selftest->cycles_remaining--;
    if (selftest->cycles_remaining == 0) {
        fprintf(stderr, "rustos-dvm-agent: input selftest emitted %u cycles\n",
                INPUT_SELFTEST_CYCLES);
        fflush(stderr);
    }
    return 0;
}

static void copy_value(char *destination, size_t destination_size, const char *value) {
    size_t length = strlen(value);
    if (length == 0 || length >= destination_size) {
        die("invalid control contract value");
    }
    memcpy(destination, value, length + 1);
}

static void parse_contract(struct control_contract *contract) {
    FILE *file = fopen(CONTROL_FILE, "re");
    char line[160];
    unsigned int seen = 0;

    if (file == NULL) {
        die("missing control contract");
    }
    memset(contract, 0, sizeof(*contract));
    while (fgets(line, sizeof(line), file) != NULL) {
        char *equals;
        char *value;
        line[strcspn(line, "\r\n")] = '\0';
        if (line[0] == '\0' || line[0] == '#') {
            continue;
        }
        equals = strchr(line, '=');
        if (equals == NULL || equals == line || equals[1] == '\0') {
            fclose(file);
            die("malformed control contract");
        }
        *equals = '\0';
        value = equals + 1;
        if (strcmp(line, "CONTROL_SCHEMA") == 0) {
            if ((seen & (1U << 0)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->schema, sizeof(contract->schema), value);
            seen |= 1U << 0;
        } else if (strcmp(line, "CONTROL_PROTOCOL") == 0) {
            if ((seen & (1U << 1)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->protocol, sizeof(contract->protocol), value);
            seen |= 1U << 1;
        } else if (strcmp(line, "CONTROL_STATE") == 0) {
            if ((seen & (1U << 2)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->state, sizeof(contract->state), value);
            seen |= 1U << 2;
        } else if (strcmp(line, "CONTROL_TRANSPORT") == 0) {
            if ((seen & (1U << 3)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->transport, sizeof(contract->transport), value);
            seen |= 1U << 3;
        } else if (strcmp(line, "CONTROL_AUTHENTICATION") == 0) {
            if ((seen & (1U << 4)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->authentication, sizeof(contract->authentication), value);
            seen |= 1U << 4;
        } else if (strcmp(line, "CONTROL_CAPABILITIES") == 0) {
            if ((seen & (1U << 5)) != 0) {
                fclose(file);
                die("duplicate control contract key");
            }
            copy_value(contract->capabilities, sizeof(contract->capabilities), value);
            seen |= 1U << 5;
        } else {
            fclose(file);
            die("unexpected control contract key");
        }
    }
    fclose(file);
    if (seen != 0x3fU || strcmp(contract->schema, "1") != 0 ||
        strcmp(contract->protocol, "agent-v1") != 0 ||
        strcmp(contract->state, "control") != 0 || strcmp(contract->transport, "kvm-vsock") != 0 ||
        strcmp(contract->authentication, "kvm-host-bound") != 0 ||
        strcmp(contract->capabilities,
               "health,device-inventory,driver-inventory,input-stream") != 0) {
        die("unsupported control contract");
    }
}

static void announce(const struct control_contract *contract) {
    FILE *file;
    if (mkdir(READY_DIR, 0700) != 0 && errno != EEXIST) {
        die("create state directory failed");
    }
    file = fopen(READY_FILE, "we");
    if (file == NULL) {
        die("write ready file failed");
    }
    if (fprintf(file,
                "schema=%s\nrole=linux-driver-domain\nprotocol=%s\nstate=%s\ntransport=%s\n"
                "authentication=%s\ncapabilities=%s\n",
                contract->schema, contract->protocol, contract->state, contract->transport,
                contract->authentication, contract->capabilities) < 0 ||
        fclose(file) != 0) {
        die("write ready file failed");
    }
}

static int write_all(int fd, const void *buffer, size_t length) {
    const unsigned char *cursor = buffer;
    while (length != 0) {
        ssize_t written = write(fd, cursor, length);
        if (written < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (written == 0) {
            errno = EPIPE;
            return -1;
        }
        cursor += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static int read_all(int fd, void *buffer, size_t length) {
    unsigned char *cursor = buffer;
    while (length != 0) {
        ssize_t received = read(fd, cursor, length);
        if (received < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (received == 0) {
            errno = ECONNRESET;
            return -1;
        }
        cursor += (size_t)received;
        length -= (size_t)received;
    }
    return 0;
}

static int send_frame(int fd, const char *payload) {
    uint32_t length = (uint32_t)strlen(payload);
    uint32_t network_length;
    if (length == 0 || length > MAX_FRAME) {
        errno = EMSGSIZE;
        return -1;
    }
    network_length = htonl(length);
    return write_all(fd, &network_length, sizeof(network_length)) == 0 &&
                   write_all(fd, payload, length) == 0
               ? 0
               : -1;
}

static int receive_frame(int fd, char *payload, size_t payload_size) {
    uint32_t network_length;
    uint32_t length;
    if (read_all(fd, &network_length, sizeof(network_length)) != 0) {
        return -1;
    }
    length = ntohl(network_length);
    if (length == 0 || length > MAX_FRAME || length >= payload_size) {
        errno = EMSGSIZE;
        return -1;
    }
    if (read_all(fd, payload, length) != 0) {
        return -1;
    }
    payload[length] = '\0';
    return 0;
}

static unsigned int pci_inventory_count(void) {
    DIR *directory = opendir("/sys/bus/pci/devices");
    struct dirent *entry;
    unsigned int count = 0;
    if (directory == NULL) {
        return 0;
    }
    while ((entry = readdir(directory)) != NULL) {
        if (entry->d_name[0] != '.') {
            count++;
        }
    }
    closedir(directory);
    return count;
}

/* A virtio driver's sysfs directory contains only its bind controls plus
 * symlinks named virtio<N> for devices currently bound to that driver. The
 * caller supplies fixed in-tree driver names; no host-provided path enters
 * this probe. */
static int virtio_driver_is_bound(const char *driver) {
    char path[PATH_MAX];
    DIR *directory;
    struct dirent *entry;

    if (driver == NULL || snprintf(path, sizeof(path), "/sys/bus/virtio/drivers/%s", driver) >=
                              (int)sizeof(path)) {
        return 0;
    }
    directory = opendir(path);
    if (directory == NULL) {
        return 0;
    }
    while ((entry = readdir(directory)) != NULL) {
        const char *name = entry->d_name;
        if (strncmp(name, "virtio", 6) == 0 && name[6] != '\0') {
            closedir(directory);
            return 1;
        }
    }
    closedir(directory);
    return 0;
}

static int request_id(const char *payload, const char *operation, unsigned int *id) {
    const char *id_line;
    const char *op_line;
    char *end;
    unsigned long parsed;
    if (strncmp(payload, "REQUEST\nid=", 11) != 0) {
        return -1;
    }
    id_line = payload + 11;
    parsed = strtoul(id_line, &end, 10);
    if (end == id_line || parsed > UINT_MAX || strncmp(end, "\nop=", 4) != 0) {
        return -1;
    }
    op_line = end + 4;
    if (strcmp(op_line, operation) != 0) {
        return -1;
    }
    *id = (unsigned int)parsed;
    return 0;
}

static int input_bit_is_set(const unsigned char *bits, unsigned int code) {
    return (bits[code / 8U] & (unsigned char)(1U << (code % 8U))) != 0;
}

static int input_has_capability(int fd, unsigned int event_type, unsigned int code) {
    unsigned char bits[INPUT_BITS_BYTES];
    memset(bits, 0, sizeof(bits));
    if (ioctl(fd, EVIOCGBIT(event_type, sizeof(bits)), bits) < 0) {
        return 0;
    }
    return code <= KEY_MAX && input_bit_is_set(bits, code);
}

static int input_device_matches(int fd, enum input_device_kind kind) {
    if (kind == INPUT_DEVICE_KEYBOARD) {
        /* A real keyboard is identified by the printable key set, not the
         * QEMU product name. This accepts physical keyboards passed through
         * to the DVM as well as virtio-input keyboards. */
        return input_has_capability(fd, EV_KEY, KEY_A) &&
               input_has_capability(fd, EV_KEY, KEY_Z) &&
               input_has_capability(fd, EV_KEY, KEY_SPACE);
    }
    return input_has_capability(fd, EV_REL, REL_X) && input_has_capability(fd, EV_REL, REL_Y) &&
           input_has_capability(fd, EV_KEY, BTN_LEFT);
}

static int input_device_name_matches(int fd, const char *expected) {
    char name[256];

    if (ioctl(fd, EVIOCGNAME(sizeof(name)), name) < 0) {
        return 0;
    }
    name[sizeof(name) - 1U] = '\0';
    return strcmp(name, expected) == 0;
}

static int open_input_device_index(unsigned int index) {
    char path[PATH_MAX];

    if (index >= INPUT_EVENT_LIMIT ||
        snprintf(path, sizeof(path), "/dev/input/event%u", index) >= (int)sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    return open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
}

static int open_input_device(enum input_device_kind kind, int excluded_index, int *index_out) {
    int prefer_selftest = cmdline_has_option(INPUT_SELFTEST_CMDLINE);
    unsigned int pass_count = prefer_selftest ? 2U : 1U;
    unsigned int pass;
    unsigned int index;
    for (pass = 0; pass < pass_count; pass++) {
        for (index = 0; index < INPUT_EVENT_LIMIT; index++) {
            int fd;
            if ((int)index == excluded_index) {
                continue;
            }
            fd = open_input_device_index(index);
            if (fd < 0) {
                continue;
            }
            if (input_device_matches(fd, kind) &&
                (!prefer_selftest || pass != 0 ||
                 input_device_name_matches(fd, INPUT_SELFTEST_NAME))) {
                *index_out = (int)index;
                return fd;
            }
            close(fd);
        }
    }
    errno = ENODEV;
    return -1;
}

static int16_t add_clamped_i16(int16_t current, int value) {
    long sum = (long)current + (long)value;
    if (sum > INT16_MAX) {
        return INT16_MAX;
    }
    if (sum < INT16_MIN) {
        return INT16_MIN;
    }
    return (int16_t)sum;
}

static int pointer_button_mask(unsigned int code, uint8_t *mask) {
    switch (code) {
    case BTN_LEFT:
        *mask = 1U << 0;
        return 0;
    case BTN_RIGHT:
        *mask = 1U << 1;
        return 0;
    case BTN_MIDDLE:
        *mask = 1U << 2;
        return 0;
    case BTN_SIDE:
        *mask = 1U << 3;
        return 0;
    case BTN_EXTRA:
        *mask = 1U << 4;
        return 0;
    default:
        return -1;
    }
}

static int send_keyboard_event(int fd, unsigned int request_id, const struct input_event *event) {
    char payload[192];
    if (event->code == KEY_RESERVED || event->code > KEY_MAX || event->value < 0 ||
        event->value > 2) {
        return 0;
    }
    if (snprintf(payload, sizeof(payload),
                 "EVENT\nid=%u\nop=input-stream\ntype=key\ncode=%u\nvalue=%d", request_id,
                 event->code, event->value) >= (int)sizeof(payload)) {
        errno = EMSGSIZE;
        return -1;
    }
    return send_frame(fd, payload);
}

static int send_pointer_packet(int fd, unsigned int request_id, struct pointer_state *state) {
    char payload[256];
    if (!state->pending) {
        return 0;
    }
    if (snprintf(payload, sizeof(payload),
                 "EVENT\nid=%u\nop=input-stream\ntype=pointer\ndx=%d\ndy=%d\nwheel-v=%d"
                 "\nwheel-h=%d\nbuttons=%u",
                 request_id, state->dx, state->dy, state->wheel_vertical,
                 state->wheel_horizontal, state->buttons) >= (int)sizeof(payload)) {
        errno = EMSGSIZE;
        return -1;
    }
    if (send_frame(fd, payload) != 0) {
        return -1;
    }
    state->dx = 0;
    state->dy = 0;
    state->wheel_vertical = 0;
    state->wheel_horizontal = 0;
    state->pending = 0;
    return 0;
}

static int consume_pointer_event(int fd, unsigned int request_id, struct pointer_state *state,
                                 const struct input_event *event) {
    uint8_t mask;
    if (event->type == EV_REL) {
        switch (event->code) {
        case REL_X:
            state->dx = add_clamped_i16(state->dx, event->value);
            break;
        case REL_Y:
            state->dy = add_clamped_i16(state->dy, event->value);
            break;
        case REL_WHEEL:
            state->wheel_vertical = add_clamped_i16(state->wheel_vertical, event->value);
            break;
        case REL_HWHEEL:
            state->wheel_horizontal = add_clamped_i16(state->wheel_horizontal, event->value);
            break;
        default:
            return 0;
        }
        state->pending = 1;
        return 0;
    }
    if (event->type == EV_KEY && pointer_button_mask(event->code, &mask) == 0) {
        if (event->value != 0) {
            state->buttons |= mask;
        } else {
            state->buttons &= (uint8_t)~mask;
        }
        state->buttons &= POINTER_BUTTON_MASK;
        state->pending = 1;
        return 0;
    }
    if (event->type == EV_SYN && event->code == SYN_REPORT) {
        return send_pointer_packet(fd, request_id, state);
    }
    if (event->type == EV_SYN && event->code == SYN_DROPPED) {
        errno = EOVERFLOW;
        return -1;
    }
    return 0;
}

static int stream_input_devices(int control_fd, int keyboard_fd, int pointer_fd,
                                unsigned int request_id, struct input_selftest *selftest) {
    struct pollfd pollfds[2];
    struct pointer_state pointer = {0};
    if (keyboard_fd < 0 || pointer_fd < 0) {
        errno = EINVAL;
        return -1;
    }
    pollfds[0].fd = keyboard_fd;
    pollfds[0].events = POLLIN;
    pollfds[1].fd = pointer_fd;
    pollfds[1].events = POLLIN;
    for (;;) {
        int ready;
        unsigned int index;
        pollfds[0].revents = 0;
        pollfds[1].revents = 0;
        ready = poll(pollfds, 2,
                     selftest->armed && selftest->cycles_remaining > 0 ? INPUT_SELFTEST_POLL_MS : -1);
        if (ready == 0) {
            if (input_selftest_emit_cycle(selftest) != 0) {
                return -1;
            }
            continue;
        }
        if (ready <= 0) {
            if (ready < 0 && errno == EINTR) {
                continue;
            }
            return -1;
        }
        for (index = 0; index < 2; index++) {
            struct input_event event;
            ssize_t bytes;
            if ((pollfds[index].revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
                errno = EIO;
                return -1;
            }
            if ((pollfds[index].revents & POLLIN) == 0) {
                continue;
            }
            bytes = read(pollfds[index].fd, &event, sizeof(event));
            if (bytes != (ssize_t)sizeof(event)) {
                if (bytes < 0 && (errno == EINTR || errno == EAGAIN)) {
                    continue;
                }
                return -1;
            }
            if (event.type == EV_SYN && event.code == SYN_DROPPED) {
                errno = EOVERFLOW;
                return -1;
            }
            if (index == 0) {
                if (event.type == EV_KEY && send_keyboard_event(control_fd, request_id, &event) != 0) {
                    return -1;
                }
            } else if (consume_pointer_event(control_fd, request_id, &pointer, &event) != 0) {
                return -1;
            }
        }
    }
}

static int serve_connection(int fd, const struct control_contract *contract,
                            struct input_selftest *selftest) {
    char payload[MAX_FRAME + 1];
    char hello[MAX_FRAME + 1];
    char welcome[192];
    unsigned int id;
    unsigned int inventory;

    snprintf(hello, sizeof(hello),
             "HELLO\nrole=linux-driver-domain\nprotocol=%s\nstate=%s\ntransport=%s\n"
             "authentication=%s\ncapabilities=%s",
             contract->protocol, contract->state, contract->transport, contract->authentication,
             contract->capabilities);
    snprintf(welcome, sizeof(welcome), "WELCOME\nprotocol=%s\ncapabilities=%s", contract->protocol,
             contract->capabilities);
    if (send_frame(fd, hello) != 0 || receive_frame(fd, payload, sizeof(payload)) != 0 ||
        strcmp(payload, welcome) != 0) {
        return -1;
    }
    for (;;) {
        if (receive_frame(fd, payload, sizeof(payload)) != 0) {
            return -1;
        }
        if (request_id(payload, "health", &id) == 0) {
            snprintf(payload, sizeof(payload), "RESPONSE\nid=%u\nop=health\nstatus=ok\nvalue=ready", id);
        } else if (request_id(payload, "device-inventory", &id) == 0) {
            inventory = pci_inventory_count();
            snprintf(payload, sizeof(payload),
                     "RESPONSE\nid=%u\nop=device-inventory\nstatus=ok\ncount=%u", id, inventory);
        } else if (request_id(payload, "driver-inventory", &id) == 0) {
            const char *virtio_net = virtio_driver_is_bound("virtio_net") ? "bound" : "missing";
            const char *virtio_gpu = virtio_driver_is_bound("virtio_gpu") ? "bound" : "missing";
            snprintf(payload, sizeof(payload),
                     "RESPONSE\nid=%u\nop=driver-inventory\nstatus=ok\nvirtio-net=%s\n"
                     "virtio-gpu=%s",
                     id, virtio_net, virtio_gpu);
        } else if (request_id(payload, "input-stream", &id) == 0) {
            int keyboard_index = -1;
            int keyboard_fd = open_input_device(INPUT_DEVICE_KEYBOARD, -1, &keyboard_index);
            int pointer_index = -1;
            int pointer_fd = -1;
            if (keyboard_fd >= 0 && input_device_matches(keyboard_fd, INPUT_DEVICE_POINTER)) {
                /* A composite HID can expose keyboard and relative-pointer
                 * capabilities through one evdev node. Reopen it instead of
                 * dup(2): each evdev open needs its own event queue so the
                 * keyboard and pointer consumers cannot steal each other's
                 * records. */
                pointer_fd = open_input_device_index((unsigned int)keyboard_index);
                if (pointer_fd >= 0) {
                    pointer_index = keyboard_index;
                }
            } else if (keyboard_fd >= 0) {
                pointer_fd = open_input_device(INPUT_DEVICE_POINTER, keyboard_index, &pointer_index);
            }
            if (keyboard_fd < 0 || pointer_fd < 0) {
                if (keyboard_fd >= 0) {
                    close(keyboard_fd);
                }
                if (pointer_fd >= 0) {
                    close(pointer_fd);
                }
                snprintf(payload, sizeof(payload),
                         "RESPONSE\nid=%u\nop=input-stream\nstatus=error\nreason=input-unavailable",
                         id);
            } else {
                snprintf(payload, sizeof(payload),
                         "RESPONSE\nid=%u\nop=input-stream\nstatus=ready\nformat=linux-evdev-v2"
                         "\nkeyboard=event%d\npointer=event%d",
                         id, keyboard_index, pointer_index);
                if (send_frame(fd, payload) != 0) {
                    close(keyboard_fd);
                    close(pointer_fd);
                    return -1;
                }
                if (selftest->enabled) {
                    selftest->armed = 1;
                    selftest->cycles_remaining = INPUT_SELFTEST_CYCLES;
                    /* Both evdev file descriptions are open now. Queue the
                     * first cycle before entering poll(2), so readiness does
                     * not depend on a timeout winning over unrelated input
                     * wakeups during concurrent guest startup. Subsequent
                     * cycles remain rate-limited by the normal poll loop. */
                    if (input_selftest_emit_cycle(selftest) != 0) {
                        close(keyboard_fd);
                        close(pointer_fd);
                        return -1;
                    }
                    fprintf(stderr, "rustos-dvm-agent: input selftest stream armed\n");
                    fflush(stderr);
                }
                if (stream_input_devices(fd, keyboard_fd, pointer_fd, id, selftest) != 0) {
                    close(keyboard_fd);
                    close(pointer_fd);
                    return -1;
                }
                close(keyboard_fd);
                close(pointer_fd);
                return -1;
            }
        } else {
            return -1;
        }
        if (send_frame(fd, payload) != 0) {
            return -1;
        }
    }
}

static int connect_host(void) {
    int fd;
    struct sockaddr_vm address = {
        .svm_family = AF_VSOCK,
        .svm_port = CONTROL_PORT,
        .svm_cid = HOST_CID,
    };
    fd = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        return -1;
    }
    if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void serve(const struct control_contract *contract) {
    struct input_selftest selftest;

    if (input_selftest_start(&selftest) != 0) {
        die("input selftest requested but uinput setup failed");
    }
    announce(contract);
    fprintf(stderr, "rustos-dvm-agent: ready protocol=%s state=%s\n", contract->protocol,
            contract->state);
    fflush(stderr);
    for (;;) {
        int fd = connect_host();
        if (fd >= 0) {
            if (serve_connection(fd, contract, &selftest) != 0) {
                fprintf(stderr, "rustos-dvm-agent: host control disconnected\n");
            }
            close(fd);
        }
        sleep(1);
    }
    input_selftest_destroy(&selftest);
}

int main(int argc, char **argv) {
    struct control_contract contract;
    parse_contract(&contract);
    if (argc == 1 || strcmp(argv[1], "announce") == 0) {
        announce(&contract);
        printf("rustos-dvm-agent: ready protocol=%s state=%s\n", contract.protocol, contract.state);
        return EXIT_SUCCESS;
    }
    if (strcmp(argv[1], "health") == 0) {
        return access(READY_FILE, R_OK) == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
    }
    if (strcmp(argv[1], "serve") == 0) {
        serve(&contract);
        return EXIT_SUCCESS;
    }
    fprintf(stderr, "usage: %s {announce|health|serve}\n", argv[0]);
    return EXIT_FAILURE;
}
