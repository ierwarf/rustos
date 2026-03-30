#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char **environ;

#define EXECSMOKE_PATH "/samples/execsmoke/execsmoke.elf"
#define EXECSMOKE_DIR "/samples/execsmoke"

static void log_line(const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    vprintf(fmt, args);
    va_end(args);
    printf("\r\n");
    fflush(stdout);
}

static void set_env_int(const char *name, int value) {
    char buffer[32];
    snprintf(buffer, sizeof(buffer), "%d", value);
    setenv(name, buffer, 1);
}

static int env_int(const char *name, int fallback) {
    const char *value = getenv(name);
    if (value == NULL || value[0] == '\0') {
        return fallback;
    }
    return atoi(value);
}

static void custom_signal_handler(int signo) {
    (void)signo;
}

static int configure_pre_exec_state(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = custom_signal_handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        log_line("exec smoke: sigaction(SIGUSR1) failed errno=%d", errno);
        return 1;
    }

    memset(&action, 0, sizeof(action));
    action.sa_handler = SIG_IGN;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL) != 0) {
        log_line("exec smoke: sigaction(SIGUSR2) failed errno=%d", errno);
        return 1;
    }

    setenv("EXECSMOKE_MARKER", "env-ok", 1);
    return 0;
}

static int prepare_fd_state(void) {
    int keep_fd = open(EXECSMOKE_PATH, O_RDONLY);
    if (keep_fd < 0) {
        log_line("exec smoke: open keep fd failed errno=%d", errno);
        return 1;
    }
    int cloexec_fd = open(EXECSMOKE_PATH, O_RDONLY);
    if (cloexec_fd < 0) {
        log_line("exec smoke: open cloexec fd failed errno=%d", errno);
        return 1;
    }
    if (fcntl(cloexec_fd, F_SETFD, FD_CLOEXEC) != 0) {
        log_line("exec smoke: F_SETFD cloexec failed errno=%d", errno);
        return 1;
    }

    set_env_int("EXECSMOKE_KEEP_FD", keep_fd);
    set_env_int("EXECSMOKE_DROP_FD", cloexec_fd);
    return 0;
}

static int inspect_signal_handler(int signo, bool *is_default, bool *is_ignored) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    if (sigaction(signo, NULL, &action) != 0) {
        return errno;
    }

    *is_default = action.sa_handler == SIG_DFL;
    *is_ignored = action.sa_handler == SIG_IGN;
    return 0;
}

static int verify_fd_state(const char *label, int fd, bool expect_open) {
    errno = 0;
    int flags = fcntl(fd, F_GETFD);
    if (expect_open) {
        if (flags < 0) {
            log_line("exec smoke: %s fd=%d unexpectedly closed errno=%d", label, fd, errno);
            return 1;
        }
        log_line("exec smoke: %s fd=%d open flags=%d", label, fd, flags);
        return 0;
    }

    if (flags >= 0 || errno != EBADF) {
        log_line(
            "exec smoke: %s fd=%d expected EBADF got flags=%d errno=%d",
            label,
            fd,
            flags,
            errno
        );
        return 1;
    }
    log_line("exec smoke: %s fd=%d closed as expected", label, fd);
    return 0;
}

static int verify_post_exec(int argc, char **argv) {
    const char *mode = getenv("EXECSMOKE_MODE");
    if (mode == NULL) {
        log_line("exec smoke: missing EXECSMOKE_MODE");
        return 1;
    }

    int status = 0;
    int expected_argc = env_int("EXECSMOKE_EXPECT_ARGC", -1);
    if (expected_argc >= 0 && argc != expected_argc) {
        log_line("exec smoke: argc mismatch got=%d expected=%d", argc, expected_argc);
        status = 1;
    }

    const char *marker = getenv("EXECSMOKE_MARKER");
    if (marker == NULL || strcmp(marker, "env-ok") != 0) {
        log_line("exec smoke: env marker missing");
        status = 1;
    }

    pid_t pid = getpid();
    pid_t tid = (pid_t)syscall(SYS_gettid);
    log_line("exec smoke: mode=%s pid=%ld tid=%ld argc=%d", mode, (long)pid, (long)tid, argc);

    if (strcmp(mode, "post-empty") == 0) {
        if (argc != 0) {
            log_line("exec smoke: empty argv expected argc=0 got=%d", argc);
            status = 1;
        }
    } else {
        if (argc < 2 || argv == NULL || argv[1] == NULL) {
            log_line("exec smoke: argv[1] missing after exec");
            status = 1;
        } else {
            log_line("exec smoke: argv0=%s argv1=%s", argv[0], argv[1]);
        }
    }

    if (pid != tid) {
        log_line("exec smoke: getpid/gettid mismatch pid=%ld tid=%ld", (long)pid, (long)tid);
        status = 1;
    }

    int keep_fd = env_int("EXECSMOKE_KEEP_FD", -1);
    int drop_fd = env_int("EXECSMOKE_DROP_FD", -1);
    if (keep_fd >= 0) {
        status |= verify_fd_state("keep", keep_fd, true);
    }
    if (drop_fd >= 0) {
        status |= verify_fd_state("cloexec", drop_fd, false);
    }

    bool is_default = false;
    bool is_ignored = false;
    int signal_error = inspect_signal_handler(SIGUSR1, &is_default, &is_ignored);
    if (signal_error != 0 || !is_default || is_ignored) {
        log_line(
            "exec smoke: SIGUSR1 handler not reset default=%d ignored=%d errno=%d",
            is_default,
            is_ignored,
            signal_error
        );
        status = 1;
    } else {
        log_line("exec smoke: SIGUSR1 reset to default");
    }

    is_default = false;
    is_ignored = false;
    signal_error = inspect_signal_handler(SIGUSR2, &is_default, &is_ignored);
    if (signal_error != 0 || !is_ignored) {
        log_line(
            "exec smoke: SIGUSR2 ignore not preserved default=%d ignored=%d errno=%d",
            is_default,
            is_ignored,
            signal_error
        );
        status = 1;
    } else {
        log_line("exec smoke: SIGUSR2 ignore preserved");
    }

    log_line("exec smoke: result=%s", status == 0 ? "ok" : "fail");
    return status;
}

static int run_execve_mode(const char *post_mode) {
    if (configure_pre_exec_state() != 0 || prepare_fd_state() != 0) {
        return 1;
    }

    setenv("EXECSMOKE_MODE", post_mode, 1);
    set_env_int("EXECSMOKE_EXPECT_ARGC", 2);

    char *const argv[] = {
        (char *)EXECSMOKE_PATH,
        (char *)post_mode,
        NULL,
    };
    execve(EXECSMOKE_PATH, argv, environ);
    log_line("exec smoke: execve failed errno=%d", errno);
    return 1;
}

static int run_execveat_mode(void) {
    if (configure_pre_exec_state() != 0 || prepare_fd_state() != 0) {
        return 1;
    }

    int dirfd = open(EXECSMOKE_DIR, O_RDONLY | O_DIRECTORY);
    if (dirfd < 0) {
        log_line("exec smoke: open exec dir failed errno=%d", errno);
        return 1;
    }

    setenv("EXECSMOKE_MODE", "post-execveat", 1);
    set_env_int("EXECSMOKE_EXPECT_ARGC", 2);

    char *const argv[] = {
        (char *)EXECSMOKE_PATH,
        (char *)"post-execveat",
        NULL,
    };
    execveat(dirfd, "execsmoke.elf", argv, environ, 0);
    log_line("exec smoke: execveat failed errno=%d", errno);
    return 1;
}

static void *thread_exec_entry(void *unused) {
    (void)unused;
    run_execve_mode("post-thread");
    return (void *)(intptr_t)1;
}

static int run_thread_exec_mode(void) {
    pthread_t thread;
    int rc = pthread_create(&thread, NULL, thread_exec_entry, NULL);
    if (rc != 0) {
        log_line("exec smoke: pthread_create failed rc=%d", rc);
        return 1;
    }

    void *result = NULL;
    rc = pthread_join(thread, &result);
    if (rc != 0) {
        log_line("exec smoke: pthread_join failed rc=%d", rc);
        return 1;
    }

    log_line("exec smoke: thread returned without exec result=%ld", (long)(intptr_t)result);
    return 1;
}

static int run_empty_argv_mode(void) {
    if (configure_pre_exec_state() != 0 || prepare_fd_state() != 0) {
        return 1;
    }

    setenv("EXECSMOKE_MODE", "post-empty", 1);
    set_env_int("EXECSMOKE_EXPECT_ARGC", 0);

    char *const argv[] = {
        NULL,
    };
    long rc = syscall(SYS_execve, EXECSMOKE_PATH, argv, environ);
    (void)rc;
    log_line("exec smoke: raw execve(empty argv) failed errno=%d", errno);
    return 1;
}

int main(int argc, char **argv) {
    const char *post_mode = getenv("EXECSMOKE_MODE");
    if (post_mode != NULL && strncmp(post_mode, "post-", 5) == 0) {
        return verify_post_exec(argc, argv);
    }

    if (argc < 2) {
        log_line("exec smoke: usage: execve | execveat | thread-execve | empty-argv");
        return 1;
    }

    if (strcmp(argv[1], "execve") == 0) {
        return run_execve_mode("post-execve");
    }
    if (strcmp(argv[1], "execveat") == 0) {
        return run_execveat_mode();
    }
    if (strcmp(argv[1], "thread-execve") == 0) {
        return run_thread_exec_mode();
    }
    if (strcmp(argv[1], "empty-argv") == 0) {
        return run_empty_argv_mode();
    }

    log_line("exec smoke: unknown mode=%s", argv[1]);
    return 1;
}
