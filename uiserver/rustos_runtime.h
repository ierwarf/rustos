#ifndef RUSTOS_RUNTIME_H
#define RUSTOS_RUNTIME_H

#include "rustos_linux.h"

#include <stddef.h>
#include <stdint.h>

enum
{
    RUSTOS_RUNTIME_IOCTL_GET_GENERATION = 0x52540001u,
    RUSTOS_RUNTIME_IOCTL_SNAPSHOT_PROGRAMS = 0x52540002u,
    RUSTOS_RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS = 0x52540003u,
    RUSTOS_RUNTIME_IOCTL_REQUEST_LAUNCH = 0x52540004u,
    RUSTOS_RUNTIME_IOCTL_REQUEST_TERMINATE = 0x52540005u,
};

enum
{
    RUSTOS_RUNTIME_RUNNING_PROGRAM_NAME_CAPACITY = 48,
    RUSTOS_RUNTIME_PROGRAM_NAME_CAPACITY = 48,
    RUSTOS_RUNTIME_PROGRAM_PATH_CAPACITY = 64,
};

struct rustos_runtime_program
{
    uint32_t program_id;
    uint32_t reserved;
    uint64_t weight_micros;
    char display_name[RUSTOS_RUNTIME_PROGRAM_NAME_CAPACITY];
    char exec_path[RUSTOS_RUNTIME_PROGRAM_PATH_CAPACITY];
};

struct rustos_runtime_running_program
{
    uint64_t pid;
    uint32_t program_id;
    uint32_t session_index;
    char display_name[RUSTOS_RUNTIME_RUNNING_PROGRAM_NAME_CAPACITY];
};

struct rustos_runtime_generation
{
    uint64_t generation;
};

struct rustos_runtime_snapshot_programs_request
{
    uint64_t programs_ptr;
    uint64_t capacity;
    uint64_t count;
};

struct rustos_runtime_snapshot_running_programs_request
{
    uint64_t programs_ptr;
    uint64_t capacity;
    uint64_t count;
};

struct rustos_runtime_launch_request
{
    uint64_t program_id;
    uint16_t target_kind;
    uint16_t reserved;
    uint32_t reserved2;
    uint64_t target_value;
};

struct rustos_runtime_terminate_request
{
    uint16_t target_kind;
    uint16_t reserved;
    uint32_t reserved2;
    uint64_t target_value;
};

static inline long rustos_runtime_open(void)
{
    return rustos_linux_openat(RUSTOS_AT_FDCWD, "/dev/runtime0", RUSTOS_O_RDWR, 0);
}

static inline long rustos_runtime_generation(long runtime_fd, uint64_t *generation)
{
    struct rustos_runtime_generation request = {
        .generation = 0,
    };
    long result = rustos_linux_ioctl(runtime_fd, RUSTOS_RUNTIME_IOCTL_GET_GENERATION, &request);
    if (rustos_syscall_failed(result))
    {
        return result;
    }

    *generation = request.generation;
    return 0;
}

static inline long rustos_runtime_snapshot_running_programs(
    long runtime_fd,
    struct rustos_runtime_running_program *programs,
    size_t capacity)
{
    struct rustos_runtime_snapshot_running_programs_request request = {
        .programs_ptr = (uint64_t)(uintptr_t)programs,
        .capacity = (uint64_t)capacity,
        .count = 0,
    };
    long result = rustos_linux_ioctl(
        runtime_fd,
        RUSTOS_RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS,
        &request);
    if (rustos_syscall_failed(result))
    {
        return result;
    }

    return (long)request.count;
}

static inline long rustos_runtime_snapshot_programs(
    long runtime_fd,
    struct rustos_runtime_program *programs,
    size_t capacity)
{
    struct rustos_runtime_snapshot_programs_request request = {
        .programs_ptr = (uint64_t)(uintptr_t)programs,
        .capacity = (uint64_t)capacity,
        .count = 0,
    };
    long result = rustos_linux_ioctl(runtime_fd, RUSTOS_RUNTIME_IOCTL_SNAPSHOT_PROGRAMS, &request);
    if (rustos_syscall_failed(result))
    {
        return result;
    }

    return (long)request.count;
}

static inline long rustos_runtime_request_launch(
    long runtime_fd,
    uint64_t program_id,
    uint16_t target_kind,
    uint64_t target_value)
{
    struct rustos_runtime_launch_request request = {
        .program_id = program_id,
        .target_kind = target_kind,
        .reserved = 0,
        .reserved2 = 0,
        .target_value = target_value,
    };
    return rustos_linux_ioctl(runtime_fd, RUSTOS_RUNTIME_IOCTL_REQUEST_LAUNCH, &request);
}

static inline long rustos_runtime_request_terminate(
    long runtime_fd,
    uint16_t target_kind,
    uint64_t target_value)
{
    struct rustos_runtime_terminate_request request = {
        .target_kind = target_kind,
        .reserved = 0,
        .reserved2 = 0,
        .target_value = target_value,
    };
    return rustos_linux_ioctl(runtime_fd, RUSTOS_RUNTIME_IOCTL_REQUEST_TERMINATE, &request);
}

#endif
