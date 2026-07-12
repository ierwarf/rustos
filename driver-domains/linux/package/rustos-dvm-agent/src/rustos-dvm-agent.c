// SPDX-License-Identifier: MIT
// Bounded host-authenticated KVM-vsock control agent for the RustOS Linux DVM.

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/input.h>
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
#include <unistd.h>

#define CONTROL_FILE "/usr/share/rustos-dvm/control-plane-v1.env"
#define READY_DIR "/run/rustos-dvm"
#define READY_FILE READY_DIR "/ready"
#define CONTROL_PORT 40500U
#define MAX_FRAME 4096U
#define HOST_CID VMADDR_CID_HOST
#define KEYBOARD_EVENT_LIMIT 32U
#define KEYBOARD_EVENT_WAIT_MS 5000
#define VIRTIO_KEYBOARD_NAME "QEMU Virtio Keyboard"

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
        strcmp(contract->capabilities, "health,device-inventory,keyboard-events") != 0) {
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

static int virtio_keyboard_event_path(char *path, size_t path_size) {
    unsigned int index;
    for (index = 0; index < KEYBOARD_EVENT_LIMIT; index++) {
        char name_path[PATH_MAX];
        char name[128];
        FILE *file;
        snprintf(name_path, sizeof(name_path), "/sys/class/input/event%u/device/name", index);
        file = fopen(name_path, "re");
        if (file == NULL) {
            continue;
        }
        if (fgets(name, sizeof(name), file) == NULL) {
            fclose(file);
            continue;
        }
        fclose(file);
        name[strcspn(name, "\r\n")] = '\0';
        if (strcmp(name, VIRTIO_KEYBOARD_NAME) == 0) {
            if (snprintf(path, path_size, "/dev/input/event%u", index) >= (int)path_size) {
                errno = ENAMETOOLONG;
                return -1;
            }
            return 0;
        }
    }
    errno = ENODEV;
    return -1;
}

static int open_virtio_keyboard(void) {
    char path[PATH_MAX];
    int fd;
    if (virtio_keyboard_event_path(path, sizeof(path)) != 0) {
        return -1;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC);
    return fd;
}

static int wait_for_virtio_keyboard_press(int fd, unsigned int *code) {
    struct pollfd pollfd;
    struct timespec deadline;
    if (fd < 0) {
        errno = EINVAL;
        return -1;
    }
    pollfd.fd = fd;
    pollfd.events = POLLIN;
    pollfd.revents = 0;
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
        return -1;
    }
    deadline.tv_sec += KEYBOARD_EVENT_WAIT_MS / 1000U;
    deadline.tv_nsec += (long)(KEYBOARD_EVENT_WAIT_MS % 1000U) * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_sec++;
        deadline.tv_nsec -= 1000000000L;
    }
    for (;;) {
        struct input_event event;
        struct timespec now;
        ssize_t bytes;
        long long remaining_ms;
        int ready;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            return -1;
        }
        remaining_ms = ((long long)deadline.tv_sec - (long long)now.tv_sec) * 1000LL +
                       ((long long)deadline.tv_nsec - (long long)now.tv_nsec + 999999LL) /
                           1000000LL;
        if (remaining_ms <= 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        ready = poll(&pollfd, 1, (int)remaining_ms);
        if (ready <= 0) {
            if (ready < 0 && errno == EINTR) {
                continue;
            }
            if (ready == 0) {
                errno = ETIMEDOUT;
            }
            return -1;
        }
        if ((pollfd.revents & (POLLERR | POLLHUP | POLLNVAL)) != 0 ||
            (pollfd.revents & POLLIN) == 0) {
            errno = EIO;
            return -1;
        }
        bytes = read(fd, &event, sizeof(event));
        if (bytes != (ssize_t)sizeof(event)) {
            if (bytes < 0 && errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (event.type == EV_KEY && event.value == 1 && event.code != KEY_RESERVED) {
            *code = event.code;
            return 0;
        }
    }
}

static int serve_connection(int fd, const struct control_contract *contract) {
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
        } else if (request_id(payload, "keyboard-event", &id) == 0) {
            unsigned int code;
            int keyboard_fd = open_virtio_keyboard();
            if (keyboard_fd < 0) {
                snprintf(payload, sizeof(payload),
                         "RESPONSE\nid=%u\nop=keyboard-event\nstatus=error\nreason=keyboard-unavailable",
                         id);
            } else {
                snprintf(payload, sizeof(payload), "READY\nid=%u\nop=keyboard-event\nstatus=ready",
                         id);
                if (send_frame(fd, payload) != 0) {
                    close(keyboard_fd);
                    return -1;
                }
                if (wait_for_virtio_keyboard_press(keyboard_fd, &code) != 0) {
                    snprintf(payload, sizeof(payload),
                             "RESPONSE\nid=%u\nop=keyboard-event\nstatus=error\nreason=keyboard-unavailable",
                             id);
                } else {
                    snprintf(payload, sizeof(payload),
                             "RESPONSE\nid=%u\nop=keyboard-event\nstatus=ok\ntype=key\ncode=%u\nvalue=1",
                             id, code);
                }
                close(keyboard_fd);
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
    announce(contract);
    fprintf(stderr, "rustos-dvm-agent: ready protocol=%s state=%s\n", contract->protocol,
            contract->state);
    fflush(stderr);
    for (;;) {
        int fd = connect_host();
        if (fd >= 0) {
            if (serve_connection(fd, contract) != 0) {
                fprintf(stderr, "rustos-dvm-agent: host control disconnected\n");
            }
            close(fd);
        }
        sleep(1);
    }
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
