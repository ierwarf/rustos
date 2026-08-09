#define _GNU_SOURCE

#include <cpuid.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* These bounds are deliberately shared with runtimed's private KVM contract. */
#define SMPQUAL_MIN_WORK_UNITS 1ULL
#define SMPQUAL_MAX_WORK_UNITS 10000000ULL
#define SMPQUAL_MIN_DEADLINE_MS 100ULL
#define SMPQUAL_MAX_DEADLINE_MS 30000ULL

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define SYS_RUSTOS_PRODUCT_MILESTONE 0x52550046UL
#define PRODUCT_MILESTONE_SMPQUAL_READY 7UL
#define PRODUCT_MILESTONE_SMPQUAL_START 8UL
#define PRODUCT_MILESTONE_SMPQUAL_FINISH 9UL
#define PRODUCT_MILESTONE_SMPQUAL_COMPLETE 10UL

struct smpqual_config {
    unsigned int workers;
    uint64_t work_units;
    uint64_t deadline_ms;
};

struct smpqual_shared {
    struct smpqual_config config;
    struct timespec deadline;
    _Atomic unsigned int ready_workers;
    _Atomic unsigned int completed_workers;
    _Atomic bool start_gate;
    _Atomic bool finish_gate;
    _Atomic bool failed;
};

struct worker_context {
    struct smpqual_shared *shared;
    unsigned int worker_id;
    volatile uint64_t work_sink;
    uint64_t completed_work_units;
};

static void debug_line(const char *format, ...) {
    char line[192];
    va_list args;
    va_start(args, format);
    int written = vsnprintf(line, sizeof(line), format, args);
    va_end(args);
    if (written <= 0) {
        return;
    }
    size_t length = (size_t)written;
    if (length >= sizeof(line)) {
        length = sizeof(line) - 1;
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, line, length);
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static bool deadline_expired(const struct timespec *deadline) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return true;
    }
    return now.tv_sec > deadline->tv_sec ||
           (now.tv_sec == deadline->tv_sec && now.tv_nsec >= deadline->tv_nsec);
}

static void mark_failed(struct smpqual_shared *shared) {
    atomic_store_explicit(&shared->failed, true, memory_order_release);
    /* Release every bounded barrier so each created thread can be joined. */
    atomic_store_explicit(&shared->start_gate, true, memory_order_release);
    atomic_store_explicit(&shared->finish_gate, true, memory_order_release);
}

static int wait_for_gate(struct smpqual_shared *shared, _Atomic bool *gate) {
    while (!atomic_load_explicit(gate, memory_order_acquire)) {
        if (atomic_load_explicit(&shared->failed, memory_order_acquire)) {
            return -1;
        }
        if (deadline_expired(&shared->deadline)) {
            mark_failed(shared);
            return -1;
        }
        __asm__ volatile("pause" ::: "memory");
    }
    return atomic_load_explicit(&shared->failed, memory_order_acquire) ? -1 : 0;
}

static int emit_milestone(unsigned long phase, unsigned int observed_cpu, unsigned int worker_id,
                          uint64_t work_units) {
    uint64_t arg0 = ((uint64_t)observed_cpu << 32) | worker_id;
    long result = syscall(SYS_RUSTOS_PRODUCT_MILESTONE, phase, arg0, work_units);
    return result < 0 ? -1 : 0;
}

static int parse_u64(const char *text, uint64_t minimum, uint64_t maximum, uint64_t *out) {
    if (text == NULL || text[0] == '\0') {
        return -1;
    }
    uint64_t value = 0;
    for (const unsigned char *cursor = (const unsigned char *)text; *cursor != '\0'; ++cursor) {
        if (*cursor < '0' || *cursor > '9') {
            return -1;
        }
        uint64_t digit = (uint64_t)(*cursor - '0');
        if (value > (UINT64_MAX - digit) / 10) {
            return -1;
        }
        value = value * 10 + digit;
    }
    if (value < minimum || value > maximum) {
        return -1;
    }
    *out = value;
    return 0;
}

static int parse_config(int argc, char **argv, struct smpqual_config *config) {
    if (argc != 7 || strcmp(argv[1], "--workers") != 0 ||
        strcmp(argv[3], "--work-units") != 0 || strcmp(argv[5], "--deadline-ms") != 0) {
        return -1;
    }
    uint64_t workers = 0;
    if (parse_u64(argv[2], 1, 8, &workers) != 0 ||
        (workers != 1 && workers != 2 && workers != 4 && workers != 8) ||
        parse_u64(argv[4], SMPQUAL_MIN_WORK_UNITS, SMPQUAL_MAX_WORK_UNITS,
                  &config->work_units) != 0 ||
        parse_u64(argv[6], SMPQUAL_MIN_DEADLINE_MS, SMPQUAL_MAX_DEADLINE_MS,
                  &config->deadline_ms) != 0) {
        return -1;
    }
    config->workers = (unsigned int)workers;
    return 0;
}

static int set_and_verify_worker_affinity(unsigned int worker_id) {
    cpu_set_t requested;
    cpu_set_t observed;
    CPU_ZERO(&requested);
    CPU_SET((int)worker_id, &requested);
    if (pthread_setaffinity_np(pthread_self(), sizeof(requested), &requested) != 0) {
        return -1;
    }
    CPU_ZERO(&observed);
    if (pthread_getaffinity_np(pthread_self(), sizeof(observed), &observed) != 0) {
        return -1;
    }
    return CPU_COUNT(&observed) == 1 && CPU_ISSET((int)worker_id, &observed) ? 0 : -1;
}

static int observed_logical_cpu(unsigned int *cpu) {
    unsigned int low;
    unsigned int high;
    unsigned int aux;
    __asm__ volatile("rdtscp" : "=a"(low), "=d"(high), "=c"(aux) : : "memory");
    (void)low;
    (void)high;
    if (aux == 0) {
        return -1;
    }
    *cpu = aux - 1;
    return 0;
}

static bool rdtscp_is_supported(void) {
    unsigned int maximum = __get_cpuid_max(0x80000000U, NULL);
    unsigned int eax;
    unsigned int ebx;
    unsigned int ecx;
    unsigned int edx;
    return maximum >= 0x80000001U &&
           __get_cpuid(0x80000001U, &eax, &ebx, &ecx, &edx) != 0 &&
           (edx & (1U << 27)) != 0;
}

static void *run_worker(void *opaque) {
    struct worker_context *context = opaque;
    struct smpqual_shared *shared = context->shared;
    const unsigned int worker_id = context->worker_id;
    unsigned int observed_cpu = 0;

    if (atomic_load_explicit(&shared->failed, memory_order_acquire) ||
        deadline_expired(&shared->deadline) || set_and_verify_worker_affinity(worker_id) != 0 ||
        observed_logical_cpu(&observed_cpu) != 0 || observed_cpu != worker_id) {
        debug_line("smpqual: worker=%u affinity-or-cpu-observation-failed", worker_id);
        mark_failed(shared);
        return NULL;
    }

    /* The ready syscall has returned before this worker becomes barrier-visible. */
    if (deadline_expired(&shared->deadline) ||
        emit_milestone(PRODUCT_MILESTONE_SMPQUAL_READY, observed_cpu, worker_id,
                       shared->config.work_units) != 0) {
        debug_line("smpqual: worker=%u ready-milestone-failed", worker_id);
        mark_failed(shared);
        return NULL;
    }
    if (atomic_fetch_add_explicit(&shared->ready_workers, 1, memory_order_acq_rel) + 1 ==
        shared->config.workers) {
        atomic_store_explicit(&shared->start_gate, true, memory_order_release);
    }
    if (wait_for_gate(shared, &shared->start_gate) != 0 || deadline_expired(&shared->deadline) ||
        emit_milestone(PRODUCT_MILESTONE_SMPQUAL_START, observed_cpu, worker_id,
                       shared->config.work_units) != 0) {
        debug_line("smpqual: worker=%u start-phase-failed", worker_id);
        mark_failed(shared);
        return NULL;
    }

    uint64_t accumulator = ((uint64_t)worker_id << 32) | 1U;
    for (uint64_t completed = 0; completed < shared->config.work_units; ++completed) {
        /* Volatile publication makes every bounded iteration observable to the compiler. */
        accumulator = (accumulator << 7) ^ (accumulator >> 3) ^ completed;
        context->work_sink = accumulator;
        context->completed_work_units = completed + 1;
        if ((completed & 0x3ffU) == 0 &&
            (atomic_load_explicit(&shared->failed, memory_order_acquire) ||
             deadline_expired(&shared->deadline))) {
            debug_line("smpqual: worker=%u work-deadline-or-peer-failure", worker_id);
            mark_failed(shared);
            return NULL;
        }
    }
    if (context->completed_work_units != shared->config.work_units ||
        deadline_expired(&shared->deadline)) {
        debug_line("smpqual: worker=%u incomplete-or-late-work", worker_id);
        mark_failed(shared);
        return NULL;
    }

    /* Do not publish a terminal success phase until every worker finished exactly W units. */
    if (atomic_fetch_add_explicit(&shared->completed_workers, 1, memory_order_acq_rel) + 1 ==
        shared->config.workers) {
        atomic_store_explicit(&shared->finish_gate, true, memory_order_release);
    }
    if (wait_for_gate(shared, &shared->finish_gate) != 0 || deadline_expired(&shared->deadline) ||
        emit_milestone(PRODUCT_MILESTONE_SMPQUAL_FINISH, observed_cpu, worker_id,
                       shared->config.work_units) != 0) {
        debug_line("smpqual: worker=%u finish-phase-failed", worker_id);
        mark_failed(shared);
        return NULL;
    }
    return NULL;
}

static int initialize_deadline(struct smpqual_shared *shared) {
    if (clock_gettime(CLOCK_MONOTONIC, &shared->deadline) != 0) {
        return -1;
    }
    shared->deadline.tv_sec += (time_t)(shared->config.deadline_ms / 1000U);
    shared->deadline.tv_nsec += (long)((shared->config.deadline_ms % 1000U) * 1000000U);
    if (shared->deadline.tv_nsec >= 1000000000L) {
        shared->deadline.tv_sec += 1;
        shared->deadline.tv_nsec -= 1000000000L;
    }
    return 0;
}

int main(int argc, char **argv) {
    struct smpqual_shared shared;
    atomic_init(&shared.ready_workers, 0);
    atomic_init(&shared.completed_workers, 0);
    atomic_init(&shared.start_gate, false);
    atomic_init(&shared.finish_gate, false);
    atomic_init(&shared.failed, false);
    if (parse_config(argc, argv, &shared.config) != 0 || !rdtscp_is_supported() ||
        initialize_deadline(&shared) != 0) {
        debug_line("smpqual: invalid-arguments-rdtscp-or-clock");
        return 1;
    }

    struct worker_context contexts[8];
    pthread_t threads[7];
    unsigned int created = 0;
    for (unsigned int worker_id = 0; worker_id < shared.config.workers; ++worker_id) {
        contexts[worker_id] = (struct worker_context){
            .shared = &shared,
            .worker_id = worker_id,
            .work_sink = 0,
            .completed_work_units = 0,
        };
    }
    for (unsigned int worker_id = 1; worker_id < shared.config.workers; ++worker_id) {
        if (pthread_create(&threads[created], NULL, run_worker, &contexts[worker_id]) != 0) {
            debug_line("smpqual: pthread-create-failed worker=%u", worker_id);
            mark_failed(&shared);
            break;
        }
        ++created;
    }

    /* The initial thread is worker 0, never a coordinator-only task. */
    (void)run_worker(&contexts[0]);
    for (unsigned int index = 0; index < created; ++index) {
        if (pthread_join(threads[index], NULL) != 0) {
            debug_line("smpqual: pthread-join-failed index=%u", index);
            mark_failed(&shared);
        }
    }

    if (atomic_load_explicit(&shared.failed, memory_order_acquire) ||
        atomic_load_explicit(&shared.completed_workers, memory_order_acquire) != shared.config.workers) {
        debug_line("smpqual: result=fail workers=%u", shared.config.workers);
        return 1;
    }
    unsigned int observed_cpu = 0;
    if (deadline_expired(&shared.deadline) || observed_logical_cpu(&observed_cpu) != 0 ||
        observed_cpu != 0 ||
        emit_milestone(PRODUCT_MILESTONE_SMPQUAL_COMPLETE, observed_cpu, 0,
                       shared.config.work_units) != 0) {
        debug_line("smpqual: terminal-complete-milestone-failed");
        return 1;
    }
    debug_line("smpqual: result=ok workers=%u work_units=%llu", shared.config.workers,
               (unsigned long long)shared.config.work_units);
    return 0;
}
