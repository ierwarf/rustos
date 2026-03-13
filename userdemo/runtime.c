#include "runtime.h"

static inline long syscall0(long number) {
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number)
        : "rcx", "r11", "memory");

    return result;
}

static inline long syscall1(long number, long arg0) {
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0)
        : "rcx", "r11", "memory");

    return result;
}

static inline long syscall2(long number, long arg0, long arg1) {
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0), "S"(arg1)
        : "rcx", "r11", "memory");

    return result;
}

long console_write(const void *buffer, size_t len) {
    return syscall2(
        USERDEMO_SYSCALL_CONSOLE_WRITE,
        (long)(uintptr_t)buffer,
        (long)len);
}

long console_read(void *buffer, size_t len) {
    return syscall2(
        USERDEMO_SYSCALL_CONSOLE_READ,
        (long)(uintptr_t)buffer,
        (long)len);
}

long console_poll_input(void) {
    return syscall0(USERDEMO_SYSCALL_CONSOLE_POLL_INPUT);
}

void sleep_ms(uint64_t milliseconds) {
    (void)syscall1(USERDEMO_SYSCALL_SLEEP_MS, (long)milliseconds);
}

_Noreturn void exit(int status) {
    (void)syscall1(USERDEMO_SYSCALL_PROCESS_EXIT, (long)status);

    for (;;) {
        __asm__ volatile("hlt");
    }
}

long write(int fd, const void *buffer, size_t len) {
    if (fd != 1 && fd != 2) {
        return USERDEMO_SYSCALL_ERR_INVALID;
    }

    return console_write(buffer, len);
}

long read(int fd, void *buffer, size_t len) {
    if (fd != 0) {
        return USERDEMO_SYSCALL_ERR_INVALID;
    }

    return console_read(buffer, len);
}

int puts(const char *text) {
    size_t len = strlen(text);

    if (write(1, text, len) < 0) {
        return -1;
    }
    if (write(1, "\n", 1) < 0) {
        return -1;
    }

    return 0;
}
