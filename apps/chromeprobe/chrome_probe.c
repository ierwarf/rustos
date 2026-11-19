#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysinfo.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <sched.h>
#include <time.h>
#include <unistd.h>

/* Chromium-class Linux surface checker. Owner: docs/ai/commands.md
 * (`kvm-smoke --chrome-compat-probe`), xtask side tools/xtask/src/kvm/chrome_probe.rs.
 * Case names below must equal CHROME_PROBE_CASES there. Probe cases never use a
 * sandbox bypass; they only observe kernel behaviour. */

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define CONTRACT_PATH "/system/registry/system/chromeprobe-v1.env"
#define CONTRACT_BODY "contract=rustos-chrome-probe-v1\n"
#define START_DELAY_S 4
#define DATA_WAIT_S 90

#ifndef SYS_renameat2
#define SYS_renameat2 316
#endif
#ifndef SYS_membarrier
#define SYS_membarrier 324
#endif
#ifndef SYS_copy_file_range
#define SYS_copy_file_range 326
#endif
#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif
#ifndef SYS_memfd_create
#define SYS_memfd_create 319
#endif
#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE 1
#endif
#ifndef F_OFD_GETLK
#define F_OFD_GETLK 36
#define F_OFD_SETLK 37
#define F_OFD_SETLKW 38
#endif
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 2U
#endif
#ifndef O_TMPFILE
#define O_TMPFILE (020000000 | O_DIRECTORY)
#endif
#ifndef O_PATH
#define O_PATH 010000000
#endif
#ifndef MSG_CMSG_CLOEXEC
#define MSG_CMSG_CLOEXEC 0x40000000
#endif

/* Minimal rtnetlink wire types so the probe does not depend on kernel headers. */
struct cp_nlmsghdr {
    uint32_t len;
    uint16_t type;
    uint16_t flags;
    uint32_t seq;
    uint32_t pid;
};
struct cp_rtgenmsg {
    uint8_t family;
};
struct cp_ifinfomsg {
    uint8_t family, pad;
    uint16_t type;
    int32_t index;
    uint32_t flags, change;
};
struct cp_ifaddrmsg {
    uint8_t family, prefixlen, flags, scope;
    uint32_t index;
};
struct cp_rtattr {
    uint16_t len;
    uint16_t type;
};
#define CP_NETLINK_ROUTE 0
#define CP_NLM_F_REQUEST 1
#define CP_NLM_F_DUMP 0x300
#define CP_NLMSG_DONE 3
#define CP_NLMSG_ERROR 2
#define CP_RTM_NEWLINK 16
#define CP_RTM_GETLINK 18
#define CP_RTM_NEWADDR 20
#define CP_RTM_GETADDR 22
#define CP_IFLA_IFNAME 3
#define CP_IFA_LABEL 3
#define CP_NLA_ALIGN(n) (((n) + 3U) & ~3U)

static int failures;
static int checks;
static const char *cur_case = "init";
static int case_failures;
static char failed_cases[512];

static void say(const char *fmt, ...) {
    char line[320];
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

enum persist_phase { PERSIST_AUTO, PERSIST_FIRST, PERSIST_SECOND };
static enum persist_phase expected_persist = PERSIST_AUTO;

static int contract_present(void) {
    char body[128];
    int fd = open(CONTRACT_PATH, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t got = read(fd, body, sizeof(body));
    close(fd);
    static const char automatic[] = CONTRACT_BODY;
    static const char first[] = CONTRACT_BODY "persist=first\n";
    static const char second[] = CONTRACT_BODY "persist=second\n";
    if (got == sizeof(first)-1 && memcmp(body, first, sizeof(first)-1) == 0) {
        expected_persist = PERSIST_FIRST; return 1;
    }
    if (got == sizeof(second)-1 && memcmp(body, second, sizeof(second)-1) == 0) {
        expected_persist = PERSIST_SECOND; return 1;
    }
    return got == sizeof(automatic)-1 && memcmp(body, automatic, sizeof(automatic)-1) == 0;
}

static void check_at(bool ok, const char *what, int line) {
    int saved = errno;
    checks++;
    if (!ok) {
        failures++;
        case_failures++;
        say("chromeprobe: %s FAIL %s @%d (errno=%d)", cur_case, what, line, saved);
    }
    errno = saved;
}

#define CHECK(cond) check_at((cond), #cond, __LINE__)
#define CHECK_ERRNO(call, want)                                              \
    do {                                                                     \
        errno = 0;                                                           \
        long rc__ = (long)(call);                                            \
        check_at(rc__ == -1 && errno == (want), #call " != -1/" #want, __LINE__); \
    } while (0)

static void case_begin(const char *name) {
    cur_case = name;
    case_failures = 0;
}

static void case_end(void) {
    if (case_failures == 0) {
        say("chromeprobe: %s ok", cur_case);
    } else {
        size_t used = strlen(failed_cases);
        snprintf(failed_cases + used, sizeof(failed_cases) - used, " %s", cur_case);
    }
}

static void msleep(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static ssize_t slurp(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    size_t used = 0;
    for (;;) {
        if (used + 1 >= cap) {
            break;
        }
        ssize_t n = read(fd, buf + used, cap - 1 - used);
        if (n < 0) {
            close(fd);
            return -1;
        }
        if (n == 0) {
            break;
        }
        used += (size_t)n;
    }
    close(fd);
    buf[used] = 0;
    return (ssize_t)used;
}

static int write_all(int fd, const void *buf, size_t len) {
    const char *p = buf;
    while (len) {
        ssize_t n = write(fd, p, len);
        if (n <= 0) {
            return -1;
        }
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static int make_file(const char *path, const char *content) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0 && content && write_all(fd, content, strlen(content)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

/* ---- tmpfs semantics (/tmp, /dev/shm, /run/user/1000) ------------------- */

static void tmpfs_suite(const char *name, const char *base) {
    case_begin(name);
    char dir[160], a[200], b[200], c[200], ln[200], lp1[200], lp2[200];
    snprintf(dir, sizeof(dir), "%s/chromeprobe.%d", base, (int)getpid());
    /* The base itself must exist; /run/user/1000 may need creating. */
    if (strcmp(base, "/run/user/1000") == 0) {
        mkdir("/run/user", 0755);
        mkdir(base, 0700);
    }
    CHECK(mkdir(dir, 0700) == 0);
    CHECK(chmod(dir, 0700) == 0);
    struct stat st;
    CHECK(stat(dir, &st) == 0 && S_ISDIR(st.st_mode) && (st.st_mode & 0777) == 0700);
    CHECK_ERRNO(mkdir(dir, 0700), EEXIST);
    snprintf(a, sizeof(a), "%s/a", dir);
    snprintf(b, sizeof(b), "%s/b", dir);
    snprintf(c, sizeof(c), "%s/c", dir);
    snprintf(ln, sizeof(ln), "%s/ln", dir);
    snprintf(lp1, sizeof(lp1), "%s/loop1", dir);
    snprintf(lp2, sizeof(lp2), "%s/loop2", dir);

    int fd = open(a, O_RDWR | O_CREAT | O_EXCL, 0600);
    CHECK(fd >= 0);
    CHECK_ERRNO(open(a, O_RDWR | O_CREAT | O_EXCL, 0600), EEXIST);
    char buf[64];
    CHECK(write(fd, "hello world", 11) == 11);
    CHECK(pread(fd, buf, 5, 6) == 5 && memcmp(buf, "world", 5) == 0);
    CHECK(pwrite(fd, "HELLO", 5, 0) == 5);
    CHECK(pread(fd, buf, 11, 0) == 11 && memcmp(buf, "HELLO world", 11) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_size == 11 && S_ISREG(st.st_mode));
    close(fd);

    fd = open(a, O_RDWR | O_TRUNC);
    CHECK(fd >= 0 && fstat(fd, &st) == 0 && st.st_size == 0);
    CHECK(write(fd, "abc", 3) == 3);
    close(fd);
    fd = open(a, O_WRONLY | O_APPEND);
    CHECK(fd >= 0 && write(fd, "def", 3) == 3);
    CHECK(lseek(fd, 0, SEEK_SET) == 0 && write(fd, "ghi", 3) == 3); /* append ignores offset */
    close(fd);
    CHECK(slurp(a, buf, sizeof(buf)) == 9 && memcmp(buf, "abcdefghi", 9) == 0);

    /* Sparse growth: reads as zero without allocating the whole range. */
    fd = open(a, O_RDWR);
    CHECK(fd >= 0 && ftruncate(fd, 1 << 20) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_size == (1 << 20));
    CHECK(st.st_blocks < (1 << 20) / 512 / 2);
    static char big[4096];
    memset(big, 0x55, sizeof(big));
    CHECK(pread(fd, big, sizeof(big), 512 * 1024) == (ssize_t)sizeof(big));
    bool zero = true;
    for (size_t i = 0; i < sizeof(big); i++) {
        zero &= big[i] == 0;
    }
    CHECK(zero);
    CHECK(pread(fd, buf, 9, 0) == 9 && memcmp(buf, "abcdefghi", 9) == 0);
    CHECK(fallocate(fd, 0, 0, 64 * 1024) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_size == (1 << 20));
    CHECK(ftruncate(fd, 4) == 0 && fstat(fd, &st) == 0 && st.st_size == 4);
    close(fd);

    /* rename, RENAME_NOREPLACE. */
    CHECK(rename(a, b) == 0);
    CHECK_ERRNO(stat(a, &st), ENOENT);
    CHECK(stat(b, &st) == 0);
    int fc = make_file(c, "other");
    CHECK(fc >= 0);
    close(fc);
    errno = 0;
    long r = syscall(SYS_renameat2, AT_FDCWD, b, AT_FDCWD, c, RENAME_NOREPLACE);
    CHECK(r == -1 && errno == EEXIST);
    CHECK(slurp(c, buf, sizeof(buf)) == 5 && memcmp(buf, "other", 5) == 0);
    CHECK(syscall(SYS_renameat2, AT_FDCWD, b, AT_FDCWD, a, RENAME_NOREPLACE) == 0);
    CHECK(rename(a, c) == 0); /* plain rename replaces */
    CHECK(stat(c, &st) == 0 && st.st_size == 4);

    /* unlink-while-open keeps data readable and writable. */
    fd = open(c, O_RDWR);
    CHECK(fd >= 0 && unlink(c) == 0);
    CHECK_ERRNO(stat(c, &st), ENOENT);
    CHECK(pwrite(fd, "zzzz", 4, 4) == 4);
    CHECK(pread(fd, buf, 8, 0) == 8 && memcmp(buf, "abcdzzzz", 8) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_nlink == 0);
    close(fd);

    /* symlinks. */
    fc = make_file(a, "target");
    close(fc);
    CHECK(symlink("a", ln) == 0);
    ssize_t n = readlink(ln, buf, sizeof(buf));
    CHECK(n == 1 && buf[0] == 'a');
    CHECK(lstat(ln, &st) == 0 && S_ISLNK(st.st_mode));
    CHECK(stat(ln, &st) == 0 && S_ISREG(st.st_mode));
    CHECK_ERRNO(open(ln, O_RDONLY | O_NOFOLLOW), ELOOP);
    fd = open(ln, O_RDONLY);
    CHECK(fd >= 0 && read(fd, buf, 6) == 6 && memcmp(buf, "target", 6) == 0);
    close(fd);
    CHECK(symlink("loop2", lp1) == 0 && symlink("loop1", lp2) == 0);
    CHECK_ERRNO(open(lp1, O_RDONLY), ELOOP);
    CHECK_ERRNO(stat(lp1, &st), ELOOP);

    /* chmod / fchmod / utimensat are observable. */
    CHECK(chmod(a, 0640) == 0 && stat(a, &st) == 0 && (st.st_mode & 0777) == 0640);
    fd = open(a, O_RDWR);
    CHECK(fd >= 0 && fchmod(fd, 0600) == 0 && fstat(fd, &st) == 0 && (st.st_mode & 0777) == 0600);
    struct timespec ts[2] = {{1000000000, 0}, {1100000000, 0}};
    CHECK(utimensat(AT_FDCWD, a, ts, 0) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_mtim.tv_sec == 1100000000 && st.st_atim.tv_sec == 1000000000);
    close(fd);

    unlink(a); unlink(c); unlink(ln); unlink(lp1); unlink(lp2); unlink(b);
    CHECK(rmdir(dir) == 0);
    case_end();
}

/* ---- /data ---------------------------------------------------------------- */

#define PERSIST_DIR "/data/chromeprobe"
#define PERSIST_MARKER "/data/chromeprobe/marker"
#define PERSIST_SIZE 4097

static void persist_pattern(unsigned char *buf) {
    for (int i = 0; i < PERSIST_SIZE; i++) {
        buf[i] = (unsigned char)(((i * 131) + 7) ^ 0x5a);
    }
}

static void persist_check(void) {
    case_begin("persist");
    static unsigned char want[PERSIST_SIZE], got[PERSIST_SIZE + 8];
    persist_pattern(want);
    errno = 0;
    mkdir(PERSIST_DIR, 0755);
    struct stat st;
    if (stat(PERSIST_MARKER, &st) != 0) {
        if (errno != ENOENT || expected_persist == PERSIST_SECOND) {
            CHECK(false);
            case_end();
            return;
        }
        int fd = open(PERSIST_MARKER, O_RDWR | O_CREAT | O_EXCL, 0600);
        CHECK(fd >= 0);
        if (fd >= 0) {
            CHECK(write_all(fd, want, PERSIST_SIZE) == 0);
            CHECK(fsync(fd) == 0);
            close(fd);
            int dfd = open(PERSIST_DIR, O_RDONLY | O_DIRECTORY);
            CHECK(dfd >= 0 && fsync(dfd) == 0);
            if (dfd >= 0) {
                close(dfd);
            }
        }
        if (case_failures == 0) {
            say("chromeprobe: persist first-boot ok");
        } else {
            case_end();
        }
        return;
    }
    if (expected_persist == PERSIST_FIRST) {
        CHECK(false);
        case_end();
        return;
    }
    CHECK(S_ISREG(st.st_mode) && st.st_size == PERSIST_SIZE);
    int fd = open(PERSIST_MARKER, O_RDONLY);
    CHECK(fd >= 0);
    if (fd >= 0) {
        CHECK(read(fd, got, sizeof(got)) == PERSIST_SIZE);
        CHECK(memcmp(got, want, PERSIST_SIZE) == 0);
        close(fd);
    }
    if (case_failures == 0) {
        say("chromeprobe: persist second-boot ok");
    } else {
        case_end();
    }
}

static void data_dirs(void) {
    case_begin("data.dirs");
    static const char *const dirs[] = {"/data/home", "/data/config", "/data/cache"};
    for (size_t i = 0; i < 3; i++) {
        struct stat st;
        CHECK(stat(dirs[i], &st) == 0 && S_ISDIR(st.st_mode));
    }
    case_end();
}

static void data_atomic_write(void) {
    case_begin("data.atomic-write");
    mkdir(PERSIST_DIR, 0755);
    const char *target = PERSIST_DIR "/atomic.dat";
    const char *tmp = PERSIST_DIR "/atomic.dat.tmp";
    int dfd = open(PERSIST_DIR, O_RDONLY | O_DIRECTORY);
    CHECK(dfd >= 0);
    for (int round = 0; round < 3; round++) {
        char payload[64];
        int len = snprintf(payload, sizeof(payload), "important-file-round-%d", round);
        int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        CHECK(fd >= 0);
        if (fd < 0) {
            break;
        }
        CHECK(write_all(fd, payload, (size_t)len) == 0);
        CHECK(fsync(fd) == 0);
        CHECK(close(fd) == 0);
        CHECK(rename(tmp, target) == 0);
        if (dfd >= 0) {
            CHECK(fsync(dfd) == 0);
        }
        char back[64];
        CHECK(slurp(target, back, sizeof(back)) == len && memcmp(back, payload, (size_t)len) == 0);
        struct stat st;
        CHECK_ERRNO(stat(tmp, &st), ENOENT);
    }
    if (dfd >= 0) {
        close(dfd);
    }
    unlink(target);
    case_end();
}

/* ---- locks ---------------------------------------------------------------- */

static int wait_status(pid_t pid) {
    int status = 0;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) {
        return -1;
    }
    return WEXITSTATUS(status);
}

/* Child takes a POSIX write lock, reports on `up`, holds it until `down` is
 * written or closed, and exits. Returns the child pid. */
static pid_t lock_holder(const char *path, int *up_r, int *down_w) {
    int up[2], down[2];
    if (pipe(up) != 0 || pipe(down) != 0) {
        return -1;
    }
    pid_t pid = fork();
    if (pid == 0) {
        close(up[0]);
        close(down[1]);
        int fd = open(path, O_RDWR);
        struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
        char ok = fd >= 0 && fcntl(fd, F_SETLK, &fl) == 0 ? 'y' : 'n';
        write(up[1], &ok, 1);
        char go;
        (void)read(down[0], &go, 1);
        _exit(ok == 'y' ? 0 : 1);
    }
    close(up[1]);
    close(down[0]);
    *up_r = up[0];
    *down_w = down[1];
    return pid;
}

static void posix_lock_suite(const char *name, const char *path) {
    case_begin(name);
    int fd = make_file(path, "lockdata");
    CHECK(fd >= 0);
    int up = -1, down = -1;
    pid_t child = lock_holder(path, &up, &down);
    CHECK(child > 0);
    char ready = 0;
    CHECK(read(up, &ready, 1) == 1 && ready == 'y');
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    errno = 0;
    int r = fcntl(fd, F_SETLK, &fl);
    CHECK(r == -1 && (errno == EAGAIN || errno == EACCES));
    struct flock query = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    CHECK(fcntl(fd, F_GETLK, &query) == 0);
    CHECK(query.l_type == F_WRLCK && query.l_pid == child);
    struct flock rd = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    errno = 0;
    CHECK(fcntl(fd, F_SETLK, &rd) == -1 && (errno == EAGAIN || errno == EACCES));
    /* Release by the holder exiting. */
    close(down);
    close(up);
    CHECK(wait_status(child) == 0);
    CHECK(fcntl(fd, F_SETLK, &fl) == 0);
    struct flock unl = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    CHECK(fcntl(fd, F_SETLK, &unl) == 0);
    close(fd);
    unlink(path);
    case_end();
}

static void lock_ofd(void) {
    case_begin("lock.ofd");
    const char *path = "/tmp/chromeprobe.ofd";
    int fd1 = make_file(path, "ofd");
    int fd2 = open(path, O_RDWR);
    CHECK(fd1 >= 0 && fd2 >= 0);
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0, .l_pid = 0};
    CHECK(fcntl(fd1, F_OFD_SETLK, &fl) == 0);
    errno = 0;
    int r = fcntl(fd2, F_OFD_SETLK, &fl);
    CHECK(r == -1 && (errno == EAGAIN || errno == EACCES));
    struct flock query = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0, .l_pid = 0};
    CHECK(fcntl(fd2, F_OFD_GETLK, &query) == 0 && query.l_type == F_WRLCK && query.l_pid == -1);
    close(fd1);
    CHECK(fcntl(fd2, F_OFD_SETLK, &fl) == 0);
    close(fd2);
    unlink(path);
    case_end();
}

static void lock_flock(void) {
    case_begin("lock.flock");
    const char *path = "/tmp/chromeprobe.flock";
    int fd1 = make_file(path, "flock");
    int fd2 = open(path, O_RDWR);
    CHECK(fd1 >= 0 && fd2 >= 0);
    CHECK(flock(fd1, LOCK_EX | LOCK_NB) == 0);
    CHECK_ERRNO(flock(fd2, LOCK_EX | LOCK_NB), EWOULDBLOCK);
    CHECK_ERRNO(flock(fd2, LOCK_SH | LOCK_NB), EWOULDBLOCK);
    int dupfd = dup(fd1);
    close(fd1); /* the open description still holds the lock through the dup */
    CHECK_ERRNO(flock(fd2, LOCK_EX | LOCK_NB), EWOULDBLOCK);
    close(dupfd); /* last close releases */
    CHECK(flock(fd2, LOCK_EX | LOCK_NB) == 0);
    CHECK(flock(fd2, LOCK_UN) == 0);
    close(fd2);
    unlink(path);
    case_end();
}

static void lock_autorelease(void) {
    case_begin("lock.autorelease");
    const char *path = "/tmp/chromeprobe.auto";
    int fd = make_file(path, "auto");
    CHECK(fd >= 0);
    /* flock auto-release when the holder child exits. */
    int sync[2];
    CHECK(pipe(sync) == 0);
    pid_t child = fork();
    if (child == 0) {
        int cfd = open(path, O_RDWR);
        char ok = cfd >= 0 && flock(cfd, LOCK_EX | LOCK_NB) == 0 ? 'y' : 'n';
        write(sync[1], &ok, 1);
        msleep(300);
        _exit(ok == 'y' ? 0 : 1);
    }
    char ok = 0;
    CHECK(read(sync[0], &ok, 1) == 1 && ok == 'y');
    CHECK_ERRNO(flock(fd, LOCK_EX | LOCK_NB), EWOULDBLOCK);
    CHECK(wait_status(child) == 0);
    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0);
    flock(fd, LOCK_UN);
    close(sync[0]);
    close(sync[1]);

    /* A POSIX lock is released when ANY fd of the file is closed. */
    int fdb = open(path, O_RDWR);
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    CHECK(fdb >= 0 && fcntl(fd, F_SETLK, &fl) == 0);
    close(fdb);
    pid_t prober = fork();
    if (prober == 0) {
        int pfd = open(path, O_RDWR);
        _exit(pfd >= 0 && fcntl(pfd, F_SETLK, &fl) == 0 ? 0 : 1);
    }
    CHECK(wait_status(prober) == 0); /* the lock was dropped by close(fdb) */
    close(fd);
    unlink(path);
    case_end();
}

static void lock_exec_cloexec(const char *self_exe) {
    case_begin("lock.exec-cloexec");
    const char *path = "/tmp/chromeprobe.execlock";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    CHECK(fd >= 0);
    if (fd >= 0) {
        CHECK(write_all(fd, "lock", 4) == 0);
        pid_t child = fork();
        CHECK(child >= 0);
        if (child == 0) {
            struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0 };
            if (fcntl(fd, F_SETLK, &fl) != 0) {
                _exit(2);
            }
            char *const argv_exec[] = { (char *)self_exe, "--exec-child", NULL };
            execv("/proc/self/exe", argv_exec);
            execv(self_exe, argv_exec);
            _exit(127);
        }
        int status = 0;
        CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        struct flock probe = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0 };
        CHECK(fcntl(fd, F_GETLK, &probe) == 0 && probe.l_type == F_UNLCK);
        CHECK(fcntl(fd, F_SETLK, &probe) == 0);
        close(fd);
    }
    unlink(path);
    case_end();
}

/* ---- sockets -------------------------------------------------------------- */

static void sock_seqpacket(void) {
    case_begin("sock.seqpacket");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) == 0);
    CHECK(send(sv[0], "abc", 3, 0) == 3);
    CHECK(send(sv[0], "defgh", 5, 0) == 5);
    CHECK(send(sv[0], "", 0, 0) == 0);
    CHECK(send(sv[0], "z", 1, 0) == 1);
    char buf[16];
    /* A short buffer truncates the message; MSG_TRUNC reports the real size. */
    struct iovec iov = {buf, 2};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
    ssize_t n = recvmsg(sv[1], &msg, 0);
    CHECK(n == 2 && (msg.msg_flags & MSG_TRUNC));
    n = recv(sv[1], buf, 2, MSG_TRUNC);
    CHECK(n == 5); /* boundaries kept: next message is "defgh", length reported */
    n = recv(sv[1], buf, sizeof(buf), 0);
    CHECK(n == 0); /* the zero-length message, not EOF */
    n = recv(sv[1], buf, sizeof(buf), 0);
    CHECK(n == 1 && buf[0] == 'z'); /* the socket is still open */
    struct pollfd p = {.fd = sv[1], .events = POLLIN};
    CHECK(poll(&p, 1, 0) == 0);
    close(sv[0]);
    CHECK(poll(&p, 1, 1000) == 1 && (p.revents & (POLLIN | POLLHUP)));
    CHECK(recv(sv[1], buf, sizeof(buf), 0) == 0); /* real EOF */
    close(sv[1]);
    case_end();
}

static void sock_scm_rights(void) {
    case_begin("sock.scm-rights");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0);
    int pfd[2];
    CHECK(pipe(pfd) == 0);
    CHECK(send(sv[0], "A", 1, 0) == 1);
    char cbuf[CMSG_SPACE(sizeof(int))];
    memset(cbuf, 0, sizeof(cbuf));
    struct iovec iov = {"B", 1};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof(cbuf)};
    struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &pfd[0], sizeof(int));
    CHECK(sendmsg(sv[0], &msg, 0) == 1);
    CHECK(send(sv[0], "C", 1, 0) == 1);
    close(pfd[0]);
    for (int i = 0; i < 3; i++) {
        char data[4], rbuf[CMSG_SPACE(sizeof(int))];
        struct iovec riov = {data, sizeof(data)};
        struct msghdr rmsg = {.msg_iov = &riov, .msg_iovlen = 1, .msg_control = rbuf, .msg_controllen = sizeof(rbuf)};
        ssize_t n = recvmsg(sv[1], &rmsg, MSG_CMSG_CLOEXEC);
        CHECK(n == 1);
        struct cmsghdr *rc = CMSG_FIRSTHDR(&rmsg);
        if (i == 1) {
            CHECK(data[0] == 'B' && rc && rc->cmsg_level == SOL_SOCKET && rc->cmsg_type == SCM_RIGHTS);
            CHECK(rc && rc->cmsg_len == CMSG_LEN(sizeof(int)) && !(rmsg.msg_flags & MSG_CTRUNC));
            int got = -1;
            if (rc) {
                memcpy(&got, CMSG_DATA(rc), sizeof(int));
            }
            CHECK(got >= 0);
            int fl = got >= 0 ? fcntl(got, F_GETFD) : -1;
            CHECK(fl >= 0 && (fl & FD_CLOEXEC));
            CHECK(write(pfd[1], "p", 1) == 1);
            char pb = 0;
            CHECK(got >= 0 && read(got, &pb, 1) == 1 && pb == 'p');
            if (got >= 0) {
                close(got);
            }
        } else {
            CHECK(data[0] == (i == 0 ? 'A' : 'C') && rc == NULL);
        }
    }
    close(pfd[1]);
    close(sv[0]);
    close(sv[1]);
    case_end();
}

static void sock_unix_path(void) {
    case_begin("sock.unix-path");
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof(sa));
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof(sa.sun_path), "/tmp/chromeprobe.sock.%d", (int)getpid());
    unlink(sa.sun_path);
    int srv = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    CHECK(srv >= 0 && bind(srv, (struct sockaddr *)&sa, sizeof(sa)) == 0);
    struct stat st;
    CHECK(stat(sa.sun_path, &st) == 0 && S_ISSOCK(st.st_mode));
    CHECK(access(sa.sun_path, F_OK) == 0);
    int dup_srv = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK_ERRNO(bind(dup_srv, (struct sockaddr *)&sa, sizeof(sa)), EADDRINUSE);
    close(dup_srv);
    CHECK(listen(srv, 4) == 0);
    int cli = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(cli, (struct sockaddr *)&sa, sizeof(sa)) == 0);
    int acc = accept(srv, NULL, NULL);
    CHECK(acc >= 0);
    char buf[8];
    CHECK(write(cli, "ping", 4) == 4);
    CHECK(acc >= 0 && read(acc, buf, sizeof(buf)) == 4 && memcmp(buf, "ping", 4) == 0);
    CHECK(unlink(sa.sun_path) == 0);
    CHECK_ERRNO(access(sa.sun_path, F_OK), ENOENT);
    int late = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK_ERRNO(connect(late, (struct sockaddr *)&sa, sizeof(sa)), ENOENT);
    close(late);
    /* Rebinding the same path works once it is unlinked. */
    int srv2 = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(srv2, (struct sockaddr *)&sa, sizeof(sa)) == 0 && listen(srv2, 1) == 0);
    close(srv2);
    unlink(sa.sun_path);
    if (acc >= 0) {
        close(acc);
    }
    close(cli);
    close(srv);
    case_end();
}

static void sock_unix_rel_umask(void) {
    case_begin("sock.unix-rel-umask");
    char saved_cwd[256] = {0};
    CHECK(getcwd(saved_cwd, sizeof(saved_cwd)) != NULL);
    CHECK(chdir("/tmp") == 0);
    char rel_name[64];
    snprintf(rel_name, sizeof(rel_name), "chromeprobe.rel.%d", (int)getpid());
    unlink(rel_name);

    struct sockaddr_un sa;
    memset(&sa, 0, sizeof(sa));
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof(sa.sun_path), "%s", rel_name);

    mode_t prev_mask = umask(0077);
    int srv = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int brc = srv >= 0 ? bind(srv, (struct sockaddr *)&sa, sizeof(sa)) : -1;
    umask(prev_mask);
    CHECK(srv >= 0 && brc == 0);

    struct stat st;
    CHECK(stat(rel_name, &st) == 0 && S_ISSOCK(st.st_mode) && (st.st_mode & 0777) == 0700);
    CHECK(listen(srv, 4) == 0);
    int cli = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    CHECK(cli >= 0 && connect(cli, (struct sockaddr *)&sa, sizeof(sa)) == 0);
    int acc = accept(srv, NULL, NULL);
    CHECK(acc >= 0);
    char buf[8] = {0};
    CHECK(cli >= 0 && write(cli, "rel", 3) == 3);
    CHECK(acc >= 0 && read(acc, buf, sizeof(buf)) == 3 && memcmp(buf, "rel", 3) == 0);

    if (acc >= 0) close(acc);
    if (cli >= 0) close(cli);
    if (srv >= 0) close(srv);
    unlink(rel_name);
    if (saved_cwd[0]) {
        CHECK(chdir(saved_cwd) == 0);
    }
    case_end();
}

static void sock_abstract(void) {
    case_begin("sock.abstract");
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof(sa));
    sa.sun_family = AF_UNIX;
    int nlen = snprintf(sa.sun_path + 1, sizeof(sa.sun_path) - 1, "chromeprobe-abs-%d", (int)getpid());
    socklen_t alen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + (size_t)nlen);
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(srv >= 0 && bind(srv, (struct sockaddr *)&sa, alen) == 0 && listen(srv, 2) == 0);
    int other = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK_ERRNO(bind(other, (struct sockaddr *)&sa, alen), EADDRINUSE);
    close(other);
    int cli = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(cli, (struct sockaddr *)&sa, alen) == 0);
    int acc = accept(srv, NULL, NULL);
    CHECK(acc >= 0);
    char buf[8];
    CHECK(write(acc, "pong", 4) == 4 && read(cli, buf, sizeof(buf)) == 4 && memcmp(buf, "pong", 4) == 0);
    if (acc >= 0) {
        close(acc);
    }
    close(cli);
    close(srv);
    case_end();
}

/* Dump `type`, requiring an `lo` entry (by name attribute) and NLMSG_DONE. */
static void netlink_dump(const char *name, uint16_t req_type, uint16_t reply_type, uint16_t name_attr, size_t hdr_len) {
    case_begin(name);
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, CP_NETLINK_ROUTE);
    CHECK(fd >= 0);
    if (fd < 0) {
        case_end();
        return;
    }
    struct {
        struct cp_nlmsghdr h;
        struct cp_rtgenmsg g;
    } req;
    memset(&req, 0, sizeof(req));
    req.h.len = sizeof(req);
    req.h.type = req_type;
    req.h.flags = CP_NLM_F_REQUEST | CP_NLM_F_DUMP;
    req.h.seq = 1;
    req.g.family = AF_UNSPEC;
    CHECK(send(fd, &req, sizeof(req), 0) == (ssize_t)sizeof(req));
    static char buf[16384];
    bool done = false, saw_lo = false, saw_entry = false;
    for (int rounds = 0; rounds < 32 && !done; rounds++) {
        struct pollfd p = {.fd = fd, .events = POLLIN};
        if (poll(&p, 1, 3000) != 1) {
            break;
        }
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n <= 0) {
            break;
        }
        size_t off = 0;
        while (off + sizeof(struct cp_nlmsghdr) <= (size_t)n) {
            struct cp_nlmsghdr *h = (struct cp_nlmsghdr *)(buf + off);
            if (h->len < sizeof(*h) || off + h->len > (size_t)n) {
                break;
            }
            if (h->type == CP_NLMSG_DONE) {
                done = true;
                break;
            }
            if (h->type == CP_NLMSG_ERROR) {
                break;
            }
            if (h->type == reply_type) {
                saw_entry = true;
                size_t at = sizeof(*h) + CP_NLA_ALIGN(hdr_len);
                while (at + sizeof(struct cp_rtattr) <= off + h->len) {
                    struct cp_rtattr *ra = (struct cp_rtattr *)(buf + at);
                    if (ra->len < sizeof(*ra) || at + ra->len > off + h->len) {
                        break;
                    }
                    if (ra->type == name_attr && ra->len > sizeof(*ra) &&
                        strncmp(buf + at + sizeof(*ra), "lo", 3) == 0) {
                        saw_lo = true;
                    }
                    at += CP_NLA_ALIGN(ra->len);
                }
            }
            off += CP_NLA_ALIGN(h->len);
        }
    }
    CHECK(saw_entry);
    CHECK(saw_lo);
    CHECK(done);
    close(fd);
    case_end();
}

/* ---- memory and misc syscalls ----------------------------------------------- */

static void mem_mremap(void) {
    case_begin("mem.mremap");
    const size_t page = 4096;
    unsigned char *p = mmap(NULL, 4 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED);
    if (p == MAP_FAILED) {
        case_end();
        return;
    }
    for (size_t i = 0; i < 4 * page; i++) {
        p[i] = (unsigned char)(i * 7 + 1);
    }
    /* Pin a neighbour so growing in place is impossible and the move is real. */
    void *guard = mmap(p + 4 * page, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    unsigned char *q = mremap(p, 4 * page, 16 * page, MREMAP_MAYMOVE);
    CHECK(q != MAP_FAILED);
    if (q != MAP_FAILED) {
        bool same = true, zero = true;
        for (size_t i = 0; i < 4 * page; i++) {
            same &= q[i] == (unsigned char)(i * 7 + 1);
        }
        for (size_t i = 4 * page; i < 16 * page; i += 509) {
            zero &= q[i] == 0;
        }
        CHECK(same && zero);
        q[15 * page] = 9;
        unsigned char *s = mremap(q, 16 * page, 2 * page, 0);
        CHECK(s == q);
        bool kept = true;
        for (size_t i = 0; i < 2 * page; i++) {
            kept &= s[i] == (unsigned char)(i * 7 + 1);
        }
        CHECK(kept);
        /* MREMAP_FIXED onto a reserved window. */
        unsigned char *target = mmap(NULL, 2 * page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(target != MAP_FAILED);
        if (target != MAP_FAILED) {
            unsigned char *f = mremap(s, 2 * page, 2 * page, MREMAP_MAYMOVE | MREMAP_FIXED, target);
            CHECK(f == target);
            if (f == target) {
                bool moved = true;
                for (size_t i = 0; i < 2 * page; i++) {
                    moved &= f[i] == (unsigned char)(i * 7 + 1);
                }
                CHECK(moved);
                munmap(f, 2 * page);
            }
        }
    }
    if (guard != MAP_FAILED) {
        munmap(guard, page);
    }
    case_end();
}

static void mem_mincore(void) {
    case_begin("mem.mincore");
    const size_t page = 4096;
    unsigned char *p = mmap(NULL, 4 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED);
    if (p != MAP_FAILED) {
        p[0] = 1;
        p[page] = 1;
        unsigned char vec[4] = {0, 0, 0, 0};
        CHECK(mincore(p, 4 * page, vec) == 0);
        CHECK((vec[0] & 1) && (vec[1] & 1));
        munmap(p, 4 * page);
        CHECK_ERRNO(mincore(p, 4 * page, vec), ENOMEM);
    }
    case_end();
}

static void mem_mlock_msync(void) {
    case_begin("mem.mlock-msync");
    const size_t page = 4096;
    unsigned char *p = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED);
    if (p != MAP_FAILED) {
        CHECK(mlock(p, 2 * page) == 0);
        CHECK(munlock(p, 2 * page) == 0);
        CHECK(msync(p, 2 * page, MS_ASYNC) == 0);
        munmap(p, 2 * page);
    }
    const char *path = "/tmp/chromeprobe.msync";
    int fd = make_file(path, NULL);
    CHECK(fd >= 0 && ftruncate(fd, page) == 0);
    if (fd >= 0) {
        unsigned char *m = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        CHECK(m != MAP_FAILED);
        if (m != MAP_FAILED) {
            memcpy(m, "mapped-write", 12);
            CHECK(msync(m, page, MS_SYNC) == 0);
            char buf[16];
            CHECK(pread(fd, buf, 12, 0) == 12 && memcmp(buf, "mapped-write", 12) == 0);
            munmap(m, page);
        }
        close(fd);
    }
    unlink(path);
    case_end();
}

static void mem_vm_semantics(void) {
    case_begin("mem.vm-semantics");
    const size_t page = 4096;
    /* madvise(MADV_DONTNEED) zeroes anonymous private pages. */
    unsigned char *anon = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(anon != MAP_FAILED);
    if (anon != MAP_FAILED) {
        anon[0] = 0xAB;
        anon[page - 1] = 0xCD;
        CHECK(madvise(anon, page, MADV_DONTNEED) == 0);
        CHECK(anon[0] == 0 && anon[page - 1] == 0);
        munmap(anon, page);
    }

    /* fork child mremap of inherited PROT_NONE reservation succeeds. */
    void *res = mmap(NULL, 2 * page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(res != MAP_FAILED);
    if (res != MAP_FAILED) {
        pid_t child = fork();
        CHECK(child >= 0);
        if (child == 0) {
            void *grown = mremap(res, 2 * page, 4 * page, MREMAP_MAYMOVE);
            _exit(grown != MAP_FAILED ? 0 : 1);
        }
        int status = 0;
        CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        munmap(res, 2 * page);
    }

    /* Partial mlock over a range with an unmapped hole returns ENOMEM while locking mapped pages. */
    unsigned char *pair = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(pair != MAP_FAILED);
    if (pair != MAP_FAILED) {
        CHECK(munmap(pair + page, page) == 0);
        CHECK_ERRNO(mlock(pair, 3 * page), ENOMEM);
        char status_buf[8192];
        ssize_t sn = slurp("/proc/self/status", status_buf, sizeof(status_buf));
        char *vmlck = sn > 0 ? strstr(status_buf, "VmLck:") : NULL;
        CHECK(vmlck != NULL && atoi(vmlck + 6) > 0);
        munlock(pair, page);
        munmap(pair, page);
        munmap(pair + 2 * page, page);
    }
    case_end();
}

static void mem_truncate_sigbus_fat(void) {
    case_begin("mem.truncate-sigbus-fat");
    const size_t page = 4096;
    const char *tpath = "/tmp/chromeprobe.sigbus";
    int fd = make_file(tpath, NULL);
    CHECK(fd >= 0 && ftruncate(fd, (off_t)(2 * page)) == 0);
    if (fd >= 0) {
        volatile unsigned char *m = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        CHECK(m != MAP_FAILED);
        if (m != MAP_FAILED) {
            m[page] = 0x42;
            CHECK(ftruncate(fd, (off_t)page) == 0);
            pid_t child = fork();
            CHECK(child >= 0);
            if (child == 0) {
                (void)m[page];
                _exit(0);
            }
            int status = 0;
            CHECK(waitpid(child, &status, 0) == child && WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS);
            munmap((void *)m, 2 * page);
        }
        close(fd);
    }
    unlink(tpath);

    const char *dpath = "/data/chromeprobe/mapshared.bin";
    int dfd = make_file(dpath, NULL);
    CHECK(dfd >= 0 && ftruncate(dfd, (off_t)page) == 0);
    if (dfd >= 0) {
        unsigned char *dm = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, dfd, 0);
        CHECK(dm != MAP_FAILED);
        if (dm != MAP_FAILED) {
            memcpy(dm, "fat-map-shared", 14);
            CHECK(msync(dm, page, MS_SYNC) == 0);
            char rbuf[16] = {0};
            CHECK(pread(dfd, rbuf, 14, 0) == 14 && memcmp(rbuf, "fat-map-shared", 14) == 0);
            munmap(dm, page);
        }
        close(dfd);
    }
    unlink(dpath);
    case_end();
}

static void mem_membarrier(void) {
    case_begin("mem.membarrier");
    errno = 0;
    long r = syscall(SYS_membarrier, 0, 0, 0); /* QUERY: any result is accepted */
    (void)r;
    CHECK(true);
    case_end();
}

static void mem_sigtimedwait(void) {
    case_begin("mem.sigtimedwait");
    sigset_t set, old;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    CHECK(sigprocmask(SIG_BLOCK, &set, &old) == 0);
    struct timespec ts = {0, 50 * 1000000L};
    siginfo_t info;
    CHECK_ERRNO(sigtimedwait(&set, &info, &ts), EAGAIN);
    CHECK(kill(getpid(), SIGUSR1) == 0);
    CHECK(sigtimedwait(&set, &info, &ts) == SIGUSR1);
    sigprocmask(SIG_SETMASK, &old, NULL);
    case_end();
}

static void mem_rlimit_sched(void) {
    case_begin("mem.rlimit-sched");
    struct rlimit rl;
    CHECK(getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur > 0);
    CHECK(setrlimit(RLIMIT_NOFILE, &rl) == 0);
    struct rlimit rl2;
    CHECK(getrlimit(RLIMIT_NOFILE, &rl2) == 0 && rl2.rlim_cur == rl.rlim_cur);
    CHECK(getrlimit(RLIMIT_STACK, &rl) == 0);
    struct sched_param sp = {.sched_priority = 0};
    CHECK(sched_setscheduler(0, SCHED_OTHER, &sp) == 0);
    errno = 0;
    int nice = getpriority(PRIO_PROCESS, 0);
    CHECK(errno == 0);
    CHECK(setpriority(PRIO_PROCESS, 0, nice) == 0);
    case_end();
}

static void mem_mmsg(void) {
    case_begin("mem.mmsg");
    int sv[2];
    /* Exercise the batch APIs on the requested record-oriented transport. */
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0);
    char m1[] = "one", m2[] = "second";
    struct iovec iov[2] = {{m1, 3}, {m2, 6}};
    struct mmsghdr out[2];
    memset(out, 0, sizeof(out));
    for (int i = 0; i < 2; i++) {
        out[i].msg_hdr.msg_iov = &iov[i];
        out[i].msg_hdr.msg_iovlen = 1;
    }
    CHECK(sendmmsg(sv[0], out, 2, 0) == 2 && out[0].msg_len == 3 && out[1].msg_len == 6);
    char b1[16], b2[16];
    struct iovec riov[2] = {{b1, sizeof(b1)}, {b2, sizeof(b2)}};
    struct mmsghdr in[2];
    memset(in, 0, sizeof(in));
    for (int i = 0; i < 2; i++) {
        in[i].msg_hdr.msg_iov = &riov[i];
        in[i].msg_hdr.msg_iovlen = 1;
    }
    CHECK(recvmmsg(sv[1], in, 2, MSG_DONTWAIT, NULL) == 2);
    CHECK(in[0].msg_len == 3 && memcmp(b1, "one", 3) == 0);
    CHECK(in[1].msg_len == 6 && memcmp(b2, "second", 6) == 0);
    close(sv[0]);
    close(sv[1]);
    case_end();
}

static void mem_epoll_pwait2(void) {
    case_begin("mem.epoll-pwait2");
    int ep = epoll_create1(EPOLL_CLOEXEC);
    CHECK(ep >= 0);
    struct epoll_event ev;
    struct timespec ts = {0, 20 * 1000000L};
    errno = 0;
    long r = syscall(SYS_epoll_pwait2, ep, &ev, 1, &ts, NULL, 0);
    CHECK(r == 0);
    close(ep);
    case_end();
}

static void mem_sysinfo(void) {
    case_begin("mem.sysinfo");
    struct sysinfo si;
    CHECK(sysinfo(&si) == 0 && si.totalram > 0 && si.mem_unit > 0);
    case_end();
}

/* ---- /proc/self ------------------------------------------------------------ */

static void proc_maps(void) {
    case_begin("proc.maps");
    static char buf[65536];
    ssize_t n = slurp("/proc/self/maps", buf, sizeof(buf));
    CHECK(n > 0);
    CHECK(n > 0 && strstr(buf, "chromeprobe.elf") != NULL);
    CHECK(n > 0 && strstr(buf, "[stack]") != NULL);
    case_end();
}

static void proc_status(void) {
    case_begin("proc.status");
    static char buf[8192];
    ssize_t n = slurp("/proc/self/status", buf, sizeof(buf));
    CHECK(n > 0);
    char *pid = n > 0 ? strstr(buf, "Pid:") : NULL;
    CHECK(pid != NULL && atoi(pid + 4) == (int)getpid());
    CHECK(n > 0 && strstr(buf, "Threads:") != NULL);
    CHECK(n > 0 && strstr(buf, "FDSize:") != NULL);
    case_end();
}

static void proc_oom_score_adj(void) {
    case_begin("proc.oom-score-adj");
    char orig[32] = {0};
    ssize_t n = slurp("/proc/self/oom_score_adj", orig, sizeof(orig));
    CHECK(n > 0);
    int fd = open("/proc/self/oom_score_adj", O_WRONLY);
    CHECK(fd >= 0);
    if (fd >= 0) {
        CHECK(write(fd, "150", 3) == 3);
        char now[32] = {0};
        CHECK(slurp("/proc/self/oom_score_adj", now, sizeof(now)) > 0 && atoi(now) == 150);
        CHECK_ERRNO(write(fd, "-1001", 5), EINVAL);
        CHECK_ERRNO(write(fd, "1001", 4), EINVAL);
        if (n > 0) {
            CHECK(write(fd, orig, (size_t)n) == n);
        }
        close(fd);
    }
    case_end();
}

static void proc_stat(void) {
    case_begin("proc.stat");
    static char buf[2048];
    ssize_t n = slurp("/proc/self/stat", buf, sizeof(buf));
    CHECK(n > 0);
    if (n > 0) {
        CHECK(atoi(buf) == (int)getpid());
        char *open_paren = strchr(buf, '(');
        char *close_paren = strrchr(buf, ')');
        CHECK(open_paren != NULL && close_paren != NULL && close_paren > open_paren);
        if (close_paren) {
            char state = 0;
            int ppid = 0;
            CHECK(sscanf(close_paren + 1, " %c %d", &state, &ppid) == 2);
            CHECK(strchr("RSDZTtXI", state) != NULL && ppid >= 0);
        }
    }
    case_end();
}

static void proc_cmdline_environ(void) {
    case_begin("proc.cmdline-environ");
    static char buf[65536];
    ssize_t n = slurp("/proc/self/cmdline", buf, sizeof(buf));
    CHECK(n > 0);
    ssize_t e = slurp("/proc/self/environ", buf, sizeof(buf));
    CHECK(e >= 0);
    case_end();
}

static void proc_fd_dir(void) {
    case_begin("proc.fd-dir");
    int pfd[2];
    CHECK(pipe(pfd) == 0);
    DIR *d = opendir("/proc/self/fd");
    CHECK(d != NULL);
    bool saw_r = false, saw_w = false;
    if (d) {
        char rn[16], wn[16];
        snprintf(rn, sizeof(rn), "%d", pfd[0]);
        snprintf(wn, sizeof(wn), "%d", pfd[1]);
        struct dirent *de;
        while ((de = readdir(d)) != NULL) {
            saw_r |= strcmp(de->d_name, rn) == 0;
            saw_w |= strcmp(de->d_name, wn) == 0;
        }
        closedir(d);
    }
    CHECK(saw_r && saw_w);
    close(pfd[0]);
    close(pfd[1]);
    case_end();
}

static void proc_fd_readlink(void) {
    case_begin("proc.fd-readlink");
    char path[64], target[256];
    int sv[2], pfd[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0 && pipe(pfd) == 0);
    int mfd = (int)syscall(SYS_memfd_create, "chromeprobe-rl", 0U);
    int rfd = open("/proc/self/stat", O_RDONLY);
    int tfd = make_file("/tmp/chromeprobe.rl", "x");
    CHECK(mfd >= 0 && rfd >= 0 && tfd >= 0);
    snprintf(path, sizeof(path), "/proc/self/fd/%d", sv[0]);
    ssize_t n = readlink(path, target, sizeof(target) - 1);
    if (n > 0) target[n] = 0;
    CHECK(n > 0 && strncmp(target, "socket:[", 8) == 0);
    snprintf(path, sizeof(path), "/proc/self/fd/%d", pfd[0]);
    n = readlink(path, target, sizeof(target) - 1);
    if (n > 0) target[n] = 0;
    CHECK(n > 0 && strncmp(target, "pipe:[", 6) == 0);
    snprintf(path, sizeof(path), "/proc/self/fd/%d", mfd);
    n = readlink(path, target, sizeof(target) - 1);
    if (n > 0) target[n] = 0;
    CHECK(n > 0 && strstr(target, "memfd:chromeprobe-rl") != NULL);
    snprintf(path, sizeof(path), "/proc/self/fd/%d", tfd);
    n = readlink(path, target, sizeof(target) - 1);
    if (n > 0) target[n] = 0;
    CHECK(n > 0 && strcmp(target, "/tmp/chromeprobe.rl") == 0);
    close(sv[0]); close(sv[1]); close(pfd[0]); close(pfd[1]);
    if (mfd >= 0) close(mfd);
    if (rfd >= 0) close(rfd);
    if (tfd >= 0) close(tfd);
    unlink("/tmp/chromeprobe.rl");
    case_end();
}

static void proc_fd_memfd_seals(void) {
    case_begin("proc.fd-memfd-seals");
    int mfd = (int)syscall(SYS_memfd_create, "chromeprobe-seal", MFD_ALLOW_SEALING);
    CHECK(mfd >= 0);
    if (mfd >= 0) {
        CHECK(ftruncate(mfd, 4096) == 0 && write(mfd, "sealed", 6) == 6);
        int seals = 0x8 | 0x2 | 0x4; /* WRITE | SHRINK | GROW */
        CHECK(fcntl(mfd, F_ADD_SEALS, seals) == 0);
        CHECK(fcntl(mfd, F_GET_SEALS) == seals);
        char path[64];
        snprintf(path, sizeof(path), "/proc/self/fd/%d", mfd);
        int rfd = open(path, O_RDONLY);
        CHECK(rfd >= 0);
        if (rfd >= 0) {
            CHECK(fcntl(rfd, F_GET_SEALS) == seals);
            char buf[8];
            CHECK(pread(rfd, buf, 6, 0) == 6 && memcmp(buf, "sealed", 6) == 0);
            close(rfd);
        }
        errno = 0;
        int wfd = open(path, O_WRONLY);
        CHECK(wfd < 0 && (errno == EPERM || errno == EACCES));
        if (wfd >= 0) {
            close(wfd);
        }
        close(mfd);
    }
    case_end();
}

static void proc_mounts(void) {
    case_begin("proc.mounts");
    static char buf[16384];
    ssize_t n = slurp("/proc/mounts", buf, sizeof(buf));
    CHECK(n > 0);
    static const char *const mounts[] = {" / ", " /data ", " /tmp ", " /dev/shm ", " /run "};
    for (size_t i = 0; i < 5; i++) {
        bool found = n > 0 && strstr(buf, mounts[i]) != NULL;
        if (!found) {
            say("chromeprobe: proc.mounts FAIL missing mount%s", mounts[i]);
        }
        CHECK(found);
    }
    case_end();
}

/* ---- fd identity and copy ----------------------------------------------------- */

static void fdkind_fstat(void) {
    case_begin("fdkind.fstat");
    struct stat st;
    int sv[2], pfd[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0 && pipe(pfd) == 0);
    CHECK(fstat(sv[0], &st) == 0 && S_ISSOCK(st.st_mode));
    CHECK(fstat(pfd[0], &st) == 0 && S_ISFIFO(st.st_mode));
    CHECK(fstat(pfd[1], &st) == 0 && S_ISFIFO(st.st_mode));
    int s = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    CHECK(s >= 0 && fstat(s, &st) == 0 && S_ISSOCK(st.st_mode));
    int fd = make_file("/tmp/chromeprobe.id", "id");
    struct stat a, b;
    int d = fd >= 0 ? dup(fd) : -1;
    CHECK(d >= 0 && fstat(fd, &a) == 0 && fstat(d, &b) == 0);
    CHECK(a.st_ino == b.st_ino && a.st_dev == b.st_dev);
    if (d >= 0) close(d);
    if (fd >= 0) close(fd);
    unlink("/tmp/chromeprobe.id");
    close(sv[0]); close(sv[1]); close(pfd[0]); close(pfd[1]); close(s);
    case_end();
}

static void fdkind_memfd_ino(void) {
    case_begin("fdkind.memfd-ino");
    int m1 = (int)syscall(SYS_memfd_create, "chromeprobe-same", 0U);
    int m2 = (int)syscall(SYS_memfd_create, "chromeprobe-same", 0U);
    CHECK(m1 >= 0 && m2 >= 0);
    struct stat a, b;
    CHECK(fstat(m1, &a) == 0 && fstat(m2, &b) == 0);
    CHECK(a.st_ino != b.st_ino);
    CHECK(S_ISREG(a.st_mode));
    int d = dup(m1);
    struct stat c;
    CHECK(d >= 0 && fstat(d, &c) == 0 && c.st_ino == a.st_ino);
    if (d >= 0) close(d);
    if (m1 >= 0) close(m1);
    if (m2 >= 0) close(m2);
    case_end();
}

static void fdkind_opath_tmpfile(void) {
    case_begin("fdkind.opath-tmpfile");
    int fd = open("/tmp", O_PATH | O_DIRECTORY);
    CHECK(fd >= 0);
    if (fd >= 0) {
        struct stat st;
        char buf[4];
        CHECK(fstat(fd, &st) == 0 && S_ISDIR(st.st_mode));
        CHECK_ERRNO(read(fd, buf, sizeof(buf)), EBADF);
        close(fd);
    }
    errno = 0;
    int t = open("/tmp", O_TMPFILE | O_RDWR, 0600);
    if (t >= 0) {
        char buf[4];
        CHECK(write(t, "tmp!", 4) == 4 && pread(t, buf, 4, 0) == 4 && memcmp(buf, "tmp!", 4) == 0);
        close(t);
    } else {
        CHECK(errno == EOPNOTSUPP);
    }
    case_end();
}

static void fdkind_sendfile_copy(void) {
    case_begin("fdkind.sendfile-copyrange");
    const char *src = "/tmp/chromeprobe.src", *dst = "/tmp/chromeprobe.dst";
    static char payload[8192], back[8192];
    for (size_t i = 0; i < sizeof(payload); i++) {
        payload[i] = (char)('a' + i % 23);
    }
    int in = make_file(src, NULL);
    int out = make_file(dst, NULL);
    CHECK(in >= 0 && out >= 0);
    if (in >= 0 && out >= 0) {
        CHECK(write_all(in, payload, sizeof(payload)) == 0);
        off_t off = 100;
        ssize_t n = sendfile(out, in, &off, 4000);
        CHECK(n == 4000 && off == 4100);
        CHECK(pread(out, back, 4000, 0) == 4000 && memcmp(back, payload + 100, 4000) == 0);
        CHECK(ftruncate(out, 0) == 0);
        off_t in_off = 1000, out_off = 10;
        n = (ssize_t)syscall(SYS_copy_file_range, in, &in_off, out, &out_off, 3000UL, 0U);
        CHECK(n == 3000 && in_off == 4000 && out_off == 3010);
        CHECK(pread(out, back, 3000, 10) == 3000 && memcmp(back, payload + 1000, 3000) == 0);
    }
    if (in >= 0) close(in);
    if (out >= 0) close(out);
    unlink(src);
    unlink(dst);
    case_end();
}

static void fileops_extended(void) {
    case_begin("fileops.extended");
    const char *reg = "/tmp/chromeprobe.fops.reg";
    const char *fifo = "/tmp/chromeprobe.fops.fifo";
    const char *sockp = "/tmp/chromeprobe.fops.sock";
    const char *hard1 = "/tmp/chromeprobe.fops.hard1";
    const char *hard2 = "/tmp/chromeprobe.fops.hard2";
    unlink(reg); unlink(fifo); unlink(sockp); unlink(hard1); unlink(hard2);

    uid_t uid = getuid();
    gid_t gid = getgid();
    CHECK(mknod(reg, S_IFREG | 0600, 0) == 0);
    CHECK(mknodat(AT_FDCWD, fifo, S_IFIFO | 0600, 0) == 0);
    CHECK(mknodat(AT_FDCWD, sockp, S_IFSOCK | 0600, 0) == 0);

    struct stat st;
    CHECK(stat(reg, &st) == 0 && S_ISREG(st.st_mode));
    CHECK(stat(fifo, &st) == 0 && S_ISFIFO(st.st_mode));
    CHECK(stat(sockp, &st) == 0 && S_ISSOCK(st.st_mode));

    CHECK(chown(reg, uid, gid) == 0);
    CHECK(lchown(reg, uid, gid) == 0);
    CHECK(fchownat(AT_FDCWD, reg, uid, gid, AT_SYMLINK_NOFOLLOW) == 0);

    struct timespec ts[2] = {
        { .tv_sec = 1700000000L, .tv_nsec = 123456789L },
        { .tv_sec = 1700000001L, .tv_nsec = 987654321L },
    };
    CHECK(utimensat(AT_FDCWD, reg, ts, 0) == 0);
    CHECK(stat(reg, &st) == 0 && st.st_atim.tv_sec == 1700000000L && st.st_atim.tv_nsec == 123456789L);
    CHECK(st.st_mtim.tv_sec == 1700000001L && st.st_mtim.tv_nsec == 987654321L);

    int rfd = open(reg, O_RDWR);
    CHECK(rfd >= 0);
    if (rfd >= 0) {
        CHECK(fchown(rfd, uid, gid) == 0);
        CHECK(write_all(rfd, "hello-sendfile", 14) == 0);

        int pfd[2];
        CHECK(pipe(pfd) == 0);
        off_t off = 0;
        char buf[16] = {0};
        CHECK(sendfile(pfd[1], rfd, &off, 14) == 14 && off == 14);
        CHECK(read(pfd[0], buf, 14) == 14 && memcmp(buf, "hello-sendfile", 14) == 0);
        close(pfd[0]);
        close(pfd[1]);

        int sv[2];
        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
        off = 0;
        memset(buf, 0, sizeof(buf));
        CHECK(sendfile(sv[0], rfd, &off, 14) == 14 && off == 14);
        CHECK(read(sv[1], buf, 14) == 14 && memcmp(buf, "hello-sendfile", 14) == 0);
        close(sv[0]);
        close(sv[1]);
        close(rfd);
    }

    CHECK(link(reg, hard1) == 0);
    CHECK(linkat(AT_FDCWD, reg, AT_FDCWD, hard2, 0) == 0);
    struct stat st1, st2;
    CHECK(stat(hard1, &st1) == 0 && stat(hard2, &st2) == 0 && st1.st_ino == st2.st_ino && st1.st_nlink >= 3);

    unlink(reg); unlink(fifo); unlink(sockp); unlink(hard1); unlink(hard2);
    case_end();
}

/* A 24-byte destination fits exactly one of our single-letter entries. */
static int next_test_dirent(int fd, char *name, off_t *cookie) {
    unsigned char bytes[24];
    for (;;) {
        long n = syscall(SYS_getdents64, fd, bytes, sizeof(bytes));
        if (n <= 0) return (int)n;
        uint16_t reclen;
        int64_t offset;
        if (n != sizeof(bytes)) return -1;
        memcpy(&reclen, bytes + 16, sizeof(reclen));
        memcpy(&offset, bytes + 8, sizeof(offset));
        if (reclen != sizeof(bytes) || offset <= 0 || bytes[20] != 0) {
            if (bytes[19] == '.' && bytes[20] == '.' && bytes[21] == 0) continue;
            return -1;
        }
        if (bytes[19] == '.') continue;
        *name = bytes[19];
        *cookie = (off_t)offset;
        return 1;
    }
}

static void directory_cookie_mutation(const char *test, const char *path) {
    case_begin(test);
    CHECK(mkdir(path, 0700) == 0);
    int fd = open(path, O_RDONLY | O_DIRECTORY);
    CHECK(fd >= 0);
    if (fd >= 0) {
        for (char name = 'a'; name <= 'd'; ++name) {
            char entry[] = {name, 0};
            int file = openat(fd, entry, O_WRONLY | O_CREAT | O_EXCL, 0600);
            CHECK(file >= 0);
            if (file >= 0) close(file);
        }
        char first = 0, second = 0, resumed = 0, replay = 0;
        off_t first_cookie = 0, second_cookie = 0, after = 0;
        CHECK(next_test_dirent(fd, &first, &first_cookie) == 1);
        CHECK(next_test_dirent(fd, &second, &second_cookie) == 1);
        CHECK(first != second && first != 0 && second != 0);
        char removed[] = {first, 0};
        CHECK(unlinkat(fd, removed, 0) == 0);
        CHECK(next_test_dirent(fd, &resumed, &after) == 1);
        CHECK(resumed != first && resumed != second && resumed != 0);
        CHECK(lseek(fd, second_cookie, SEEK_SET) == second_cookie);
        CHECK(next_test_dirent(fd, &replay, &after) == 1 && replay == resumed);
        for (char name = 'a'; name <= 'd'; ++name) {
            char entry[] = {name, 0};
            if (name != first) CHECK(unlinkat(fd, entry, 0) == 0);
        }
        close(fd);
    }
    CHECK(rmdir(path) == 0);
    case_end();
}

static bool dir_lists_once(const char *path, const char *name) {
    DIR *d = opendir(path);
    if (!d) return false;
    int count = 0;
    bool is_dir = false;
    struct dirent *entry;
    errno = 0;
    while ((entry = readdir(d)) != NULL) {
        if (strcmp(entry->d_name, name) == 0) {
            ++count;
            is_dir = entry->d_type == DT_DIR;
        }
    }
    bool ok = errno == 0;
    closedir(d);
    return ok && count == 1 && is_dir;
}

static void directory_mount_overlay(void) {
    case_begin("dir.mount-overlay");
    CHECK(dir_lists_once("/", "data"));
    CHECK(dir_lists_once("/", "tmp"));
    CHECK(dir_lists_once("/dev", "shm"));
    case_end();
}

static void overlong_path_errno(void) {
    case_begin("dir.name-too-long");
    char path[630];
    memset(path, 'x', sizeof(path));
    memcpy(path, "/tmp/", 5);
    path[sizeof(path) - 1] = 0;
    struct stat st;
    errno = 0;
    CHECK(stat(path, &st) == -1 && errno == ENAMETOOLONG);
    path[5 + 256] = 0;
    errno = 0;
    CHECK(stat(path, &st) == -1 && errno == ENAMETOOLONG);
    case_end();
}

/* ---- driver --------------------------------------------------------------------- */

static void wait_for_data(void) {
    for (int i = 0; i < DATA_WAIT_S * 2; i++) {
        struct stat st;
        if (stat("/data/home", &st) == 0 && stat("/tmp", &st) == 0) {
            return;
        }
        msleep(500);
    }
    say("chromeprobe: setup FAIL /data/home never appeared (errno=%d)", errno);
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "--exec-child") == 0) {
        return 0;
    }
    /* Session-startup programs run in every boot that reaches ui-ready, so the
     * probe runs only when `kvm-smoke --chrome-compat-probe` wrote the private
     * contract; without it the program exits at once, silently. Any argument
     * forces a run (manual use). */
    int armed = contract_present();
    if (argc < 2 && !armed) {
        return 0;
    }
    if (argc < 2) {
        msleep(START_DELAY_S * 1000L);
    }
    signal(SIGPIPE, SIG_IGN);
    say("chromeprobe: start");
    wait_for_data();

    tmpfs_suite("tmpfs.tmp", "/tmp");
    tmpfs_suite("tmpfs.devshm", "/dev/shm");
    tmpfs_suite("tmpfs.run", "/run/user/1000");
    persist_check();
    data_dirs();
    data_atomic_write();
    directory_cookie_mutation("dir.tmpfs-cookie", "/tmp/chromeprobe-cookies");
    directory_cookie_mutation("dir.fat-cookie", "/data/chromeprobe/cookies");
    directory_mount_overlay();
    overlong_path_errno();
    posix_lock_suite("lock.posix-tmpfs", "/tmp/chromeprobe.plock");
    posix_lock_suite("lock.posix-data", "/data/chromeprobe/plock");
    lock_ofd();
    lock_flock();
    lock_autorelease();
    lock_exec_cloexec(argv[0]);
    sock_seqpacket();
    sock_scm_rights();
    sock_unix_path();
    sock_unix_rel_umask();
    sock_abstract();
    netlink_dump("sock.netlink-link", CP_RTM_GETLINK, CP_RTM_NEWLINK, CP_IFLA_IFNAME, sizeof(struct cp_ifinfomsg));
    netlink_dump("sock.netlink-addr", CP_RTM_GETADDR, CP_RTM_NEWADDR, CP_IFA_LABEL, sizeof(struct cp_ifaddrmsg));
    mem_mremap();
    mem_mincore();
    mem_mlock_msync();
    mem_vm_semantics();
    mem_truncate_sigbus_fat();
    mem_membarrier();
    mem_sigtimedwait();
    mem_rlimit_sched();
    mem_mmsg();
    mem_epoll_pwait2();
    mem_sysinfo();
    proc_maps();
    proc_status();
    proc_stat();
    proc_oom_score_adj();
    proc_cmdline_environ();
    proc_fd_dir();
    proc_fd_readlink();
    proc_fd_memfd_seals();
    proc_mounts();
    fdkind_fstat();
    fdkind_memfd_ino();
    fdkind_opath_tmpfile();
    fdkind_sendfile_copy();
    fileops_extended();

    if (failures == 0) {
        say("chromeprobe: PASS (%d checks, 0 failures)", checks);
        return 0;
    }
    say("chromeprobe: FAIL %d of %d checks failed; failing cases:%s", failures, checks,
        failed_cases[0] ? failed_cases : " (persist)");
    return 1;
}
