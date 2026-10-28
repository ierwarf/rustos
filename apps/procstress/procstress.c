#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// Process-lifecycle stress for the KVM panic investigation.
//
// Several threads of one process churn its address space (mmap, touch,
// mprotect, munmap), which makes every CPU running a sibling take TLB
// shootdowns, while other threads fork children that write copy-on-write pages
// and exit. That combination is what the unexplained scheduler and TLB panics
// were seen next to: a forked child's termination, and shootdowns that a CPU
// did not acknowledge.
//
// Session-startup programs are launched in every boot that reaches ui-ready, so
// this runs only when `cargo xtask kvm-smoke --proc-stress` has written the
// private contract below; without it the program exits at once, silently.

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define CONTRACT_PATH "/system/registry/system/procstress-v1.env"
#define CONTRACT_BODY "contract=rustos-procstress-v1\n"
#define START_DELAY_S 3
#define RUN_MS 8000UL
#define MAPPERS 4
#define FORKERS 4
#define MAP_BYTES (256UL * 1024UL)
#define PAGE_BYTES 4096UL
#define SHARED_BYTES (1024UL * 1024UL)

static volatile int stop;
static uint64_t maps_done;
static uint64_t forks_done;
static uint64_t fork_failures;
static char *shared;

static void debug_line(const char *message) {
    size_t len = strlen(message);
    if (len != 0) {
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, message, len);
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static void debug_counters(const char *prefix, uint64_t a, uint64_t b, uint64_t c) {
    char message[160];
    int written = snprintf(message, sizeof(message), "%s %llu %llu %llu", prefix,
                           (unsigned long long)a, (unsigned long long)b, (unsigned long long)c);
    if (written > 0 && (size_t)written < sizeof(message)) {
        debug_line(message);
    }
}

static uint64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000UL + (uint64_t)now.tv_nsec / 1000000UL;
}

static void *mapper(void *unused) {
    (void)unused;
    while (!stop) {
        volatile char *region = mmap(NULL, MAP_BYTES, PROT_READ | PROT_WRITE,
                                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (region == MAP_FAILED) {
            continue;
        }
        for (size_t offset = 0; offset < MAP_BYTES; offset += PAGE_BYTES) {
            region[offset] = 1;
        }
        // Dropping and restoring write permission forces a shootdown each way.
        mprotect((void *)region, MAP_BYTES, PROT_READ);
        mprotect((void *)region, MAP_BYTES, PROT_READ | PROT_WRITE);
        munmap((void *)region, MAP_BYTES);
        __atomic_fetch_add(&maps_done, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

static void *forker(void *unused) {
    (void)unused;
    while (!stop) {
        pid_t child = fork();
        if (child == 0) {
            // Only memory writes and _exit: the parent is multi-threaded.
            for (size_t offset = 0; offset < SHARED_BYTES; offset += 16 * PAGE_BYTES) {
                shared[offset] = 2;
            }
            _exit(7);
        }
        if (child < 0) {
            __atomic_fetch_add(&fork_failures, 1, __ATOMIC_RELAXED);
            struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
            nanosleep(&pause, NULL);
            continue;
        }
        int status = 0;
        waitpid(child, &status, 0);
        __atomic_fetch_add(&forks_done, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

static int contract_present(void) {
    char body[sizeof(CONTRACT_BODY)];
    int fd = open(CONTRACT_PATH, O_RDONLY);
    if (fd < 0) {
        return 0;
    }
    ssize_t got = read(fd, body, sizeof(body));
    close(fd);
    return got == (ssize_t)(sizeof(CONTRACT_BODY) - 1) &&
           memcmp(body, CONTRACT_BODY, sizeof(CONTRACT_BODY) - 1) == 0;
}

int main(void) {
    if (!contract_present()) {
        return 0;
    }
    struct timespec settle = {.tv_sec = START_DELAY_S, .tv_nsec = 0};
    nanosleep(&settle, NULL);

    shared = mmap(NULL, SHARED_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        debug_line("procstress: shared mmap failed");
        return 1;
    }
    memset(shared, 1, SHARED_BYTES);

    debug_counters("procstress: start pid", (uint64_t)getpid(), MAPPERS, FORKERS);
    pthread_t threads[MAPPERS + FORKERS];
    for (int index = 0; index < MAPPERS + FORKERS; index++) {
        void *(*body)(void *) = index < MAPPERS ? mapper : forker;
        if (pthread_create(&threads[index], NULL, body, NULL) != 0) {
            debug_counters("procstress: pthread_create failed", (uint64_t)index, 0, 0);
            stop = 1;
            return 1;
        }
    }
    uint64_t started = now_ms();
    while (now_ms() - started < RUN_MS) {
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 50000000};
        nanosleep(&pause, NULL);
    }
    stop = 1;
    for (int index = 0; index < MAPPERS + FORKERS; index++) {
        pthread_join(threads[index], NULL);
    }
    debug_counters("procstress: done maps forks fork_failures",
                   __atomic_load_n(&maps_done, __ATOMIC_RELAXED),
                   __atomic_load_n(&forks_done, __ATOMIC_RELAXED),
                   __atomic_load_n(&fork_failures, __ATOMIC_RELAXED));
    return 0;
}
