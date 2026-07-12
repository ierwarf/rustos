// SPDX-License-Identifier: MIT
// Bounded host-authenticated KVM-vsock control agent for the RustOS Linux DVM.

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <linux/vm_sockets.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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
            copy_value(contract->schema, sizeof(contract->schema), value);
        } else if (strcmp(line, "CONTROL_PROTOCOL") == 0) {
            copy_value(contract->protocol, sizeof(contract->protocol), value);
        } else if (strcmp(line, "CONTROL_STATE") == 0) {
            copy_value(contract->state, sizeof(contract->state), value);
        } else if (strcmp(line, "CONTROL_TRANSPORT") == 0) {
            copy_value(contract->transport, sizeof(contract->transport), value);
        } else if (strcmp(line, "CONTROL_AUTHENTICATION") == 0) {
            copy_value(contract->authentication, sizeof(contract->authentication), value);
        } else if (strcmp(line, "CONTROL_CAPABILITIES") == 0) {
            copy_value(contract->capabilities, sizeof(contract->capabilities), value);
        }
    }
    fclose(file);
    if (strcmp(contract->schema, "1") != 0 || strcmp(contract->protocol, "agent-v1") != 0 ||
        strcmp(contract->state, "control") != 0 || strcmp(contract->transport, "kvm-vsock") != 0 ||
        strcmp(contract->authentication, "kvm-host-bound") != 0 ||
        strcmp(contract->capabilities, "health,device-inventory") != 0) {
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
    char expected[96];
    const char *id_line = strstr(payload, "\nid=");
    const char *op_line = strstr(payload, "\nop=");
    char *end;
    unsigned long parsed;
    if (strncmp(payload, "REQUEST\n", 8) != 0 || id_line == NULL || op_line == NULL) {
        return -1;
    }
    parsed = strtoul(id_line + 4, &end, 10);
    if (end == id_line + 4 || *end != '\n' || parsed > UINT_MAX) {
        return -1;
    }
    snprintf(expected, sizeof(expected), "\nop=%s", operation);
    if (strncmp(op_line, expected, strlen(expected)) != 0) {
        return -1;
    }
    *id = (unsigned int)parsed;
    return 0;
}

static int serve_connection(int fd, const struct control_contract *contract) {
    char payload[MAX_FRAME + 1];
    char hello[MAX_FRAME + 1];
    unsigned int id;
    unsigned int inventory;

    snprintf(hello, sizeof(hello),
             "HELLO\nrole=linux-driver-domain\nprotocol=%s\nstate=%s\ntransport=%s\n"
             "authentication=%s\ncapabilities=%s",
             contract->protocol, contract->state, contract->transport, contract->authentication,
             contract->capabilities);
    if (send_frame(fd, hello) != 0 || receive_frame(fd, payload, sizeof(payload)) != 0 ||
        strcmp(payload, "WELCOME\nprotocol=agent-v1\ncapabilities=health,device-inventory") != 0) {
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
