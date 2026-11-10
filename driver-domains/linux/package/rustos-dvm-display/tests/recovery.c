#define main display_relay_main
#include "../src/rustos-dvm-display.c"
#undef main
#include <assert.h>
#include <sys/wait.h>
#include <sys/prctl.h>

static void test_deadlines(void)
{
    uint64_t deadline, remaining;
    assert(gpu_deadline_after(100, 500, &deadline) == 0 && deadline == 600);
    assert(gpu_deadline_remaining(250, deadline, &remaining) == 0 && remaining == 350);
    assert(gpu_deadline_min(deadline, 400) == 400);
    assert(gpu_deadline_remaining(600, deadline, &remaining) == -1 && errno == ETIMEDOUT);
    assert(gpu_deadline_after(UINT64_MAX, 1, &deadline) == -1 && errno == EOVERFLOW);
}

static void test_fence_deadline(void)
{
    int fds[2];
    uint64_t now, deadline;
    char byte = 'x';
    assert(pipe(fds) == 0);
    assert(monotonic_time_ns(&now) == 0);
    assert(gpu_deadline_after(now, 1000000000ULL, &deadline) == 0);
    assert(write(fds[1], &byte, 1) == 1);
    assert(wait_sync_file_until(fds[0], deadline) == 0);
    assert(wait_sync_file_until(fds[0], now) == -1 && errno == ETIMEDOUT);
    close(fds[0]);
    close(fds[1]);
}

static void test_invitation(void)
{
    uint8_t page[4096] = {0};
    struct shared_display shared = {.base = page};
    recovery_generation = 0;
    recovery_attempt = 3;
    bind_recovery_generation(&shared);
    assert(recovery_generation == 0 && recovery_attempt == 3);
    write_le64(page + GUI_POOL_INVITATION_OFFSET, 8);
    bind_recovery_generation(&shared);
    assert(recovery_generation == 8 && recovery_attempt == 1);
    recovery_attempt = 4;
    bind_recovery_generation(&shared);
    assert(recovery_attempt == 4);
    write_le64(page + GUI_POOL_INVITATION_OFFSET, 10);
    bind_recovery_generation(&shared);
    assert(recovery_generation == 10 && recovery_attempt == 1);
}

static void test_marker_fields(void)
{
    int pipefd[2], saved;
    char output[512];
    ssize_t length;
    assert(pipe(pipefd) == 0);
    saved = dup(STDERR_FILENO);
    assert(saved >= 0 && dup2(pipefd[1], STDERR_FILENO) >= 0);
    close(pipefd[1]);
    if (relay_log_fd >= 0)
        close(relay_log_fd);
    relay_log_fd = -2;
    recovery_generation = 42;
    recovery_attempt = 3;
    recovery_started_ms = relay_log_now_ms();
    recovery_marker("ready", "frame-presented", 0, 1, 12);
    if (relay_log_fd >= 0)
        close(relay_log_fd);
    relay_log_fd = -2;
    assert(dup2(saved, STDERR_FILENO) >= 0);
    close(saved);
    length = read(pipefd[0], output, sizeof(output) - 1);
    assert(length > 0);
    output[length] = '\0';
    assert(strstr(output, "recovery v=1 generation=42 attempt=3 state=ready cause=frame-presented errno=0") != NULL);
    assert(strstr(output, "frames=1 sequence=12") != NULL);
    close(pipefd[0]);
}

static void test_watchdog_stops_and_retains_process(void)
{
    pid_t parent = getpid();
    pid_t child = fork();
    int status;
    assert(child >= 0);
    if (child == 0) {
        assert(prctl(PR_SET_PDEATHSIG, SIGKILL) == 0);
        if (getppid() != parent) _exit(1);
        struct shared_display shared = {0};
        struct gpu_watchdog watchdog = {.shared = &shared};
        struct timespec now;
        uint64_t deadline;
        recovery_generation = 8;
        recovery_attempt = 2;
        recovery_started_ms = relay_log_now_ms();
        assert(pthread_create(&watchdog.thread, NULL, gpu_watchdog_main, &watchdog) == 0);
        active_watchdog = &watchdog;
        assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
        deadline = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec + 30000000ULL;
        gpu_watchdog_arm(deadline);
        for (;;) pause();
    }
    assert(waitpid(child, &status, WUNTRACED) == child);
    assert(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
    kill(child, SIGKILL);
    assert(waitpid(child, &status, 0) == child && WIFSIGNALED(status));
}

int main(void)
{
    test_deadlines();
    test_fence_deadline();
    test_invitation();
    test_marker_fields();
    test_watchdog_stops_and_retains_process();
    puts("GPU recovery tests passed");
    return 0;
}
