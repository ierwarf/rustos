#ifndef RUSTOS_LINUX_H
#define RUSTOS_LINUX_H

#include <stddef.h>
#include <stdint.h>

enum
{
    RUSTOS_LINUX_SYS_READ = 0,
    RUSTOS_LINUX_SYS_CLOSE = 3,
    RUSTOS_LINUX_SYS_MMAP = 9,
    RUSTOS_LINUX_SYS_MUNMAP = 11,
    RUSTOS_LINUX_SYS_IOCTL = 16,
    RUSTOS_LINUX_SYS_OPENAT = 257,
};

enum
{
    RUSTOS_AT_FDCWD = -100,
};

enum
{
    RUSTOS_O_RDONLY = 0,
    RUSTOS_O_WRONLY = 1,
    RUSTOS_O_RDWR = 2,
};

enum
{
    RUSTOS_PROT_READ = 0x1,
    RUSTOS_PROT_WRITE = 0x2,
    RUSTOS_PROT_EXEC = 0x4,
};

enum
{
    RUSTOS_MAP_SHARED = 0x01,
    RUSTOS_MAP_PRIVATE = 0x02,
    RUSTOS_MAP_ANONYMOUS = 0x20,
};

enum
{
    RUSTOS_LINUX_EIO = 5,
};

static inline long rustos_syscall1(long number, long arg0)
{
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0)
        : "rcx", "r11", "memory");

    return result;
}

static inline long rustos_syscall2(long number, long arg0, long arg1)
{
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0), "S"(arg1)
        : "rcx", "r11", "memory");

    return result;
}

static inline long rustos_syscall3(long number, long arg0, long arg1, long arg2)
{
    long result;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0), "S"(arg1), "d"(arg2)
        : "rcx", "r11", "memory");

    return result;
}

static inline long rustos_syscall4(long number, long arg0, long arg1, long arg2, long arg3)
{
    long result;
    register long r10 __asm__("r10") = arg3;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0), "S"(arg1), "d"(arg2), "r"(r10)
        : "rcx", "r11", "memory");

    return result;
}

static inline long rustos_syscall6(
    long number,
    long arg0,
    long arg1,
    long arg2,
    long arg3,
    long arg4,
    long arg5)
{
    long result;
    register long r10 __asm__("r10") = arg3;
    register long r8 __asm__("r8") = arg4;
    register long r9 __asm__("r9") = arg5;

    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(arg0), "S"(arg1), "d"(arg2), "r"(r10), "r"(r8), "r"(r9)
        : "rcx", "r11", "memory");

    return result;
}

static inline int rustos_syscall_failed(long result)
{
    return result < 0 && result >= -4095;
}

static inline long rustos_linux_openat(long dirfd, const char *path, long flags, long mode)
{
    return rustos_syscall4(RUSTOS_LINUX_SYS_OPENAT, dirfd, (long)path, flags, mode);
}

static inline long rustos_linux_close(long fd)
{
    return rustos_syscall1(RUSTOS_LINUX_SYS_CLOSE, fd);
}

static inline long rustos_linux_read(long fd, void *buffer, size_t len)
{
    return rustos_syscall3(RUSTOS_LINUX_SYS_READ, fd, (long)buffer, (long)len);
}

static inline long rustos_linux_ioctl(long fd, unsigned long request, void *arg)
{
    return rustos_syscall3(RUSTOS_LINUX_SYS_IOCTL, fd, (long)request, (long)arg);
}

static inline long rustos_linux_mmap(
    void *requested_addr,
    uint64_t len,
    long prot,
    long flags,
    long fd,
    uint64_t offset)
{
    return rustos_syscall6(
        RUSTOS_LINUX_SYS_MMAP,
        (long)requested_addr,
        (long)len,
        prot,
        flags,
        fd,
        (long)offset);
}

static inline long rustos_linux_munmap(void *start, uint64_t len)
{
    return rustos_syscall2(RUSTOS_LINUX_SYS_MUNMAP, (long)start, (long)len);
}

#endif
