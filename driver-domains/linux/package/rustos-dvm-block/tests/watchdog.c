#define READY_DIR "/dev/null/rustos-dvm"
#define main block_relay_main
#include "../src/rustos-dvm-block.c"
#undef main
#include <assert.h>
#include <sys/prctl.h>

static void run_case(bool send_begin)
{
    struct relay_state state = {0};
    int events[2], doorbell[2], lock, contender, status;
    char path[] = "/tmp/rustos-block-watchdog-XXXXXX";
    pid_t worker, supervisor, parent = getpid();
    struct watchdog_event event = {
        .request_id = 9, .operation = DVM_BLOCK_OP_WRITE, .begin = 1U,
    };
    struct timespec delay = {.tv_nsec = 10000000};
    unsigned int i;

    state.transport.base = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    state.quarantined = mmap(NULL, sizeof(*state.quarantined), PROT_READ | PROT_WRITE,
                             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    assert(state.transport.base != MAP_FAILED && state.quarantined != MAP_FAILED);
    atomic_init(state.quarantined, false);
    state.header.generation = 7;
    state.ready_published = true;
    fetch_or_flags(state.transport.base, DVM_BLOCK_FLAG_DVM_READY);
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, events) == 0 && pipe(doorbell) == 0);
    state.transport.uio_fd = doorbell[1];
    lock = mkstemp(path);
    assert(lock >= 0);
    contender = open(path, O_RDWR);
    assert(contender >= 0 && unlink(path) == 0 && flock(lock, LOCK_EX | LOCK_NB) == 0);
    worker = fork();
    assert(worker >= 0);
    if (worker == 0) {
        assert(prctl(PR_SET_PDEATHSIG, SIGKILL) == 0);
        if (getppid() != parent) _exit(1);
        close(events[0]);
        close(events[1]);
        for (;;) pause();
    }
    supervisor = fork();
    assert(supervisor >= 0);
    if (supervisor == 0) {
        assert(prctl(PR_SET_PDEATHSIG, SIGKILL) == 0);
        if (getppid() != parent) _exit(1);
        close(events[1]);
        supervise_worker(&state, events[0], worker, 40);
        _exit(2);
    }
    close(lock);
    close(events[0]);
    if (send_begin)
        assert(watchdog_event_send(events[1], &event) == 0);
    else
        close(events[1]);
    for (i = 0; i < 200 && !atomic_load(state.quarantined); i++)
        nanosleep(&delay, NULL);
    assert(atomic_load(state.quarantined));
    if (send_begin)
        close(events[1]);
    for (i = 0; i < 200 && (fetch_or_flags(state.transport.base, 0) & DVM_BLOCK_FLAG_DVM_READY); i++)
        nanosleep(&delay, NULL);
    assert(!(fetch_or_flags(state.transport.base, 0) & DVM_BLOCK_FLAG_DVM_READY));
    assert(flock(contender, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
    assert(waitpid(supervisor, &status, WNOHANG) == 0);
    kill(supervisor, SIGKILL);
    waitpid(supervisor, &status, 0);
    kill(worker, SIGKILL);
    waitpid(worker, &status, 0);
    close(contender); close(doorbell[0]); close(doorbell[1]);
    munmap((void *)state.transport.base, 4096);
    munmap(state.quarantined, sizeof(*state.quarantined));
}

int main(void)
{
    run_case(true);
    run_case(false);
    puts("block watchdog tests passed");
    return 0;
}
