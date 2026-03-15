#ifndef RUSTOS_DISPLAY_H
#define RUSTOS_DISPLAY_H

#include "rustos_linux.h"

#include <stdint.h>

enum
{
    RUSTOS_PIXEL_FORMAT_BGRA8888 = 1,
};

enum
{
    RUSTOS_DISPLAY_IOCTL_GET_INFO = 0x44530001u,
    RUSTOS_DISPLAY_IOCTL_CREATE_SURFACE = 0x44530002u,
    RUSTOS_DISPLAY_IOCTL_PRESENT = 0x44530003u,
};

struct rustos_display_info
{
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
    uint32_t bytes_per_pixel;
    uint32_t pixel_format;
    uint32_t reserved;
};

struct rustos_display_surface_create
{
    uint32_t width;
    uint32_t height;
    uint32_t pixel_format;
    uint32_t flags;
    uint32_t handle;
    uint32_t bytes_per_pixel;
    uint32_t stride_bytes;
    uint32_t reserved;
    uint64_t mapping_len;
};

struct rustos_display_present_request
{
    uint32_t surface_handle;
    uint32_t reserved;
};

static inline long rustos_display_open(void)
{
    return rustos_linux_openat(RUSTOS_AT_FDCWD, "/dev/display0", RUSTOS_O_RDWR, 0);
}

static inline long rustos_display_get_info(long display_fd, struct rustos_display_info *info)
{
    return rustos_linux_ioctl(display_fd, RUSTOS_DISPLAY_IOCTL_GET_INFO, info);
}

static inline long rustos_display_create_surface(
    long display_fd,
    struct rustos_display_surface_create *surface)
{
    return rustos_linux_ioctl(display_fd, RUSTOS_DISPLAY_IOCTL_CREATE_SURFACE, surface);
}

static inline long rustos_display_map_surface(
    long surface_fd,
    uint64_t mapping_len,
    void **mapped_base)
{
    long mapped = rustos_linux_mmap(
        0,
        mapping_len,
        RUSTOS_PROT_READ | RUSTOS_PROT_WRITE,
        RUSTOS_MAP_SHARED,
        surface_fd,
        0);
    if (rustos_syscall_failed(mapped))
    {
        return mapped;
    }

    *mapped_base = (void *)(uintptr_t)mapped;
    return 0;
}

static inline long rustos_display_unmap_surface(void *mapped_base, uint64_t mapping_len)
{
    return rustos_linux_munmap(mapped_base, mapping_len);
}

static inline long rustos_display_present(long display_fd, uint32_t surface_handle)
{
    struct rustos_display_present_request request = {
        .surface_handle = surface_handle,
        .reserved = 0,
    };

    return rustos_linux_ioctl(display_fd, RUSTOS_DISPLAY_IOCTL_PRESENT, &request);
}

#endif
