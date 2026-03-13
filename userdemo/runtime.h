#ifndef USERDEMO_RUNTIME_H
#define USERDEMO_RUNTIME_H

#include <stddef.h>
#include <stdint.h>

#define USERDEMO_BAD_USER_PTR 0x8000200000ULL

enum userdemo_syscall_number {
    USERDEMO_SYSCALL_CONSOLE_WRITE = 1,
    USERDEMO_SYSCALL_CONSOLE_READ = 2,
    USERDEMO_SYSCALL_CONSOLE_POLL_INPUT = 3,
    USERDEMO_SYSCALL_SLEEP_MS = 4,
    USERDEMO_SYSCALL_PROCESS_EXIT = 5,
};

enum userdemo_syscall_error {
    USERDEMO_SYSCALL_ERR_INVALID = -1,
    USERDEMO_SYSCALL_ERR_FAULT = -2,
};

long console_write(const void *buffer, size_t len);
long console_read(void *buffer, size_t len);
long console_poll_input(void);
void sleep_ms(uint64_t milliseconds);
_Noreturn void exit(int status);

long write(int fd, const void *buffer, size_t len);
long read(int fd, void *buffer, size_t len);
int puts(const char *text);

void *memcpy(void *dest, const void *src, size_t len);
void *memmove(void *dest, const void *src, size_t len);
void *memset(void *dest, int value, size_t len);
int memcmp(const void *lhs, const void *rhs, size_t len);
size_t strlen(const char *text);

#endif
