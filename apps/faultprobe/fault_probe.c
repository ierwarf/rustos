#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define CONTRACT_PATH "/system/registry/system/faultprobe-v1.env"
#define CONTRACT_BODY "contract=rustos-fault-probe-v1\n"
#define START_DELAY_S 4
#define WAIT_MS 10000    /* every parent wait on a child */
#define GONE_MS 5000     /* bound for "all siblings gone" polling */
#define CHILD_ALARM_S 40 /* a stuck child dies on its own */
#define MAX_TIDS 8
#define FAULT_ITERATIONS 6
#define STORM_THREADS 1100 /* above the old 1024-slot ceiling */
#define STORM_STACK_BYTES (64 * 1024)
#define STORM_WAIT_MS 100000

static int failures;

struct tid_report {
    int n;
    int tids[MAX_TIDS];
};

/* Lines go to the kernel debug channel: a session program has no console, and
 * `kvm-smoke --fault-probe` matches these lines in the debugcon log. */
static void say(const char *fmt, ...) {
    char line[256];
    va_list args;
    va_start(args, fmt);
    int len = vsnprintf(line, sizeof(line) - 1, fmt, args);
    va_end(args);
    if (len <= 0) {
        return;
    }
    if ((size_t)len > sizeof(line) - 2) {
        len = (int)sizeof(line) - 2;
    }
    line[len++] = '\n';
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, line, (unsigned long)len);
}

static void fail(const char *name, const char *fmt, ...) {
    char detail[160];
    va_list args;
    va_start(args, fmt);
    vsnprintf(detail, sizeof(detail), fmt, args);
    va_end(args);
    failures++;
    say("fault-probe: FAIL %s: %s", name, detail);
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

static void msleep(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static int gettid_raw(void) {
    return (int)syscall(SYS_gettid);
}

/* ---- parent-side helpers ------------------------------------------------ */

/* Poll wait4(WNOHANG) until `ms` elapse. Returns pid, 0 on timeout, -1 on error. */
static pid_t wait_bounded(pid_t pid, int *status, long ms) {
    for (long waited = 0; waited <= ms; waited += 5) {
        pid_t got = wait4(pid, status, WNOHANG, NULL);
        if (got != 0) {
            return got;
        }
        msleep(5);
    }
    return 0;
}

static void reap_hung(pid_t pid) {
    int status;
    kill(pid, SIGKILL);
    (void)wait_bounded(pid, &status, 2000);
}

/* Polls until kill(pid,0) / tgkill(pid,tid,0) report ESRCH. */
static bool gone_bounded(pid_t pid, int tid) {
    for (long waited = 0; waited <= GONE_MS; waited += 10) {
        errno = 0;
        long rc = tid ? syscall(SYS_tgkill, pid, tid, 0) : kill(pid, 0);
        if (rc == -1 && errno == ESRCH) {
            return true;
        }
        msleep(10);
    }
    return false;
}

static bool all_gone(const char *name, pid_t pid, const struct tid_report *rep) {
    bool ok = true;
    if (!gone_bounded(pid, 0)) {
        fail(name, "kill(child,0) never returned ESRCH");
        ok = false;
    }
    for (int i = 0; rep != NULL && i < rep->n; i++) {
        if (!gone_bounded(pid, rep->tids[i])) {
            fail(name, "tgkill(child,tid %d) never returned ESRCH", rep->tids[i]);
            ok = false;
        }
    }
    return ok;
}

static bool read_report(int fd, struct tid_report *rep) {
    size_t off = 0;
    for (long waited = 0; off < sizeof(*rep) && waited <= WAIT_MS;) {
        struct pollfd pfd = {fd, POLLIN, 0};
        int rc = poll(&pfd, 1, 50);
        if (rc == 0) {
            waited += 50;
            continue;
        }
        if (rc < 0) {
            return false;
        }
        ssize_t got = read(fd, (char *)rep + off, sizeof(*rep) - off);
        if (got <= 0) {
            return false;
        }
        off += (size_t)got;
    }
    return off == sizeof(*rep);
}

static void send_report(int fd, const struct tid_report *rep) {
    (void)write(fd, rep, sizeof(*rep));
}

/* Reaps `pid` once, then asserts the second wait4 has nothing left to report. */
static bool expect_status(const char *name, pid_t pid, int want_sig, int *out_status) {
    int status = 0;
    pid_t got = wait_bounded(pid, &status, WAIT_MS);
    if (got == 0) {
        fail(name, "wait4 timed out (hang)");
        reap_hung(pid);
        return false;
    }
    if (got != pid) {
        fail(name, "wait4 returned %d errno=%d", (int)got, errno);
        return false;
    }
    if (out_status != NULL) {
        *out_status = status;
    }
    if (want_sig != 0 && !(WIFSIGNALED(status) && WTERMSIG(status) == want_sig)) {
        fail(name, "status 0x%x, want signal %d", status, want_sig);
        return false;
    }
    errno = 0;
    if (wait4(pid, &status, WNOHANG, NULL) != -1 || errno != ECHILD) {
        fail(name, "second wait4 did not return ECHILD (reported twice)");
        return false;
    }
    return true;
}

/* ---- child-side helpers ------------------------------------------------- */

static volatile int ready_count;
static volatile int go_flag;
static int tids[MAX_TIDS];
static volatile uint32_t futex_word;

static void child_setup(void) {
    struct rlimit no_core = {0, 0};
    setrlimit(RLIMIT_CORE, &no_core);
    signal(SIGSEGV, SIG_DFL);
    signal(SIGKILL, SIG_DFL);
    signal(SIGALRM, SIG_DFL);
    alarm(CHILD_ALARM_S);
}

static void spin_pause(void) {
    __asm__ volatile("pause" ::: "memory");
}

static void announce(int slot) {
    tids[slot] = gettid_raw();
    say("fault-probe: trace thread up slot=%d tid=%d", slot, tids[slot]);
    __sync_fetch_and_add(&ready_count, 1);
}

static void fault_now(void) {
    /* A volatile pointer keeps the compiler from proving the address null. */
    volatile int *volatile target = (volatile int *)0x10;
    *target = 1;
}

static void *spin_thread(void *arg) {
    announce((int)(intptr_t)arg);
    for (;;) {
        spin_pause();
    }
    return NULL;
}

static void *sleep_thread(void *arg) {
    announce((int)(intptr_t)arg);
    for (;;) {
        msleep(100);
    }
    return NULL;
}

static void *futex_thread(void *arg) {
    announce((int)(intptr_t)arg);
    for (;;) {
        syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, NULL, NULL, 0);
    }
    return NULL;
}

static void *fault_thread(void *arg) {
    announce((int)(intptr_t)arg);
    while (!go_flag) {
        spin_pause();
    }
    fault_now();
    return NULL;
}

static void start(void *(*fn)(void *), int slot) {
    pthread_t th;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    say("fault-probe: trace creating slot=%d", slot);
    if (pthread_create(&th, &attr, fn, (void *)(intptr_t)slot) != 0) {
        say("fault-probe: trace create failed slot=%d errno=%d", slot, errno);
        _exit(90);
    }
    say("fault-probe: trace created slot=%d", slot);
}

static void wait_ready(int want) {
    for (long i = 0; ready_count < want; i++) {
        if (i > 20000) {
            _exit(91); /* threads never came up */
        }
        msleep(1);
    }
}

static void report_tids(int fd, int n) {
    struct tid_report rep;
    memset(&rep, 0, sizeof(rep));
    rep.n = n;
    memcpy(rep.tids, tids, sizeof(int) * (size_t)n);
    send_report(fd, &rep);
}

/* ---- cases -------------------------------------------------------------- */

static pid_t spawn(int *report_fd) {
    int fds[2] = {-1, -1};
    if (report_fd != NULL && pipe(fds) != 0) {
        return -1;
    }
    pid_t pid = fork();
    say("fault-probe: trace fork returned %d", (int)pid);
    if (pid == 0) {
        if (report_fd != NULL) {
            close(fds[0]);
            *report_fd = fds[1];
        }
        child_setup();
        return 0;
    }
    if (report_fd != NULL) {
        close(fds[1]);
        *report_fd = fds[0];
    }
    return pid;
}

static void case_single_thread(void) {
    const char *name = "single-thread-segv";
    int before = failures;
    static const uintptr_t addrs[] = {0x10, 0xdead000000000000UL};
    for (size_t i = 0; i < sizeof(addrs) / sizeof(addrs[0]); i++) {
        pid_t pid = spawn(NULL);
        if (pid < 0) {
            fail(name, "fork errno=%d", errno);
            break;
        }
        if (pid == 0) {
            *(volatile int *)addrs[i] = 1;
            _exit(80); /* fault was swallowed */
        }
        if (expect_status(name, pid, SIGSEGV, NULL)) {
            all_gone(name, pid, NULL);
        }
    }
    if (failures == before) {
        say("fault-probe: %s ok", name);
    }
}

static void case_multi_nonleader(void) {
    const char *name = "mt-nonleader-fault";
    int before = failures;
    int rfd;
    pid_t pid = spawn(&rfd);
    if (pid < 0) {
        fail(name, "fork errno=%d", errno);
        return;
    }
    if (pid == 0) {
        start(spin_thread, 0);
        start(spin_thread, 1);
        start(spin_thread, 2);
        start(futex_thread, 3);
        start(fault_thread, 4);
        wait_ready(5);
        report_tids(rfd, 5);
        go_flag = 1; /* a non-leader thread faults; the leader just sleeps */
        for (;;) {
            msleep(1000);
        }
    }
    struct tid_report rep;
    if (!read_report(rfd, &rep)) {
        fail(name, "child never reported its threads");
        reap_hung(pid);
        close(rfd);
        return;
    }
    close(rfd);
    if (expect_status(name, pid, SIGSEGV, NULL)) {
        all_gone(name, pid, &rep);
    }
    if (failures == before) {
        say("fault-probe: %s ok", name);
    }
}

static void case_leader_exit_kill(void) {
    const char *name = "leader-exit-sigkill";
    int before = failures;
    int rfd;
    pid_t pid = spawn(&rfd);
    if (pid < 0) {
        fail(name, "fork errno=%d", errno);
        return;
    }
    if (pid == 0) {
        start(spin_thread, 0);
        start(sleep_thread, 1);
        wait_ready(2);
        report_tids(rfd, 2);
        syscall(SYS_exit, 0); /* the leader thread alone retires */
        _exit(81);
    }
    struct tid_report rep;
    if (!read_report(rfd, &rep)) {
        fail(name, "child never reported its threads");
        reap_hung(pid);
        close(rfd);
        return;
    }
    close(rfd);
    msleep(300); /* let the leader retire */
    int status = 0;
    pid_t early = wait4(pid, &status, WNOHANG, NULL);
    if (early != 0) {
        fail(name, "wait4 reported the group (rc=%d status=0x%x) while siblings live", (int)early, status);
        if (early < 0) {
            reap_hung(pid);
        }
        return;
    }
    errno = 0;
    if (syscall(SYS_tgkill, pid, rep.tids[0], 0) != 0) {
        fail(name, "tgkill(sibling) errno=%d with leader retired", errno);
    }
    errno = 0;
    if (kill(pid, SIGKILL) != 0) {
        fail(name, "kill(pid,SIGKILL) errno=%d with leader retired", errno);
        reap_hung(pid);
        return;
    }
    if (expect_status(name, pid, SIGKILL, NULL)) {
        all_gone(name, pid, &rep);
    }
    if (failures == before) {
        say("fault-probe: %s ok", name);
    }
}

static void case_fault_vs_exit_group(void) {
    const char *name = "fault-vs-exit-group";
    int before = failures;
    int exited = 0;
    int segv = 0;
    for (int iter = 0; iter < FAULT_ITERATIONS; iter++) {
        int rfd;
        pid_t pid = spawn(&rfd);
        if (pid < 0) {
            fail(name, "fork errno=%d", errno);
            return;
        }
        if (pid == 0) {
            ready_count = 0;
            go_flag = 0;
            start(fault_thread, 0);
            wait_ready(1);
            report_tids(rfd, 1);
            go_flag = 1;
            for (int spin = 0; spin < iter * 4000; spin++) {
                spin_pause();
            }
            syscall(SYS_exit_group, 7);
            _exit(82);
        }
        struct tid_report rep;
        if (!read_report(rfd, &rep)) {
            fail(name, "iteration %d: child never reported", iter);
            reap_hung(pid);
            close(rfd);
            return;
        }
        close(rfd);
        int status = 0;
        if (!expect_status(name, pid, 0, &status)) {
            return;
        }
        if (WIFEXITED(status) && WEXITSTATUS(status) == 7) {
            exited++;
        } else if (WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV) {
            segv++;
        } else {
            fail(name, "iteration %d: status 0x%x is neither exit(7) nor SIGSEGV", iter, status);
            return;
        }
        if (!all_gone(name, pid, &rep)) {
            return;
        }
    }
    if (failures == before) {
        say("fault-probe: %s ok (exit7=%d segv=%d)", name, exited, segv);
    }
}

/* Thread storm: one process holds more simultaneous threads than the retired
 * 1024-slot table allowed, all parked on a futex, then releases and joins them.
 * Proves slots, kernel stacks and the heap grow on demand instead of failing at
 * a fixed ceiling. Runs in a forked child so a refusal cannot hurt the parent. */
static volatile uint32_t storm_gate;
static pthread_t storm_threads[STORM_THREADS];

static void *storm_thread(void *arg) {
    (void)arg;
    __sync_fetch_and_add(&ready_count, 1);
    while (storm_gate == 0) {
        syscall(SYS_futex, &storm_gate, FUTEX_WAIT, 0, NULL, NULL, 0);
    }
    return NULL;
}

static void storm_child(int rfd) {
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, STORM_STACK_BYTES);
    int created = 0;
    int err = 0;
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    alarm(120);
    for (; created < STORM_THREADS; created++) {
        err = pthread_create(&storm_threads[created], &attr, storm_thread, NULL);
        if (err != 0) {
            break;
        }
        if ((created + 1) % 100 == 0) {
            struct timespec now;
            clock_gettime(CLOCK_MONOTONIC, &now);
            say("fault-probe: thread-storm progress created=%d ms=%ld", created + 1,
                (now.tv_sec - t0.tv_sec) * 1000L + (now.tv_nsec - t0.tv_nsec) / 1000000L);
        }
    }
    long waited = 0;
    while (ready_count < created && waited < STORM_WAIT_MS) {
        msleep(10);
        waited += 10;
    }
    int ready = ready_count;
    say("fault-probe: thread-storm created=%d ready=%d err=%d", created, ready, err);
    storm_gate = 1;
    syscall(SYS_futex, &storm_gate, FUTEX_WAKE, 0x7fffffff, NULL, NULL, 0);
    for (int i = 0; i < created; i++) {
        pthread_join(storm_threads[i], NULL);
    }
    int ok = created == STORM_THREADS && ready == STORM_THREADS;
    (void)write(rfd, &ok, sizeof(ok));
    _exit(ok ? 0 : 91);
}

static void case_thread_storm(void) {
    const char *name = "thread-storm";
    int before = failures;
    int rfd;
    pid_t pid = spawn(&rfd);
    if (pid < 0) {
        fail(name, "fork errno=%d", errno);
        return;
    }
    if (pid == 0) {
        storm_child(rfd);
    }
    int status = 0;
    pid_t got = wait_bounded(pid, &status, STORM_WAIT_MS + 15000);
    if (got != pid) {
        fail(name, "child did not finish (got=%d errno=%d)", (int)got, errno);
        reap_hung(pid);
    } else if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fail(name, "child status 0x%x (see thread-storm record)", status);
    }
    close(rfd);
    if (failures == before) {
        say("fault-probe: %s ok (%d threads)", name, STORM_THREADS);
    }
}

int main(int argc, char **argv) {
    (void)argv;
    /* Session-startup programs run in every boot that reaches ui-ready, so the
     * probe runs only when `kvm-smoke --fault-probe` wrote the private
     * contract; without it the program exits at once, silently. Any manual
     * argument forces an immediate run. */
    if (argc < 2 && !contract_present()) {
        return 0;
    }
    if (argc < 2) {
        msleep(START_DELAY_S * 1000L);
    }
    signal(SIGPIPE, SIG_IGN);
    say("fault-probe: start");
    case_single_thread();
    case_multi_nonleader();
    case_leader_exit_kill();
    case_fault_vs_exit_group();
    case_thread_storm();
    if (failures == 0) {
        say("fault-probe: all passed");
        return 0;
    }
    say("fault-probe: FAIL total=%d", failures);
    return 1;
}
