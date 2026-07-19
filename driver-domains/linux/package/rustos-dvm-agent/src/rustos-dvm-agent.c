// SPDX-License-Identifier: MIT
// Bounded host-authenticated KVM-vsock control agent for the RustOS Linux DVM.

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <linux/input.h>
#include <linux/if_alg.h>
#include <linux/uinput.h>
#include <linux/vm_sockets.h>
#include <poll.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/file.h>
#include <unistd.h>

#define CONTROL_FILE "/usr/share/rustos-dvm/control-plane-v1.env"
#define READY_DIR "/run/rustos-dvm"
#define READY_FILE READY_DIR "/ready"
#define READY_OWNER_NAME "agent-owner.lock"
#define READY_CANDIDATE_NAME ".ready.next"
#define DISPLAY_READY_LOCK READY_DIR "/display-ready.lock"
#define DISPLAY_EVIDENCE_FILE READY_DIR "/display-evidence-v2.env"
#define CONTROL_PORT_FLOOR 49152U
#define CONTROL_PORT_SPAN (UINT32_MAX - CONTROL_PORT_FLOOR + 1U)
#define MAX_FRAME 4096U
#define HOST_CID VMADDR_CID_HOST
#define CONTROL_SECRET_BYTES 32U
#define CONTROL_SECRET_HEX_BYTES (CONTROL_SECRET_BYTES * 2U)
#define CONTROL_SECRET_FW_CFG \
    "/sys/firmware/qemu_fw_cfg/by_name/opt/rustos/dvm-control-secret/raw"
#define CONTROL_PROOF_CONTEXT "rustos-dvm-control-hmac-v1"
#define CONTROL_PROOF_CONTEXT_BYTES (sizeof(CONTROL_PROOF_CONTEXT))
#define INPUT_EVENT_LIMIT 64U
#define INPUT_BITS_BYTES ((KEY_MAX / 8U) + 1U)
#define POINTER_BUTTON_MASK 0x1fU
#define INPUT_SELFTEST_CMDLINE "rustos.dvm.input-selftest=1"
#define INPUT_SELFTEST_NAME "RustOS DVM input selftest"
#define INPUT_SELFTEST_CYCLES 4000U
#define INPUT_SELFTEST_POLL_MS 10
#define INPUT_SELFTEST_INTERVAL_NS \
    ((uint64_t)INPUT_SELFTEST_POLL_MS * 1000ULL * 1000ULL)
#define INPUT_SELFTEST_LEG_CYCLES 64U
#define INPUT_RELAY_RR_PRIORITY 10
#define INPUT_RELAY_RTTIME_SOFT_US 50000U
#define INPUT_RELAY_RTTIME_HARD_US 100000U
// Forty seconds covers the public 30-second KVM gate plus bounded guest boot
// and stream admission time. The synthetic producer still terminates and
// therefore cannot become an unbounded DVM workload.
// RDI3 uses a deliberately bounded serial transport. Coalescing relative
// motion at 100Hz preserves responsive desktop input without outrunning the
// measured end-to-end consumer; button state is forwarded immediately.
#define INPUT_POINTER_FLUSH_MS 5
#define INPUT_POINTER_FLUSH_NS ((uint64_t)INPUT_POINTER_FLUSH_MS * 1000000ULL)
// The interactive KVM topology has a fixed 1600x900 DVM scanout. An absolute
// virtio tablet is normalized to that scanout and remains absolute through
// the authenticated RDI3 frame, so hovering needs no manual grab and an
// initial/duplicate tablet report cannot synthesize relative cursor motion.
#define DVM_POINTER_WIDTH 1600
#define DVM_POINTER_HEIGHT 900

enum input_device_kind {
    INPUT_DEVICE_KEYBOARD,
    INPUT_DEVICE_POINTER,
};

struct input_scheduler_guard {
    int active;
    int fatal;
    int saved_policy;
    struct sched_param saved_param;
    struct rlimit saved_rttime;
};

struct pointer_state {
    int16_t dx;
    int16_t dy;
    int16_t wheel_vertical;
    int16_t wheel_horizontal;
    uint8_t buttons;
    int pending;
    int buttons_changed;
    uint64_t flush_deadline_ns;
    int absolute_mode;
    int absolute_initialized;
    int absolute_published;
    int absolute_seen_x;
    int absolute_seen_y;
    int absolute_min_x;
    int absolute_max_x;
    int absolute_min_y;
    int absolute_max_y;
    int absolute_x;
    int absolute_y;
    int absolute_previous_x;
    int absolute_previous_y;
};

struct input_selftest {
    int uinput_fd;
    int32_t pointer_x;
    int32_t pointer_y;
    unsigned int cycles_remaining;
    unsigned int motion_phase;
    uint64_t next_emit_ns;
    int enabled;
    int armed;
};

static int monotonic_time_ns(uint64_t *out);
static int read_all(int fd, void *buffer, size_t length);

struct control_contract {
    char schema[16];
    char protocol[32];
    char state[32];
    char transport[32];
    char authentication[32];
    char capabilities[96];
};

struct ready_owner_guard {
    int singleton_fd;
    int ready_fd;
};

struct display_evidence_sample {
    uint64_t sequence;
    uint64_t monotonic_ns;
    uint64_t window_ns;
    uint64_t pageflip_completions;
    uint64_t frame_hz_milli;
    uint64_t cpu_copy_us_avg;
    uint64_t pageflip_latency_us_avg;
    uint64_t pageflip_latency_us_max;
    uint64_t atomic_commit_us_avg;
    uint32_t connector_id;
    uint32_t mode_width;
    uint32_t mode_height;
};

static void die(const char *message) {
    fprintf(stderr, "rustos-dvm-agent: %s\n", message);
    exit(EXIT_FAILURE);
}

static int input_scheduler_leave(struct input_scheduler_guard *guard) {
    struct sched_param observed_param;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno = 0;

    if (!guard->active) {
        return guard->fatal ? -1 : 0;
    }
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
 * Input transport is the only latency-critical work in the control agent.
 * Admit SCHED_RR only for an authenticated live stream and pair it with a
 * strict continuous-CPU ceiling. A wedged or hostile loop is therefore killed
 * by Linux rather than starving KMS, recovery, or the control plane forever.
 */
static int input_scheduler_enter(struct input_scheduler_guard *guard) {
    struct rlimit bounded_rttime = {
        .rlim_cur = INPUT_RELAY_RTTIME_SOFT_US,
        .rlim_max = INPUT_RELAY_RTTIME_HARD_US,
    };
    struct sched_param realtime = {.sched_priority = INPUT_RELAY_RR_PRIORITY};
    struct sched_param observed;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno;

    memset(guard, 0, sizeof(*guard));
    guard->saved_policy = sched_getscheduler(0);
    if (guard->saved_policy < 0 || sched_getparam(0, &guard->saved_param) != 0 ||
        getrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0) {
        return -1;
    }
    if (guard->saved_policy != SCHED_OTHER || guard->saved_param.sched_priority != 0) {
        errno = EINVAL;
        return -1;
    }
    if (setrlimit(RLIMIT_RTTIME, &bounded_rttime) != 0) {
        return -1;
    }
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
        observed.sched_priority != INPUT_RELAY_RR_PRIORITY ||
        getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
        observed_rttime.rlim_cur != bounded_rttime.rlim_cur ||
        observed_rttime.rlim_max != bounded_rttime.rlim_max) {
        saved_errno = errno != 0 ? errno : EINVAL;
        (void)input_scheduler_leave(guard);
        errno = saved_errno;
        return -1;
    }
    return 0;
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
    struct uinput_abs_setup abs_setup;
    struct uinput_setup setup;
    struct timespec settle = {.tv_sec = 0, .tv_nsec = 50 * 1000 * 1000};
    int fd;

    memset(selftest, 0, sizeof(*selftest));
    selftest->uinput_fd = -1;
    if (!cmdline_has_option(INPUT_SELFTEST_CMDLINE)) {
        return 0;
    }
    fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0 || ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0 || ioctl(fd, UI_SET_EVBIT, EV_ABS) < 0 ||
        ioctl(fd, UI_SET_EVBIT, EV_SYN) < 0 || ioctl(fd, UI_SET_KEYBIT, KEY_A) < 0 ||
        ioctl(fd, UI_SET_KEYBIT, KEY_Z) < 0 || ioctl(fd, UI_SET_KEYBIT, KEY_SPACE) < 0 ||
        ioctl(fd, UI_SET_KEYBIT, KEY_F12) < 0 || ioctl(fd, UI_SET_KEYBIT, BTN_LEFT) < 0 ||
        ioctl(fd, UI_SET_ABSBIT, ABS_X) < 0 || ioctl(fd, UI_SET_ABSBIT, ABS_Y) < 0) {
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
    if (ioctl(fd, UI_DEV_SETUP, &setup) < 0) {
        close(fd);
        return -1;
    }
    memset(&abs_setup, 0, sizeof(abs_setup));
    abs_setup.code = ABS_X;
    abs_setup.absinfo.minimum = 0;
    abs_setup.absinfo.maximum = DVM_POINTER_WIDTH - 1;
    if (ioctl(fd, UI_ABS_SETUP, &abs_setup) < 0) {
        close(fd);
        return -1;
    }
    abs_setup.code = ABS_Y;
    abs_setup.absinfo.maximum = DVM_POINTER_HEIGHT - 1;
    if (ioctl(fd, UI_ABS_SETUP, &abs_setup) < 0 || ioctl(fd, UI_DEV_CREATE) < 0) {
        close(fd);
        return -1;
    }
    (void)nanosleep(&settle, NULL);
    selftest->uinput_fd = fd;
    selftest->pointer_x = DVM_POINTER_WIDTH / 2;
    selftest->pointer_y = DVM_POINTER_HEIGHT / 2;
    selftest->enabled = 1;
    fprintf(stderr, "rustos-dvm-agent: input selftest evdev ready\n");
    fflush(stderr);
    return 0;
}

static int input_selftest_emit_cycle(struct input_selftest *selftest) {
    int fd = selftest->uinput_fd;
    int32_t dx;
    int32_t dy;
    uint64_t now;

    if (!selftest->armed || selftest->cycles_remaining == 0) {
        return 0;
    }
    if (monotonic_time_ns(&now) != 0) {
        return -1;
    }
    if (selftest->next_emit_ns != 0U && now < selftest->next_emit_ns) {
        return 0;
    }
    if (selftest->next_emit_ns == 0U) {
        selftest->next_emit_ns = now + INPUT_SELFTEST_INTERVAL_NS;
    } else {
        do {
            selftest->next_emit_ns += INPUT_SELFTEST_INTERVAL_NS;
        } while (selftest->next_emit_ns <= now);
    }
    /*
     * Trace a 192-pixel absolute square at constant speed. The former 24x16-pixel
     * center oscillation looked like a broken, trembling physical mouse even
     * when transport was healthy. This path stays away from screen edges but
     * makes every accepted motion visually distinguishable.
     */
    switch ((selftest->motion_phase / INPUT_SELFTEST_LEG_CYCLES) % 4U) {
    case 0U:
        dx = 3;
        dy = 0;
        break;
    case 1U:
        dx = 0;
        dy = 3;
        break;
    case 2U:
        dx = -3;
        dy = 0;
        break;
    default:
        dx = 0;
        dy = -3;
        break;
    }
    /*
     * The validation stream must never type into a focused shell or click a
     * user's desktop. One initial KEY_F12 press/release proves keyboard
     * ingress; every later cycle is pointer-only, so a long motion run cannot
     * become a console-command flood. BTN_LEFT is advertised only so this
     * intentionally composite uinput device is selected by the normal pointer
     * capability test; no button event is emitted. Printable keys, wheel
     * motion, and pointer buttons are deliberately excluded.
     */
    if (selftest->motion_phase == 0U &&
        (write_input_event(fd, EV_KEY, KEY_F12, 1) != 0 ||
         write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0 ||
         write_input_event(fd, EV_KEY, KEY_F12, 0) != 0 ||
         write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0)) {
        return -1;
    }
    selftest->pointer_x += dx;
    selftest->pointer_y += dy;
    if (selftest->pointer_x < 0) {
        selftest->pointer_x = 0;
    } else if (selftest->pointer_x >= DVM_POINTER_WIDTH) {
        selftest->pointer_x = DVM_POINTER_WIDTH - 1;
    }
    if (selftest->pointer_y < 0) {
        selftest->pointer_y = 0;
    } else if (selftest->pointer_y >= DVM_POINTER_HEIGHT) {
        selftest->pointer_y = DVM_POINTER_HEIGHT - 1;
    }
    if (write_input_event(fd, EV_ABS, ABS_X, selftest->pointer_x) != 0 ||
        write_input_event(fd, EV_ABS, ABS_Y, selftest->pointer_y) != 0 ||
        write_input_event(fd, EV_SYN, SYN_REPORT, 0) != 0) {
        return -1;
    }
    selftest->motion_phase++;
    selftest->cycles_remaining--;
    if (selftest->cycles_remaining == 0) {
        fprintf(stderr, "rustos-dvm-agent: input selftest emitted %u cycles\n",
                INPUT_SELFTEST_CYCLES);
        fflush(stderr);
    }
    return 0;
}

static int input_selftest_timeout_ms(const struct input_selftest *selftest) {
    uint64_t now;
    uint64_t remaining_ns;
    uint64_t remaining_ms;
    if (!selftest->armed || selftest->cycles_remaining == 0U) {
        return -1;
    }
    if (selftest->next_emit_ns == 0U || monotonic_time_ns(&now) != 0 ||
        now >= selftest->next_emit_ns) {
        return 0;
    }
    remaining_ns = selftest->next_emit_ns - now;
    remaining_ms = (remaining_ns + 999999ULL) / 1000000ULL;
    return remaining_ms > (uint64_t)INT_MAX ? INT_MAX : (int)remaining_ms;
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
        strcmp(contract->authentication, "dvm-agent-hmac-sha256-v1") != 0 ||
        strcmp(contract->capabilities,
               "health,device-inventory,driver-inventory,display-evidence-v2,input-stream") != 0) {
        die("unsupported control contract");
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

static int format_ready_payload(const struct control_contract *contract, char *payload,
                                size_t capacity, size_t *length) {
    int formatted = snprintf(payload, capacity,
                             "schema=%s\nrole=linux-driver-domain\nprotocol=%s\nstate=%s\n"
                             "transport=%s\nauthentication=%s\ncapabilities=%s\n",
                             contract->schema, contract->protocol, contract->state,
                             contract->transport, contract->authentication,
                             contract->capabilities);
    if (formatted < 0 || (size_t)formatted >= capacity) {
        errno = EOVERFLOW;
        return -1;
    }
    *length = (size_t)formatted;
    return 0;
}

static int open_ready_directory(int create) {
    struct stat state;
    int fd;

    if (create && mkdir(READY_DIR, 0700) != 0 && errno != EEXIST) {
        return -1;
    }
    fd = open(READY_DIR, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        return -1;
    }
    if (fstat(fd, &state) != 0 || !S_ISDIR(state.st_mode) || state.st_uid != geteuid() ||
        (state.st_mode & 0777U) != 0700U) {
        int saved = errno != 0 ? errno : EPERM;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

/*
 * Publish readiness as a locked inode, then atomically install that inode at
 * READY_FILE. A health reader therefore observes either the old unlocked inode
 * or the complete new locked inode, never stale contents paired with a new
 * process's lock. A separate singleton lock bounds crash residue to one fixed
 * candidate name and prevents concurrent publishers. Both returned
 * descriptors are intentionally held for the full serve lifetime; Linux
 * releases their locks on every process-exit path, including SIGKILL and
 * RLIMIT_RTTIME termination.
 */
static int publish_ready(const struct control_contract *contract, struct ready_owner_guard *guard) {
    char payload[512];
    struct stat state;
    size_t payload_length;
    int directory_fd;
    int candidate_created = 0;
    int installed = 0;
    int saved;

    guard->singleton_fd = -1;
    guard->ready_fd = -1;
    if (format_ready_payload(contract, payload, sizeof(payload), &payload_length) != 0) {
        return -1;
    }
    directory_fd = open_ready_directory(1);
    if (directory_fd < 0) {
        return -1;
    }
    guard->singleton_fd =
        openat(directory_fd, READY_OWNER_NAME, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (guard->singleton_fd < 0 || fstat(guard->singleton_fd, &state) != 0 ||
        !S_ISREG(state.st_mode) || state.st_uid != geteuid() ||
        (state.st_mode & 0777U) != 0600U || state.st_nlink != 1 ||
        flock(guard->singleton_fd, LOCK_EX | LOCK_NB) != 0) {
        goto fail;
    }
    if (unlinkat(directory_fd, READY_CANDIDATE_NAME, 0) != 0 && errno != ENOENT) {
        goto fail;
    }
    guard->ready_fd = openat(directory_fd, READY_CANDIDATE_NAME,
                             O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (guard->ready_fd < 0) {
        goto fail;
    }
    candidate_created = 1;
    if (fstat(guard->ready_fd, &state) != 0 || !S_ISREG(state.st_mode) ||
        state.st_uid != geteuid() || (state.st_mode & 0777U) != 0600U || state.st_nlink != 1 ||
        flock(guard->ready_fd, LOCK_EX | LOCK_NB) != 0 ||
        write_all(guard->ready_fd, payload, payload_length) != 0 || fsync(guard->ready_fd) != 0 ||
        renameat(directory_fd, READY_CANDIDATE_NAME, directory_fd, "ready") != 0) {
        goto fail;
    }
    candidate_created = 0;
    installed = 1;
    if (fsync(directory_fd) != 0) {
        goto fail;
    }
    close(directory_fd);
    return 0;

fail:
    saved = errno != 0 ? errno : EIO;
    if (!installed && candidate_created) {
        (void)unlinkat(directory_fd, READY_CANDIDATE_NAME, 0);
    }
    if (guard->ready_fd >= 0) {
        close(guard->ready_fd);
        guard->ready_fd = -1;
    }
    if (guard->singleton_fd >= 0) {
        close(guard->singleton_fd);
        guard->singleton_fd = -1;
    }
    close(directory_fd);
    errno = saved;
    return -1;
}

static int local_health(const struct control_contract *contract) {
    char expected[512];
    char observed[512];
    struct stat state;
    size_t expected_length;
    int directory_fd;
    int ready_fd;
    int locked;
    int saved;

    if (format_ready_payload(contract, expected, sizeof(expected), &expected_length) != 0) {
        return 0;
    }
    directory_fd = open_ready_directory(0);
    if (directory_fd < 0) {
        return 0;
    }
    ready_fd = openat(directory_fd, "ready", O_RDWR | O_CLOEXEC | O_NOFOLLOW);
    close(directory_fd);
    if (ready_fd < 0) {
        return 0;
    }
    if (fstat(ready_fd, &state) != 0 || !S_ISREG(state.st_mode) ||
        state.st_uid != geteuid() || (state.st_mode & 0777U) != 0600U || state.st_nlink != 1 ||
        state.st_size != (off_t)expected_length ||
        read_all(ready_fd, observed, expected_length) != 0 ||
        memcmp(observed, expected, expected_length) != 0) {
        close(ready_fd);
        return 0;
    }
    locked = flock(ready_fd, LOCK_EX | LOCK_NB);
    saved = errno;
    if (locked == 0) {
        (void)flock(ready_fd, LOCK_UN);
    }
    close(ready_fd);
    return locked != 0 && (saved == EWOULDBLOCK || saved == EAGAIN);
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

static int hex_nibble(char value, unsigned char *decoded) {
    if (value >= '0' && value <= '9') {
        *decoded = (unsigned char)(value - '0');
        return 0;
    }
    if (value >= 'a' && value <= 'f') {
        *decoded = (unsigned char)(value - 'a' + 10);
        return 0;
    }
    if (value >= 'A' && value <= 'F') {
        *decoded = (unsigned char)(value - 'A' + 10);
        return 0;
    }
    errno = EINVAL;
    return -1;
}

static int decode_hex_exact(const char *encoded, size_t encoded_length, unsigned char *decoded,
                            size_t decoded_length) {
    size_t index;

    if (encoded_length != decoded_length * 2U) {
        errno = EINVAL;
        return -1;
    }
    for (index = 0; index < decoded_length; index++) {
        unsigned char high;
        unsigned char low;
        if (hex_nibble(encoded[index * 2U], &high) != 0 ||
            hex_nibble(encoded[index * 2U + 1U], &low) != 0) {
            return -1;
        }
        decoded[index] = (unsigned char)((high << 4U) | low);
    }
    return 0;
}

static void encode_hex(const unsigned char *decoded, size_t decoded_length, char *encoded) {
    static const char hex[] = "0123456789abcdef";
    size_t index;

    for (index = 0; index < decoded_length; index++) {
        encoded[index * 2U] = hex[decoded[index] >> 4U];
        encoded[index * 2U + 1U] = hex[decoded[index] & 0x0fU];
    }
    encoded[decoded_length * 2U] = '\0';
}

static int read_control_secret(unsigned char secret[CONTROL_SECRET_BYTES]) {
    char encoded[CONTROL_SECRET_HEX_BYTES];
    unsigned char extra;
    int fd;
    int result = -1;
    ssize_t bytes;
    size_t index;

    /* qemu_fw_cfg's `raw` file is mode 0400 in the kernel. Requiring root
     * here keeps the per-launch secret out of ordinary processes sharing the
     * DVM CID; privileged guest code remains explicitly inside the DVM TCB. */
    if (geteuid() != 0) {
        errno = EPERM;
        return -1;
    }
    fd = open(CONTROL_SECRET_FW_CFG, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    if (read_all(fd, encoded, sizeof(encoded)) != 0) {
        goto out;
    }
    do {
        bytes = read(fd, &extra, sizeof(extra));
    } while (bytes < 0 && errno == EINTR);
    if (bytes != 0 ||
        decode_hex_exact(encoded, sizeof(encoded), secret, CONTROL_SECRET_BYTES) != 0) {
        errno = EINVAL;
        goto out;
    }
    for (index = 0; index < CONTROL_SECRET_BYTES; index++) {
        if (secret[index] != 0) {
            result = 0;
            break;
        }
    }
    if (result != 0) {
        errno = EINVAL;
    }
out:
    if (close(fd) != 0 && result == 0) {
        result = -1;
    }
    if (result != 0) {
        memset(secret, 0, CONTROL_SECRET_BYTES);
    }
    return result;
}

static int hmac_sha256(const unsigned char secret[CONTROL_SECRET_BYTES], const unsigned char *message,
                       size_t message_length, unsigned char digest[CONTROL_SECRET_BYTES]) {
    struct sockaddr_alg address;
    int algorithm_fd = -1;
    int operation_fd = -1;
    int result = -1;

    memset(&address, 0, sizeof(address));
    address.salg_family = AF_ALG;
    memcpy(address.salg_type, "hash", sizeof("hash"));
    memcpy(address.salg_name, "hmac(sha256)", sizeof("hmac(sha256)"));
    algorithm_fd = socket(AF_ALG, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (algorithm_fd < 0 ||
        bind(algorithm_fd, (const struct sockaddr *)&address, sizeof(address)) != 0 ||
        setsockopt(algorithm_fd, SOL_ALG, ALG_SET_KEY, secret, CONTROL_SECRET_BYTES) != 0) {
        goto out;
    }
    operation_fd = accept4(algorithm_fd, NULL, NULL, SOCK_CLOEXEC);
    if (operation_fd < 0 || write_all(operation_fd, message, message_length) != 0 ||
        read_all(operation_fd, digest, CONTROL_SECRET_BYTES) != 0) {
        goto out;
    }
    result = 0;
out:
    if (operation_fd >= 0) {
        close(operation_fd);
    }
    if (algorithm_fd >= 0) {
        close(algorithm_fd);
    }
    return result;
}

static int parse_challenge(const char *payload, unsigned char nonce[CONTROL_SECRET_BYTES]) {
    static const char prefix[] = "CHALLENGE\nnonce=";
    size_t length = strlen(payload);

    if (length != sizeof(prefix) - 1U + CONTROL_SECRET_HEX_BYTES ||
        memcmp(payload, prefix, sizeof(prefix) - 1U) != 0) {
        errno = EPROTO;
        return -1;
    }
    return decode_hex_exact(payload + sizeof(prefix) - 1U, CONTROL_SECRET_HEX_BYTES, nonce,
                            CONTROL_SECRET_BYTES);
}

static int make_control_proof(const unsigned char secret[CONTROL_SECRET_BYTES],
                              const unsigned char nonce[CONTROL_SECRET_BYTES], const char *hello,
                              char proof[sizeof("PROOF\nmac=") + CONTROL_SECRET_HEX_BYTES]) {
    unsigned char transcript[CONTROL_PROOF_CONTEXT_BYTES + CONTROL_SECRET_BYTES + MAX_FRAME];
    unsigned char digest[CONTROL_SECRET_BYTES];
    size_t hello_length = strlen(hello);

    if (hello_length > MAX_FRAME) {
        errno = EMSGSIZE;
        return -1;
    }
    memcpy(transcript, CONTROL_PROOF_CONTEXT, CONTROL_PROOF_CONTEXT_BYTES);
    memcpy(transcript + CONTROL_PROOF_CONTEXT_BYTES, nonce, CONTROL_SECRET_BYTES);
    memcpy(transcript + CONTROL_PROOF_CONTEXT_BYTES + CONTROL_SECRET_BYTES, hello, hello_length);
    if (hmac_sha256(secret, transcript,
                    CONTROL_PROOF_CONTEXT_BYTES + CONTROL_SECRET_BYTES + hello_length, digest) != 0) {
        return -1;
    }
    memcpy(proof, "PROOF\nmac=", sizeof("PROOF\nmac=") - 1U);
    encode_hex(digest, sizeof(digest), proof + sizeof("PROOF\nmac=") - 1U);
    memset(digest, 0, sizeof(digest));
    memset(transcript, 0, sizeof(transcript));
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

static int display_driver_name(char driver_name[64]) {
    char target[PATH_MAX];
    const char *driver;
    ssize_t length = readlink("/sys/class/drm/card0/device/driver", target,
                              sizeof(target) - 1U);
    if (length <= 0 || (size_t)length >= sizeof(target))
        return 0;
    target[length] = '\0';
    driver = strrchr(target, '/');
    driver = driver == NULL ? target : driver + 1;
    if (driver[0] == '\0' || strlen(driver) >= 64U)
        return 0;
    memcpy(driver_name, driver, strlen(driver) + 1U);
    return 1;
}

static int supported_display_driver_is_bound(void) {
    char driver[64];
    if (!display_driver_name(driver))
        return 0;
    return strcmp(driver, "virtio_gpu") == 0 || strcmp(driver, "i915") == 0 ||
           strcmp(driver, "xe") == 0 || strcmp(driver, "amdgpu") == 0 ||
           strcmp(driver, "nvidia") == 0;
}

static int display_relay_is_ready(void) {
    static const char staged_expected[] =
        "DISPLAY_RELAY_SCHEMA=2\n"
        "STATE=ready\n"
        "MODE=gpu-compositor-staged-copy\n"
        "ZERO_COPY=0\n"
        "GPU_COMPOSITION=1\n"
        "EXPLICIT_FENCE=1\n";
    static const char amdgpu_expected[] =
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
    char driver[64];
    char state[512];
    const char *expected;
    size_t expected_length;
    int fd = open(DISPLAY_READY_LOCK, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    ssize_t length;
    int locked;
    int saved;
    if (fd < 0)
        return 0;
    if (!display_driver_name(driver)) {
        close(fd);
        return 0;
    }
    if (strcmp(driver, "amdgpu") == 0) {
        expected = amdgpu_expected;
        expected_length = sizeof(amdgpu_expected) - 1U;
    } else {
        expected = staged_expected;
        expected_length = sizeof(staged_expected) - 1U;
    }
    length = read(fd, state, sizeof(state));
    if (length != (ssize_t)expected_length || memcmp(state, expected, expected_length) != 0) {
        close(fd);
        return 0;
    }
    locked = flock(fd, LOCK_EX | LOCK_NB);
    saved = errno;
    if (locked == 0)
        (void)flock(fd, LOCK_UN);
    close(fd);
    return locked != 0 && (saved == EWOULDBLOCK || saved == EAGAIN);
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
    return input_has_capability(fd, EV_KEY, BTN_LEFT) &&
           ((input_has_capability(fd, EV_REL, REL_X) &&
             input_has_capability(fd, EV_REL, REL_Y)) ||
            (input_has_capability(fd, EV_ABS, ABS_X) &&
             input_has_capability(fd, EV_ABS, ABS_Y)));
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

static int scale_absolute_axis(int value, int minimum, int maximum, int extent) {
    int64_t range;
    int64_t offset;
    if (maximum <= minimum || extent <= 1) {
        return -1;
    }
    range = (int64_t)maximum - (int64_t)minimum;
    offset = (int64_t)value - (int64_t)minimum;
    if (offset < 0) {
        offset = 0;
    }
    if (offset > range) {
        offset = range;
    }
    return (int)((offset * (int64_t)(extent - 1) + range / 2) / range);
}

static int pointer_state_configure(struct pointer_state *state, int pointer_fd) {
    struct input_absinfo abs_x;
    struct input_absinfo abs_y;
    if (input_has_capability(pointer_fd, EV_REL, REL_X) &&
        input_has_capability(pointer_fd, EV_REL, REL_Y)) {
        return 0;
    }
    if (!input_has_capability(pointer_fd, EV_ABS, ABS_X) ||
        !input_has_capability(pointer_fd, EV_ABS, ABS_Y) ||
        ioctl(pointer_fd, EVIOCGABS(ABS_X), &abs_x) < 0 ||
        ioctl(pointer_fd, EVIOCGABS(ABS_Y), &abs_y) < 0 ||
        abs_x.maximum <= abs_x.minimum || abs_y.maximum <= abs_y.minimum) {
        errno = EINVAL;
        return -1;
    }
    state->absolute_mode = 1;
    state->absolute_min_x = abs_x.minimum;
    state->absolute_max_x = abs_x.maximum;
    state->absolute_min_y = abs_y.minimum;
    state->absolute_max_y = abs_y.maximum;
    return 0;
}

static int monotonic_time_ns(uint64_t *out) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 || now.tv_nsec < 0) {
        return -1;
    }
    *out = ((uint64_t)now.tv_sec * 1000000000ULL) + (uint64_t)now.tv_nsec;
    return 0;
}

static int read_exact_small_file(const char *path, char *buffer, size_t buffer_size,
                                 int require_private_root) {
    struct stat metadata;
    ssize_t total = 0;
    int fd;
    if (path == NULL || buffer == NULL || buffer_size < 2U) {
        errno = EINVAL;
        return -1;
    }
    fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0)
        return -1;
    if (fstat(fd, &metadata) != 0 || !S_ISREG(metadata.st_mode) ||
        (require_private_root && (metadata.st_uid != 0 || (metadata.st_mode & 0077) != 0))) {
        int saved = errno == 0 ? EPERM : errno;
        close(fd);
        errno = saved;
        return -1;
    }
    while ((size_t)total < buffer_size - 1U) {
        ssize_t bytes = read(fd, buffer + total, buffer_size - 1U - (size_t)total);
        if (bytes < 0) {
            if (errno == EINTR)
                continue;
            close(fd);
            return -1;
        }
        if (bytes == 0)
            break;
        total += bytes;
    }
    if ((size_t)total == buffer_size - 1U) {
        char extra;
        ssize_t bytes = read(fd, &extra, 1U);
        if (bytes != 0) {
            int saved = bytes < 0 ? errno : EOVERFLOW;
            close(fd);
            errno = saved;
            return -1;
        }
    }
    if (close(fd) != 0)
        return -1;
    if (total <= 0) {
        errno = EPROTO;
        return -1;
    }
    buffer[total] = '\0';
    return (int)total;
}

static int display_pci_hex_id(const char *attribute, char value[5]) {
    char path[PATH_MAX];
    char text[16];
    int length;
    size_t index;
    if (snprintf(path, sizeof(path), "/sys/class/drm/card0/device/%s", attribute) >=
        (int)sizeof(path)) {
        errno = EOVERFLOW;
        return -1;
    }
    length = read_exact_small_file(path, text, sizeof(text), 0);
    if (!((length == 6) || (length == 7 && text[6] == '\n')) || text[0] != '0' ||
        text[1] != 'x') {
        errno = EPROTO;
        return -1;
    }
    for (index = 0U; index < 4U; index++) {
        char byte = text[index + 2U];
        if (!((byte >= '0' && byte <= '9') || (byte >= 'a' && byte <= 'f'))) {
            errno = EPROTO;
            return -1;
        }
        value[index] = byte;
    }
    value[4] = '\0';
    return 0;
}

static int display_guest_pci_bdf(char bdf[16]) {
    char target[PATH_MAX];
    const char *name;
    ssize_t length = readlink("/sys/class/drm/card0/device", target, sizeof(target) - 1U);
    unsigned int domain;
    unsigned int bus;
    unsigned int device;
    unsigned int function;
    char extra;
    if (length <= 0 || (size_t)length >= sizeof(target))
        return -1;
    target[length] = '\0';
    name = strrchr(target, '/');
    name = name == NULL ? target : name + 1;
    if (strlen(name) != 12U ||
        sscanf(name, "%4x:%2x:%2x.%1x%c", &domain, &bus, &device, &function, &extra) != 4 ||
        domain > 0xffffU || bus > 0xffU || device > 0x1fU || function > 7U) {
        errno = EPROTO;
        return -1;
    }
    memcpy(bdf, name, 13U);
    return 0;
}

static int read_display_evidence(struct display_evidence_sample *sample, uint64_t *age_ms,
                                 char driver[64], char vendor[5], char device[5], char bdf[16]) {
    char state[1024];
    char source_path[12];
    char zero_copy[4];
    char gpu_composition[4];
    char explicit_fence[4];
    char atomic_kms_scanout[4];
    char staged_damage_copy[4];
    uint32_t scanout_buffers;
    uint64_t now;
    int length;
    int consumed = 0;
    if (sample == NULL || age_ms == NULL || !display_relay_is_ready() ||
        !display_driver_name(driver) || display_pci_hex_id("vendor", vendor) != 0 ||
        display_pci_hex_id("device", device) != 0 || display_guest_pci_bdf(bdf) != 0) {
        return -1;
    }
    length = read_exact_small_file(DISPLAY_EVIDENCE_FILE, state, sizeof(state), 1);
    if (length <= 0)
        return -1;
    memset(sample, 0, sizeof(*sample));
    if (sscanf(
            state,
            "DISPLAY_EVIDENCE_SCHEMA=2\nSAMPLE_SEQUENCE=%" SCNu64
            "\nSAMPLE_MONOTONIC_NS=%" SCNu64 "\nWINDOW_NS=%" SCNu64
            "\nPAGEFLIP_COMPLETIONS=%" SCNu64 "\nFRAME_HZ_MILLI=%" SCNu64
            "\nCPU_COPY_US_AVG=%" SCNu64 "\nPAGEFLIP_LATENCY_US_AVG=%" SCNu64
            "\nPAGEFLIP_LATENCY_US_MAX=%" SCNu64 "\nATOMIC_COMMIT_US_AVG=%" SCNu64
            "\nCONNECTOR_ID=%" SCNu32 "\nMODE_WIDTH=%" SCNu32 "\nMODE_HEIGHT=%" SCNu32
            "\nSOURCE_PATH=%11s\nZERO_COPY=%3s\nGPU_COMPOSITION=%3s"
            "\nEXPLICIT_FENCE=%3s\nATOMIC_KMS_SCANOUT=%3s\nSCANOUT_BUFFERS=%" SCNu32
            "\nSTAGED_DAMAGE_COPY=%3s\n%n",
            &sample->sequence, &sample->monotonic_ns, &sample->window_ns,
            &sample->pageflip_completions, &sample->frame_hz_milli,
            &sample->cpu_copy_us_avg, &sample->pageflip_latency_us_avg,
            &sample->pageflip_latency_us_max, &sample->atomic_commit_us_avg,
            &sample->connector_id, &sample->mode_width, &sample->mode_height,
            source_path, zero_copy, gpu_composition, explicit_fence,
            atomic_kms_scanout, &scanout_buffers, staged_damage_copy, &consumed) != 19 ||
        consumed != length || strcmp(source_path, "dmabuf") != 0 ||
        strcmp(zero_copy, "yes") != 0 || strcmp(gpu_composition, "yes") != 0 ||
        strcmp(explicit_fence, "yes") != 0 || strcmp(atomic_kms_scanout, "yes") != 0 ||
        scanout_buffers != 3U || strcmp(staged_damage_copy, "no") != 0 ||
        sample->sequence == 0U ||
        sample->monotonic_ns == 0U || monotonic_time_ns(&now) != 0 ||
        now < sample->monotonic_ns) {
        errno = EPROTO;
        return -1;
    }
    *age_ms = (now - sample->monotonic_ns) / 1000000ULL;
    return 0;
}

static void pointer_mark_pending(struct pointer_state *state) {
    uint64_t now;
    if (!state->pending && monotonic_time_ns(&now) == 0) {
        state->flush_deadline_ns = now + INPUT_POINTER_FLUSH_NS;
    }
    state->pending = 1;
}

static int pointer_flush_timeout_ms(const struct pointer_state *state) {
    uint64_t now;
    uint64_t remaining_ns;
    uint64_t remaining_ms;
    if (!state->pending || state->buttons_changed) {
        return state->pending ? 0 : -1;
    }
    if (state->flush_deadline_ns == 0 || monotonic_time_ns(&now) != 0 ||
        now >= state->flush_deadline_ns) {
        return 0;
    }
    remaining_ns = state->flush_deadline_ns - now;
    remaining_ms = (remaining_ns + 999999ULL) / 1000000ULL;
    return remaining_ms > (uint64_t)INT_MAX ? INT_MAX : (int)remaining_ms;
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
    if (state->absolute_mode) {
        if (!state->absolute_initialized ||
            snprintf(payload, sizeof(payload),
                     "EVENT\nid=%u\nop=input-stream\ntype=pointer-position\nx=%d\ny=%d"
                     "\nwheel-v=%d\nwheel-h=%d\nbuttons=%u",
                     request_id, state->absolute_x, state->absolute_y,
                     state->wheel_vertical, state->wheel_horizontal,
                     state->buttons) >= (int)sizeof(payload)) {
            errno = EMSGSIZE;
            return -1;
        }
    } else if (snprintf(payload, sizeof(payload),
                        "EVENT\nid=%u\nop=input-stream\ntype=pointer\ndx=%d\ndy=%d"
                        "\nwheel-v=%d\nwheel-h=%d\nbuttons=%u",
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
    state->buttons_changed = 0;
    state->flush_deadline_ns = 0;
    if (state->absolute_mode) {
        state->absolute_previous_x = state->absolute_x;
        state->absolute_previous_y = state->absolute_y;
        state->absolute_published = 1;
    }
    return 0;
}

static int consume_pointer_event(struct pointer_state *state, const struct input_event *event) {
    uint8_t mask;
    if (event->type == EV_REL) {
        switch (event->code) {
        case REL_X:
            if (state->absolute_mode) {
                return 0;
            }
            state->dx = add_clamped_i16(state->dx, event->value);
            break;
        case REL_Y:
            if (state->absolute_mode) {
                return 0;
            }
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
        if (!state->absolute_mode) {
            pointer_mark_pending(state);
        }
        return 0;
    }
    if (state->absolute_mode && event->type == EV_ABS) {
        int scaled;
        if (event->code == ABS_X) {
            scaled = scale_absolute_axis(event->value, state->absolute_min_x,
                                         state->absolute_max_x, DVM_POINTER_WIDTH);
            if (scaled < 0) {
                errno = EINVAL;
                return -1;
            }
            state->absolute_x = scaled;
            state->absolute_seen_x = 1;
        } else if (event->code == ABS_Y) {
            scaled = scale_absolute_axis(event->value, state->absolute_min_y,
                                         state->absolute_max_y, DVM_POINTER_HEIGHT);
            if (scaled < 0) {
                errno = EINVAL;
                return -1;
            }
            state->absolute_y = scaled;
            state->absolute_seen_y = 1;
        } else {
            return 0;
        }
        state->absolute_initialized = state->absolute_seen_x && state->absolute_seen_y;
        return 0;
    }
    if (event->type == EV_KEY && pointer_button_mask(event->code, &mask) == 0) {
        if (event->value != 0) {
            state->buttons |= mask;
        } else {
            state->buttons &= (uint8_t)~mask;
        }
        state->buttons &= POINTER_BUTTON_MASK;
        if (!state->absolute_mode) {
            pointer_mark_pending(state);
        }
        state->buttons_changed = 1;
        return 0;
    }
    if (event->type == EV_SYN && event->code == SYN_REPORT) {
        if (state->absolute_mode && state->absolute_initialized &&
            (!state->absolute_published ||
             state->absolute_x != state->absolute_previous_x ||
             state->absolute_y != state->absolute_previous_y ||
             state->wheel_vertical != 0 || state->wheel_horizontal != 0 ||
             state->buttons_changed)) {
            pointer_mark_pending(state);
        }
        return 0;
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
    if (pointer_state_configure(&pointer, pointer_fd) != 0) {
        return -1;
    }
    pollfds[0].fd = keyboard_fd;
    pollfds[0].events = POLLIN;
    pollfds[1].fd = pointer_fd;
    pollfds[1].events = POLLIN;
    for (;;) {
        int ready;
        int timeout_ms;
        int selftest_timeout_ms;
        unsigned int index;
        pollfds[0].revents = 0;
        pollfds[1].revents = 0;
        timeout_ms = pointer_flush_timeout_ms(&pointer);
        selftest_timeout_ms = input_selftest_timeout_ms(selftest);
        if (selftest_timeout_ms >= 0 &&
            (timeout_ms < 0 || timeout_ms > selftest_timeout_ms)) {
            timeout_ms = selftest_timeout_ms;
        }
        ready = poll(pollfds, 2, timeout_ms);
        if (ready < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        /* Cadence is independent of poll idleness: unrelated keyboard,
         * pointer, or control wakeups cannot starve the proof stream. */
        if (input_selftest_emit_cycle(selftest) != 0) {
            return -1;
        }
        if (ready == 0) {
            if (pointer.pending && pointer_flush_timeout_ms(&pointer) == 0) {
                if (send_pointer_packet(control_fd, request_id, &pointer) != 0) {
                    return -1;
                }
                continue;
            }
            continue;
        }
        for (index = 0; index < 2; index++) {
            struct input_event event;
            uint8_t pointer_mask;
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
                /* A composite evdev node is opened once for keyboard and once
                 * for pointer state. Pointer buttons must travel only through
                 * the pointer packet: duplicating them as keys both changes
                 * the RustOS input ABI and can exceed L0's bounded frame rate.
                 */
                if (event.type == EV_KEY &&
                    pointer_button_mask(event.code, &pointer_mask) != 0 &&
                    send_keyboard_event(control_fd, request_id, &event) != 0) {
                    return -1;
                }
            } else {
                if (consume_pointer_event(&pointer, &event) != 0) {
                    return -1;
                }
                if (pointer.buttons_changed &&
                    (!pointer.absolute_mode ||
                     (event.type == EV_SYN && event.code == SYN_REPORT)) &&
                    send_pointer_packet(control_fd, request_id, &pointer) != 0) {
                    return -1;
                }
            }
        }
    }
}

static int serve_connection(int fd, const struct control_contract *contract,
                            struct input_selftest *selftest,
                            const unsigned char secret[CONTROL_SECRET_BYTES]) {
    char payload[MAX_FRAME + 1];
    char hello[MAX_FRAME + 1];
    char welcome[192];
    char proof[sizeof("PROOF\nmac=") + CONTROL_SECRET_HEX_BYTES];
    unsigned char nonce[CONTROL_SECRET_BYTES];
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
        parse_challenge(payload, nonce) != 0 || make_control_proof(secret, nonce, hello, proof) != 0 ||
        send_frame(fd, proof) != 0 || receive_frame(fd, payload, sizeof(payload)) != 0 ||
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
            const char *display_driver =
                supported_display_driver_is_bound() ? "bound" : "missing";
            const char *display_relay = display_relay_is_ready() ? "ready" : "missing";
            snprintf(payload, sizeof(payload),
                     "RESPONSE\nid=%u\nop=driver-inventory\nstatus=ok\nvirtio-net=%s\n"
                     "virtio-gpu=%s\ndisplay-driver=%s\ndisplay-relay=%s",
                     id, virtio_net, virtio_gpu, display_driver, display_relay);
        } else if (request_id(payload, "display-evidence-v2", &id) == 0) {
            struct display_evidence_sample evidence;
            uint64_t age_ms;
            char driver[64];
            char vendor[5];
            char device[5];
            char bdf[16];
            if (read_display_evidence(&evidence, &age_ms, driver, vendor, device, bdf) == 0) {
                snprintf(
                    payload, sizeof(payload),
                    "RESPONSE\nid=%u\nop=display-evidence-v2\nstatus=ok\n"
                    "sample-sequence=%" PRIu64 "\nsample-age-ms=%" PRIu64
                    "\ndriver=%s\npci-vendor=%s\npci-device=%s\nguest-pci-bdf=%s\n"
                    "connector-id=%" PRIu32 "\nmode-width=%" PRIu32
                    "\nmode-height=%" PRIu32
                    "\nsource-path=dmabuf\nzero-copy=yes\ngpu-composition=yes"
                    "\nexplicit-fence=yes\natomic-kms-scanout=yes\nscanout-buffers=3"
                    "\nstaged-damage-copy=no\nwindow-ns=%" PRIu64
                    "\nframe-hz-milli=%" PRIu64 "\npageflip-completions=%" PRIu64
                    "\ncpu-copy-us-avg=%" PRIu64 "\npageflip-latency-us-avg=%" PRIu64
                    "\npageflip-latency-us-max=%" PRIu64 "\natomic-commit-us-avg=%" PRIu64,
                    id, evidence.sequence, age_ms, driver, vendor, device, bdf,
                    evidence.connector_id, evidence.mode_width, evidence.mode_height,
                    evidence.window_ns, evidence.frame_hz_milli,
                    evidence.pageflip_completions, evidence.cpu_copy_us_avg,
                    evidence.pageflip_latency_us_avg, evidence.pageflip_latency_us_max,
                    evidence.atomic_commit_us_avg);
            } else {
                snprintf(
                    payload, sizeof(payload),
                    "RESPONSE\nid=%u\nop=display-evidence-v2\nstatus=unavailable\n"
                    "sample-sequence=0\nsample-age-ms=0\ndriver=missing\npci-vendor=0000\n"
                    "pci-device=0000\nguest-pci-bdf=none\nconnector-id=0\nmode-width=0\n"
                    "mode-height=0\nsource-path=none\nzero-copy=no\ngpu-composition=no\n"
                    "explicit-fence=no\natomic-kms-scanout=no\nscanout-buffers=0\n"
                    "staged-damage-copy=no\nwindow-ns=0\nframe-hz-milli=0\n"
                    "pageflip-completions=0\ncpu-copy-us-avg=0\npageflip-latency-us-avg=0\n"
                    "pageflip-latency-us-max=0\natomic-commit-us-avg=0",
                    id);
            }
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
                struct input_scheduler_guard scheduler;
                if (input_scheduler_enter(&scheduler) != 0) {
                    if (scheduler.fatal)
                        die("input scheduler admission rollback failed");
                    close(keyboard_fd);
                    close(pointer_fd);
                    snprintf(payload, sizeof(payload),
                             "RESPONSE\nid=%u\nop=input-stream\nstatus=error"
                             "\nreason=scheduler-unavailable",
                             id);
                } else {
                    int stream_errno;
                    snprintf(payload, sizeof(payload),
                             "RESPONSE\nid=%u\nop=input-stream\nstatus=ready\nformat=linux-evdev-v3"
                             "\nkeyboard=event%d\npointer=event%d",
                             id, keyboard_index, pointer_index);
                    if (send_frame(fd, payload) != 0) {
                        if (input_scheduler_leave(&scheduler) != 0)
                            die("input scheduler restore failed");
                        close(keyboard_fd);
                        close(pointer_fd);
                        return -1;
                    }
                    if (selftest->enabled) {
                        selftest->armed = 1;
                        selftest->cycles_remaining = INPUT_SELFTEST_CYCLES;
                        selftest->next_emit_ns = 0U;
                        /* Both evdev file descriptions are open now. Queue the
                         * first cycle before entering poll(2), so readiness does
                         * not depend on a timeout winning over unrelated input
                         * wakeups during concurrent guest startup. Subsequent
                         * cycles remain rate-limited by the normal poll loop. */
                        if (input_selftest_emit_cycle(selftest) != 0) {
                            if (input_scheduler_leave(&scheduler) != 0)
                                die("input scheduler restore failed");
                            close(keyboard_fd);
                            close(pointer_fd);
                            return -1;
                        }
                        fprintf(stderr, "rustos-dvm-agent: input selftest stream armed\n");
                        fflush(stderr);
                    }
                    if (stream_input_devices(fd, keyboard_fd, pointer_fd, id, selftest) == 0) {
                        errno = EIO;
                    }
                    stream_errno = errno;
                    if (input_scheduler_leave(&scheduler) != 0) {
                        die("input scheduler restore failed");
                    }
                    close(keyboard_fd);
                    close(pointer_fd);
                    errno = stream_errno;
                    return -1;
                }
            }
        } else {
            return -1;
        }
        if (send_frame(fd, payload) != 0) {
            return -1;
        }
    }
}

static uint32_t control_port_from_secret(const unsigned char secret[CONTROL_SECRET_BYTES]) {
    uint32_t entropy = ((uint32_t)secret[0] << 24U) | ((uint32_t)secret[1] << 16U) |
                       ((uint32_t)secret[2] << 8U) | (uint32_t)secret[3];

    /* This must match ControlSecret::control_port() in the L0 broker.  The
     * root-only fw_cfg secret therefore also hides the per-launch vsock endpoint
     * from ordinary processes that share this DVM CID. */
    return CONTROL_PORT_FLOOR + entropy % CONTROL_PORT_SPAN;
}

static int connect_host(uint32_t control_port) {
    int fd;
    struct sockaddr_vm address = {
        .svm_family = AF_VSOCK,
        .svm_port = control_port,
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
    struct ready_owner_guard ready_owner;
    unsigned char secret[CONTROL_SECRET_BYTES];
    uint32_t control_port;

    if (read_control_secret(secret) != 0) {
        die("load owner-provisioned DVM control secret failed");
    }
    control_port = control_port_from_secret(secret);
    if (prctl(PR_SET_DUMPABLE, 0) != 0) {
        die("disable DVM control agent dumpability failed");
    }
    if (input_selftest_start(&selftest) != 0) {
        die("input selftest requested but uinput setup failed");
    }
    if (publish_ready(contract, &ready_owner) != 0) {
        die("publish process-owned readiness failed");
    }
    fprintf(stderr, "rustos-dvm-agent: ready protocol=%s state=%s\n", contract->protocol,
            contract->state);
    fflush(stderr);
    for (;;) {
        int fd = connect_host(control_port);
        if (fd >= 0) {
            if (serve_connection(fd, contract, &selftest, secret) != 0) {
                fprintf(stderr, "rustos-dvm-agent: host control disconnected\n");
            }
            close(fd);
        }
        sleep(1);
    }
    close(ready_owner.ready_fd);
    close(ready_owner.singleton_fd);
    input_selftest_destroy(&selftest);
}

int main(int argc, char **argv) {
    struct control_contract contract;
    parse_contract(&contract);
    if (argc == 1 || strcmp(argv[1], "announce") == 0) {
        printf("rustos-dvm-agent: contract protocol=%s state=%s\n", contract.protocol,
               contract.state);
        return EXIT_SUCCESS;
    }
    if (strcmp(argv[1], "health") == 0) {
        return local_health(&contract) ? EXIT_SUCCESS : EXIT_FAILURE;
    }
    if (strcmp(argv[1], "serve") == 0) {
        serve(&contract);
        return EXIT_SUCCESS;
    }
    fprintf(stderr, "usage: %s {announce|health|serve}\n", argv[0]);
    return EXIT_FAILURE;
}
