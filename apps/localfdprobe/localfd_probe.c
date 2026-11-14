#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PROBE_PATH "/apps/localfdprobe/localfdprobe.elf"
#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define CONTRACT_PATH "/system/registry/system/localfdprobe-v1.env"
#define CONTRACT_BODY "contract=rustos-localfd-probe-v1\n"
#define START_DELAY_S 4

extern char **environ;

static int failures;
static int checks;
static volatile sig_atomic_t sigpipe_count;

/* Lines go to the kernel debug channel: a session program has no console, and
 * `kvm-smoke --local-fd-probe` matches these lines in the debugcon log. */
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

static void check(bool ok, const char *name, const char *what) {
    checks++;
    if (!ok) {
        failures++;
        say("localfd: FAIL %s: %s (errno=%d)", name, what, errno);
    }
}

#define CHECK(cond, name) check((cond), (name), #cond)
#define CHECK_ERRNO(call, want, name)                                        \
    do {                                                                     \
        errno = 0;                                                           \
        long rc__ = (long)(call);                                            \
        check(rc__ == -1 && errno == (want), (name), #call " != -1/" #want); \
    } while (0)

static void on_sigpipe(int signo) {
    (void)signo;
    sigpipe_count++;
}

static void msleep(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static void case_done(const char *name, int failures_before) {
    if (failures == failures_before) {
        say("localfd: %s ok", name);
    } else {
        say("localfd: FAIL case %s", name);
    }
}

/* ---- pipe -------------------------------------------------------------- */

static void pipe_basic(void) {
    const char *name = "pipe-basic";
    int before = failures;
    int fds[2];
    CHECK(pipe(fds) == 0, name);
    char buf[64];
    CHECK(write(fds[1], "hello", 5) == 5, name);
    int avail = -1;
    CHECK(ioctl(fds[0], FIONREAD, &avail) == 0 && avail == 5, name);
    struct stat st;
    CHECK(fstat(fds[0], &st) == 0 && S_ISFIFO(st.st_mode), name);
    CHECK(fstat(fds[1], &st) == 0 && S_ISFIFO(st.st_mode), name);
    CHECK(read(fds[0], buf, sizeof(buf)) == 5 && memcmp(buf, "hello", 5) == 0, name);
    CHECK(write(fds[1], "", 0) == 0, name);
    CHECK(read(fds[0], buf, 0) == 0, name);
    CHECK_ERRNO(read(fds[1], buf, 1), EBADF, name);
    CHECK_ERRNO(write(fds[0], "x", 1), EBADF, name);
    CHECK_ERRNO(lseek(fds[0], 0, SEEK_SET), ESPIPE, name);
    CHECK(fcntl(fds[0], F_GETFL) == O_RDONLY, name);
    CHECK((fcntl(fds[1], F_GETFL) & O_ACCMODE) == O_WRONLY, name);
    CHECK(fcntl(fds[0], F_GETPIPE_SZ) == 65536, name);
    CHECK_ERRNO(pipe2(fds, O_DIRECT), EINVAL, name);
    CHECK_ERRNO(pipe2(fds, 0x40000000), EINVAL, name);
    CHECK_ERRNO(pipe2(NULL, 0), EFAULT, name);
    close(fds[0]);
    close(fds[1]);
    case_done(name, before);
}

static void pipe_flags(void) {
    const char *name = "pipe-flags";
    int before = failures;
    int fds[2];
    CHECK(pipe2(fds, O_CLOEXEC | O_NONBLOCK) == 0, name);
    CHECK(fcntl(fds[0], F_GETFD) == FD_CLOEXEC, name);
    CHECK(fcntl(fds[1], F_GETFD) == FD_CLOEXEC, name);
    CHECK(fcntl(fds[0], F_GETFL) & O_NONBLOCK, name);
    char buf[8];
    CHECK_ERRNO(read(fds[0], buf, sizeof(buf)), EAGAIN, name);

    /* Writes up to PIPE_BUF are atomic: fill the pipe to 100 bytes free, then
     * a 200 byte write must not be partial. */
    static char big[65536];
    memset(big, 'a', sizeof(big));
    CHECK(write(fds[1], big, 65536 - 100) == 65536 - 100, name);
    CHECK_ERRNO(write(fds[1], big, 200), EAGAIN, name);
    CHECK(write(fds[1], big, 100) == 100, name);
    CHECK_ERRNO(write(fds[1], big, 1), EAGAIN, name);
    /* Drain 10 bytes: a large nonblocking write is partial, never zero. */
    CHECK(read(fds[0], big, 10) == 10, name);
    CHECK(write(fds[1], big, 65536) == 10, name);

    /* F_SETFL propagates to dups: the status flag is on the description. */
    int dupfd = fcntl(fds[0], F_DUPFD_CLOEXEC, 10);
    CHECK(dupfd >= 10 && fcntl(dupfd, F_GETFD) == FD_CLOEXEC, name);
    CHECK(fcntl(fds[0], F_SETFL, 0) == 0, name);
    CHECK(!(fcntl(dupfd, F_GETFL) & O_NONBLOCK), name);
    CHECK(fcntl(dupfd, F_SETFL, O_NONBLOCK) == 0, name);
    CHECK(fcntl(fds[0], F_GETFL) & O_NONBLOCK, name);
    CHECK_ERRNO(fcntl(fds[0], F_SETFL, O_DIRECT), EINVAL, name);

    int small = fcntl(fds[0], F_SETPIPE_SZ, 4096);
    CHECK_ERRNO(fcntl(fds[0], F_SETPIPE_SZ, 4096), EBUSY, name); /* still full */
    (void)small;
    CHECK(fcntl(fds[0], F_SETPIPE_SZ, 65536) == 65536, name);
    CHECK_ERRNO(fcntl(fds[0], F_SETPIPE_SZ, 1 << 20), EPERM, name);
    close(dupfd);
    close(fds[0]);
    close(fds[1]);
    case_done(name, before);
}

static void pipe_eof_and_epipe(void) {
    const char *name = "pipe-eof-epipe";
    int before = failures;
    int fds[2];
    char buf[8];
    CHECK(pipe(fds) == 0, name);
    CHECK(write(fds[1], "ab", 2) == 2, name);
    close(fds[1]);
    CHECK(read(fds[0], buf, sizeof(buf)) == 2, name); /* data first */
    CHECK(read(fds[0], buf, sizeof(buf)) == 0, name); /* then EOF */
    CHECK(read(fds[0], buf, sizeof(buf)) == 0, name);
    close(fds[0]);

    /* EPIPE with SIGPIPE handled: the signal must actually be delivered. */
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_sigpipe;
    sigemptyset(&action.sa_mask);
    CHECK(sigaction(SIGPIPE, &action, NULL) == 0, name);
    CHECK(pipe(fds) == 0, name);
    close(fds[0]);
    sigpipe_count = 0;
    CHECK_ERRNO(write(fds[1], "x", 1), EPIPE, name);
    CHECK(sigpipe_count == 1, name);
    CHECK(write(fds[1], "", 0) == 0, name); /* empty write ignores readers */
    /* Ignored SIGPIPE still yields EPIPE. */
    signal(SIGPIPE, SIG_IGN);
    CHECK_ERRNO(write(fds[1], "x", 1), EPIPE, name);
    close(fds[1]);
    case_done(name, before);
}

static void pipe_dup_and_close(void) {
    const char *name = "pipe-dup-close";
    int before = failures;
    int fds[2];
    char buf[8];
    CHECK(pipe2(fds, O_NONBLOCK) == 0, name);
    int w2 = dup(fds[1]);
    int w3 = dup2(fds[1], 20);
    int w4 = dup3(fds[1], 21, O_CLOEXEC);
    CHECK(w2 >= 0 && w3 == 20 && w4 == 21, name);
    CHECK(fcntl(21, F_GETFD) == FD_CLOEXEC && fcntl(20, F_GETFD) == 0, name);
    close(fds[1]);
    close(w2);
    close(w3);
    CHECK_ERRNO(read(fds[0], buf, 1), EAGAIN, name); /* a writer remains */
    CHECK(write(w4, "z", 1) == 1, name);
    close(w4);
    CHECK(read(fds[0], buf, 8) == 1, name);
    CHECK(read(fds[0], buf, 8) == 0, name); /* last writer gone: EOF */
    close(fds[0]);
    case_done(name, before);
}

struct reader_ctx {
    int fd;
    ssize_t result;
    int err;
    char byte;
};

static void *blocking_reader(void *raw) {
    struct reader_ctx *ctx = raw;
    errno = 0;
    ctx->result = read(ctx->fd, &ctx->byte, 1);
    ctx->err = errno;
    return NULL;
}

static void pipe_blocking_and_wakeups(void) {
    const char *name = "pipe-blocking";
    int before = failures;
    int fds[2];
    pthread_t thread;
    struct reader_ctx ctx;

    /* A reader blocked on an empty pipe is woken by a write. */
    CHECK(pipe(fds) == 0, name);
    memset(&ctx, 0, sizeof(ctx));
    ctx.fd = fds[0];
    CHECK(pthread_create(&thread, NULL, blocking_reader, &ctx) == 0, name);
    msleep(100);
    CHECK(write(fds[1], "q", 1) == 1, name);
    say("localfd: step pipe-blocking reader-written");
    pthread_join(thread, NULL);
    say("localfd: step pipe-blocking reader-joined");
    CHECK(ctx.result == 1 && ctx.byte == 'q', name);
    close(fds[0]);
    close(fds[1]);

    /* A reader blocked on an empty pipe is woken by the last writer closing. */
    CHECK(pipe(fds) == 0, name);
    memset(&ctx, 0, sizeof(ctx));
    ctx.fd = fds[0];
    CHECK(pthread_create(&thread, NULL, blocking_reader, &ctx) == 0, name);
    msleep(100);
    close(fds[1]);
    pthread_join(thread, NULL);
    CHECK(ctx.result == 0, name);
    close(fds[0]);

    /* A writer blocked on a full pipe is woken by a read (space) and by the
     * reader closing (EPIPE). */
    static char big[65536];
    CHECK(pipe(fds) == 0, name);
    CHECK(write(fds[1], big, sizeof(big)) == (ssize_t)sizeof(big), name);
    say("localfd: step pipe-blocking full");
    pid_t child = fork();
    if (child == 0) {
        signal(SIGPIPE, SIG_IGN);
        ssize_t n = write(fds[1], "w", 1); /* blocks until the parent reads */
        _exit(n == 1 ? 0 : 1);
    }
    msleep(100);
    CHECK(read(fds[0], big, 4096) == 4096, name);
    say("localfd: step pipe-blocking read-done");
    int status = 0;
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, name);
    say("localfd: step pipe-blocking writer-woken");
    /* Refill to exactly full without blocking, then block a writer again. */
    int flags = fcntl(fds[1], F_GETFL);
    CHECK(fcntl(fds[1], F_SETFL, flags | O_NONBLOCK) == 0, name);
    while (write(fds[1], big, sizeof(big)) > 0) {
    }
    CHECK(errno == EAGAIN, name);
    CHECK(fcntl(fds[1], F_SETFL, flags) == 0, name);
    say("localfd: step pipe-blocking refilled");
    int w2 = dup(fds[1]);
    child = fork();
    if (child == 0) {
        signal(SIGPIPE, SIG_IGN);
        close(fds[0]); /* the child's copy of the read end must not keep the pipe open */
        errno = 0;
        say("localfd: step pipe-blocking child-writing");
        ssize_t n = write(fds[1], "z", 1); /* full: blocks, then reader closes */
        int saved = errno;
        say("localfd: step pipe-blocking child-write n=%d errno=%d", (int)n, saved);
        _exit(n == -1 && saved == EPIPE ? 0 : 2);
    }
    msleep(100);
    say("localfd: step pipe-blocking closing-reader");
    close(fds[0]);
    close(fds[1]);
    {
        /* Diagnostic: with the reader gone the write end polls POLLERR. */
        struct pollfd pfd = {w2, POLLOUT, 0};
        int ready = poll(&pfd, 1, 0);
        say("localfd: step pipe-blocking probe-poll ready=%d revents=%x", ready, pfd.revents);
    }
    /* Poll so a stuck writer is reported instead of hanging the boot. */
    pid_t reaped = 0;
    for (int i = 0; i < 50 && reaped == 0; i++) {
        reaped = waitpid(child, &status, WNOHANG);
        if (reaped == 0) {
            msleep(100);
        }
    }
    say("localfd: step pipe-blocking reaped=%d status=%d errno=%d", (int)reaped, status, errno);
    CHECK(reaped == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, name);
    close(w2);
    case_done(name, before);
}

static void pipe_fork_and_exec(void) {
    const char *name = "pipe-fork-exec";
    int before = failures;
    int fds[2];
    char buf[16];
    CHECK(pipe(fds) == 0, name);
    pid_t child = fork();
    if (child == 0) {
        close(fds[0]);
        write(fds[1], "from-child", 10);
        _exit(0);
    }
    close(fds[1]); /* the parent's write end: EOF arrives when the child exits */
    ssize_t total = 0, n;
    while ((n = read(fds[0], buf + total, sizeof(buf) - (size_t)total)) > 0) {
        total += n;
    }
    CHECK(n == 0 && total == 10 && memcmp(buf, "from-child", 10) == 0, name);
    int status = 0;
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status), name);
    close(fds[0]);

    /* exec honours CLOEXEC: the kept pipe survives, the cloexec one is gone. */
    int keep[2], gone[2];
    CHECK(pipe(keep) == 0 && pipe2(gone, O_CLOEXEC) == 0, name);
    child = fork();
    if (child == 0) {
        char a[16], b[16], c[16];
        snprintf(a, sizeof(a), "%d", keep[1]);
        snprintf(b, sizeof(b), "%d", gone[1]);
        snprintf(c, sizeof(c), "%d", gone[0]);
        char *argv[] = {(char *)PROBE_PATH, (char *)"post-exec", a, b, c, NULL};
        execve(PROBE_PATH, argv, environ);
        _exit(90);
    }
    close(keep[1]);
    close(gone[1]);
    CHECK(read(keep[0], buf, sizeof(buf)) == 4 && memcmp(buf, "kept", 4) == 0, name);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, name);
    close(keep[0]);
    close(gone[0]);

    /* A process that dies while holding the only write end gives EOF. */
    CHECK(pipe(fds) == 0, name);
    child = fork();
    if (child == 0) {
        close(fds[0]);
        _exit(0);
    }
    close(fds[1]);
    CHECK(waitpid(child, &status, 0) == child, name);
    CHECK(read(fds[0], buf, 1) == 0, name);
    close(fds[0]);
    case_done(name, before);
}

static int verify_post_exec(char **argv) {
    int keep_w = atoi(argv[2]), gone_w = atoi(argv[3]), gone_r = atoi(argv[4]);
    const char *name = "pipe-post-exec";
    CHECK_ERRNO(fcntl(gone_w, F_GETFD), EBADF, name);
    CHECK_ERRNO(fcntl(gone_r, F_GETFD), EBADF, name);
    CHECK(write(keep_w, "kept", 4) == 4, name);
    return failures == 0 ? 0 : 1;
}

static void *delayed_writer(void *raw) {
    int *fds = raw;
    msleep(100);
    uint64_t one = 1;
    if (fds[1] >= 0) {
        write(fds[1], &one, sizeof(one));
    }
    return NULL;
}

static void pipe_poll_and_epoll(void) {
    const char *name = "pipe-poll-epoll";
    int before = failures;
    int fds[2];
    char buf[8];
    CHECK(pipe(fds) == 0, name);

    struct pollfd p[2] = {{fds[0], POLLIN, 0}, {fds[1], POLLOUT, 0}};
    CHECK(poll(p, 2, 0) == 1 && p[0].revents == 0 && (p[1].revents & POLLOUT), name);
    CHECK(write(fds[1], "x", 1) == 1, name);
    CHECK(poll(p, 2, 0) == 2 && (p[0].revents & POLLIN), name);
    struct timespec zero = {0, 0};
    CHECK(ppoll(p, 2, &zero, NULL) == 2, name);

    int ep = epoll_create1(EPOLL_CLOEXEC);
    CHECK(ep >= 0, name);
    struct epoll_event ev = {.events = EPOLLIN, .data.u32 = 1};
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev) == 0, name);
    struct epoll_event out[4];
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && out[0].data.u32 == 1 && (out[0].events & EPOLLIN), name);
    CHECK(epoll_wait(ep, out, 4, 0) == 1, name); /* level-triggered: still ready */
    CHECK(read(fds[0], buf, 8) == 1, name);
    CHECK(epoll_wait(ep, out, 4, 0) == 0, name);

    /* A blocked epoll_wait is woken by a write from another thread. */
    pthread_t thread;
    int writer[2] = {-1, fds[1]};
    CHECK(pthread_create(&thread, NULL, delayed_writer, writer) == 0, name);
    int n = epoll_wait(ep, out, 4, 5000);
    CHECK(n == 1 && (out[0].events & EPOLLIN), name);
    pthread_join(thread, NULL);
    CHECK(read(fds[0], buf, 8) == 8, name);

    /* Hangup: the read end reports EPOLLHUP once the last writer is gone. */
    close(fds[1]);
    n = epoll_wait(ep, out, 4, 1000);
    CHECK(n == 1 && (out[0].events & EPOLLHUP), name);
    CHECK(poll(p, 1, 0) == 1 && (p[0].revents & POLLHUP), name);

    /* Edge-triggered: one report per new write. */
    int e[2];
    CHECK(pipe(e) == 0, name);
    int ep2 = epoll_create1(0);
    struct epoll_event et = {.events = EPOLLIN | EPOLLET, .data.u32 = 7};
    CHECK(epoll_ctl(ep2, EPOLL_CTL_ADD, e[0], &et) == 0, name);
    CHECK(epoll_wait(ep2, out, 4, 0) == 0, name);
    CHECK(write(e[1], "1", 1) == 1, name);
    CHECK(epoll_wait(ep2, out, 4, 1000) == 1, name);
    CHECK(epoll_wait(ep2, out, 4, 0) == 0, name); /* edge consumed */
    CHECK(write(e[1], "2", 1) == 1, name);
    CHECK(epoll_wait(ep2, out, 4, 1000) == 1, name);
    /* The write end: EPOLLOUT while there is room, EPOLLERR once unread. */
    int ep3 = epoll_create1(0);
    struct epoll_event wr = {.events = EPOLLOUT, .data.u32 = 9};
    CHECK(epoll_ctl(ep3, EPOLL_CTL_ADD, e[1], &wr) == 0, name);
    CHECK(epoll_wait(ep3, out, 4, 0) == 1 && (out[0].events & EPOLLOUT), name);
    close(e[0]);
    n = epoll_wait(ep3, out, 4, 1000);
    CHECK(n == 1 && (out[0].events & EPOLLERR), name);
    /* Closing the last descriptor drops its interest (no stale events). */
    CHECK(epoll_ctl(ep3, EPOLL_CTL_DEL, e[1], NULL) == 0, name);
    close(e[1]);
    close(fds[0]);
    close(ep);
    close(ep2);
    close(ep3);
    case_done(name, before);
}

static void pipe_many(void) {
    const char *name = "pipe-many";
    int before = failures;
    /* Resource hygiene: a thousand create/close cycles leave nothing behind and
     * a process that exhausts its descriptors gets EMFILE, not a hang. */
    for (int i = 0; i < 1000; i++) {
        int fds[2];
        if (pipe(fds) != 0) {
            check(false, name, "pipe() failed in the create/close loop");
            break;
        }
        write(fds[1], "x", 1);
        close(fds[0]);
        close(fds[1]);
    }
    int opened[256][2];
    int count = 0;
    while (count < 256 && pipe(opened[count]) == 0) {
        count++;
    }
    CHECK(count > 0, name);
    for (int i = 0; i < count; i++) {
        close(opened[i][0]);
        close(opened[i][1]);
    }
    case_done(name, before);
}

/* ---- eventfd ----------------------------------------------------------- */

static void eventfd_basic(void) {
    const char *name = "eventfd-basic";
    int before = failures;
    uint64_t v = 0;
    int fd = eventfd(3, EFD_NONBLOCK | EFD_CLOEXEC);
    CHECK(fd >= 0 && fcntl(fd, F_GETFD) == FD_CLOEXEC, name);
    CHECK(fcntl(fd, F_GETFL) & O_NONBLOCK, name);
    CHECK(read(fd, &v, 8) == 8 && v == 3, name);
    CHECK_ERRNO(read(fd, &v, 8), EAGAIN, name);
    v = 5;
    CHECK(write(fd, &v, 8) == 8, name);
    v = 6;
    CHECK(write(fd, &v, 8) == 8, name);
    CHECK(read(fd, &v, 8) == 8 && v == 11, name); /* accumulated and zeroed */
    CHECK_ERRNO(read(fd, &v, 4), EINVAL, name);
    CHECK_ERRNO(write(fd, &v, 4), EINVAL, name);
    v = UINT64_MAX;
    CHECK_ERRNO(write(fd, &v, 8), EINVAL, name);
    v = UINT64_MAX - 1;
    CHECK(write(fd, &v, 8) == 8, name);
    v = 1;
    CHECK_ERRNO(write(fd, &v, 8), EAGAIN, name); /* would overflow */
    CHECK(read(fd, &v, 8) == 8 && v == UINT64_MAX - 1, name);
    struct stat st;
    CHECK(fstat(fd, &st) == 0 && !S_ISFIFO(st.st_mode), name);
    CHECK_ERRNO(lseek(fd, 0, SEEK_SET), ESPIPE, name);
    CHECK_ERRNO(ioctl(fd, FIONREAD, &v), ENOTTY, name);
    close(fd);
    CHECK_ERRNO(eventfd(0, 0x40000000), EINVAL, name);

    int sem = eventfd(2, EFD_SEMAPHORE | EFD_NONBLOCK);
    CHECK(sem >= 0, name);
    CHECK(read(sem, &v, 8) == 8 && v == 1, name);
    CHECK(read(sem, &v, 8) == 8 && v == 1, name);
    CHECK_ERRNO(read(sem, &v, 8), EAGAIN, name);
    close(sem);
    int raw = (int)syscall(SYS_eventfd2, 4, EFD_NONBLOCK);
    CHECK(raw >= 0 && read(raw, &v, 8) == 8 && v == 4, name);
    close(raw);
    case_done(name, before);
}

struct efd_ctx {
    int fd;
    uint64_t value;
    ssize_t result;
};

static void *efd_blocking_reader(void *raw) {
    struct efd_ctx *ctx = raw;
    ctx->result = read(ctx->fd, &ctx->value, 8);
    return NULL;
}

static void *efd_blocking_writer(void *raw) {
    struct efd_ctx *ctx = raw;
    ctx->result = write(ctx->fd, &ctx->value, 8);
    return NULL;
}

static void eventfd_blocking_and_poll(void) {
    const char *name = "eventfd-blocking";
    int before = failures;
    int fd = eventfd(0, 0);
    CHECK(fd >= 0, name);
    pthread_t thread;
    struct efd_ctx ctx = {.fd = fd};
    CHECK(pthread_create(&thread, NULL, efd_blocking_reader, &ctx) == 0, name);
    msleep(100);
    uint64_t v = 42;
    CHECK(write(fd, &v, 8) == 8, name);
    pthread_join(thread, NULL);
    CHECK(ctx.result == 8 && ctx.value == 42, name);

    /* A writer blocked on overflow is woken by a read. */
    v = UINT64_MAX - 1;
    CHECK(write(fd, &v, 8) == 8, name);
    struct efd_ctx wctx = {.fd = fd, .value = 1};
    CHECK(pthread_create(&thread, NULL, efd_blocking_writer, &wctx) == 0, name);
    msleep(100);
    CHECK(read(fd, &v, 8) == 8 && v == UINT64_MAX - 1, name);
    pthread_join(thread, NULL);
    CHECK(wctx.result == 8, name);
    CHECK(read(fd, &v, 8) == 8 && v == 1, name);

    /* poll/epoll readiness. */
    struct pollfd p = {fd, POLLIN | POLLOUT, 0};
    CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLOUT, name);
    v = 1;
    write(fd, &v, 8);
    CHECK(poll(&p, 1, 0) == 1 && p.revents == (POLLIN | POLLOUT), name);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 0xfeed};
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) == 0, name);
    struct epoll_event out[2];
    CHECK(epoll_wait(ep, out, 2, 0) == 1 && out[0].data.u64 == 0xfeed, name);
    CHECK(read(fd, &v, 8) == 8, name);
    CHECK(epoll_wait(ep, out, 2, 0) == 0, name);

    /* This is the message-loop wake pattern: a blocked epoll_wait is woken by
     * an eventfd write from another thread, repeatedly. */
    for (int i = 0; i < 20; i++) {
        struct efd_ctx wake = {.fd = fd, .value = 1};
        CHECK(pthread_create(&thread, NULL, efd_blocking_writer, &wake) == 0, name);
        int n = epoll_wait(ep, out, 2, 5000);
        CHECK(n == 1 && (out[0].events & EPOLLIN), name);
        pthread_join(thread, NULL);
        CHECK(read(fd, &v, 8) == 8 && v == 1, name);
    }
    CHECK(epoll_ctl(ep, EPOLL_CTL_DEL, fd, NULL) == 0, name);
    close(ep);

    /* Inherited by fork: the child's write wakes the parent. */
    pid_t child = fork();
    if (child == 0) {
        msleep(100);
        uint64_t w = 7;
        _exit(write(fd, &w, 8) == 8 ? 0 : 1);
    }
    CHECK(read(fd, &v, 8) == 8 && v == 7, name);
    int status = 0;
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, name);
    int dupfd = dup(fd);
    close(fd);
    v = 9;
    CHECK(write(dupfd, &v, 8) == 8, name); /* the description outlived fd */
    close(dupfd);
    case_done(name, before);
}

int main(int argc, char **argv) {
    if (argc >= 5 && strcmp(argv[1], "post-exec") == 0) {
        return verify_post_exec(argv);
    }
    /* Session-startup programs run in every boot that reaches ui-ready, so the
     * probe runs only when `kvm-smoke --local-fd-probe` wrote the private
     * contract; without it the program exits at once, silently. A manual
     * argument ("pipe", "eventfd", "all") also forces a run. */
    if (argc < 2 && !contract_present()) {
        return 0;
    }
    const char *mode = argc >= 2 ? argv[1] : "all";
    bool pipes = strcmp(mode, "all") == 0 || strcmp(mode, "pipe") == 0;
    bool events = strcmp(mode, "all") == 0 || strcmp(mode, "eventfd") == 0;
    if (!pipes && !events) {
        say("localfd: usage: all | pipe | eventfd");
        return 2;
    }
    if (argc < 2) {
        msleep(START_DELAY_S * 1000L);
    }
    say("localfd: start mode=%s", mode);
    if (pipes) {
        pipe_basic();
        pipe_flags();
        pipe_eof_and_epipe();
        pipe_dup_and_close();
        pipe_blocking_and_wakeups();
        pipe_fork_and_exec();
        pipe_poll_and_epoll();
        pipe_many();
    }
    if (events) {
        eventfd_basic();
        eventfd_blocking_and_poll();
    }
    say("localfd: %s (%d checks, %d failures)", failures == 0 ? "PASS" : "FAIL", checks, failures);
    return failures == 0 ? 0 : 1;
}
