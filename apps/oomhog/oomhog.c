#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

// Memory-pressure workload for the KVM OOM end-to-end proof.
//
// Every session-startup program is launched in every boot that reaches
// ui-ready, so this one runs only when `cargo xtask kvm-smoke --oom-hog` has
// written the private contract below. Without it the program exits at once and
// silently: a production or ordinary smoke boot must never start a workload
// that consumes all of memory.
//
// Several workers touch anonymous memory until the physical allocator refuses
// them at its kernel reserve. A refused first touch retires only the faulting
// thread, so the main thread, which holds the process open, outlives the
// workers while the process still maps most of RAM. That is the situation the
// root supervisor's memory governor exists for: it must pick this process, the
// largest one outside every service lease, and terminate it. The main thread
// therefore never allocates and never exits on its own while memory is short;
// if it reaches the survival deadline the governor failed and the marker below
// says so.

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define CONTRACT_PATH "/system/registry/system/oomhog-v1.env"
#define CONTRACT_BODY "contract=rustos-oomhog-v1\n"
#define WORKERS 4
#define CHUNK_BYTES (8UL * 1024UL * 1024UL)
#define PAGE_BYTES 4096UL
#define START_DELAY_MS 3000UL
#define SURVIVAL_DEADLINE_MS 20000UL
#define REPORT_PERIOD_MS 1000UL
#define POLL_MS 50UL

static volatile uint64_t touched_pages;

static void debug_line(const char *message) {
    size_t len = strlen(message);
    if (len != 0) {
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, message, len);
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static void debug_counter(const char *prefix, uint64_t a, uint64_t b) {
    char message[128];
    int written = snprintf(message, sizeof(message), "%s %llu %llu", prefix,
                           (unsigned long long)a, (unsigned long long)b);
    if (written > 0 && (size_t)written < sizeof(message)) {
        debug_line(message);
    }
}

static void *worker(void *unused) {
    (void)unused;
    for (;;) {
        volatile char *chunk = mmap(NULL, CHUNK_BYTES, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (chunk == MAP_FAILED) {
            debug_counter("oomhog: mmap refused errno", (uint64_t)errno, 0);
            return NULL;
        }
        for (size_t offset = 0; offset < CHUNK_BYTES; offset += PAGE_BYTES) {
            chunk[offset] = 1;
            __atomic_fetch_add(&touched_pages, 1, __ATOMIC_RELAXED);
        }
    }
}

static uint64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000UL + (uint64_t)now.tv_nsec / 1000000UL;
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
    // Let the desktop and the driver-domain relay finish coming up first: the
    // proof is that pressure kills the hog while the rest of the system keeps
    // running, not that a starved boot fails.
    struct timespec settle = {.tv_sec = (time_t)(START_DELAY_MS / 1000UL), .tv_nsec = 0};
    nanosleep(&settle, NULL);
    debug_counter("oomhog: start pid", (uint64_t)getpid(), 0);
    pthread_t threads[WORKERS];
    for (int index = 0; index < WORKERS; index++) {
        if (pthread_create(&threads[index], NULL, worker, NULL) != 0) {
            debug_counter("oomhog: pthread_create failed index", (uint64_t)index, 0);
            return 1;
        }
    }

    uint64_t started = now_ms();
    uint64_t next_report = started + REPORT_PERIOD_MS;
    for (;;) {
        struct timespec pause = {.tv_sec = 0, .tv_nsec = (long)(POLL_MS * 1000000UL)};
        nanosleep(&pause, NULL);
        uint64_t now = now_ms();
        if (now >= next_report) {
            debug_counter("oomhog: alive touched_mib elapsed_ms",
                          __atomic_load_n(&touched_pages, __ATOMIC_RELAXED) * PAGE_BYTES >> 20,
                          now - started);
            next_report += REPORT_PERIOD_MS;
        }
        if (now - started >= SURVIVAL_DEADLINE_MS) {
            debug_counter("oomhog: survived touched_mib",
                          __atomic_load_n(&touched_pages, __ATOMIC_RELAXED) * PAGE_BYTES >> 20,
                          0);
            return 2;
        }
    }
}
