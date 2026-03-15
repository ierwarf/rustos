#ifndef RUSTOS_INPUT_H
#define RUSTOS_INPUT_H

#include "rustos_linux.h"

#include <stddef.h>
#include <stdint.h>

enum
{
    RUSTOS_INPUT_KIND_KEYBOARD = 1,
    RUSTOS_INPUT_KIND_POINTER_MOTION = 2,
    RUSTOS_INPUT_KIND_POINTER_BUTTON = 3,
};

enum
{
    RUSTOS_INPUT_ACTION_NONE = 0,
    RUSTOS_INPUT_ACTION_PRESSED = 1,
    RUSTOS_INPUT_ACTION_RELEASED = 2,
    RUSTOS_INPUT_ACTION_REPEATED = 3,
};

enum
{
    RUSTOS_POINTER_BUTTON_LEFT = 1,
};

struct rustos_input_event
{
    uint16_t kind;
    uint16_t action;
    uint32_t code;
    int32_t value0;
    int32_t value1;
    uint32_t modifiers;
    uint32_t text;
};

static inline long rustos_input_open(void)
{
    return rustos_linux_openat(RUSTOS_AT_FDCWD, "/dev/input0", RUSTOS_O_RDONLY, 0);
}

static inline long rustos_input_read(
    long input_fd,
    struct rustos_input_event *events,
    size_t capacity)
{
    long bytes = rustos_linux_read(input_fd, events, capacity * sizeof(*events));
    if (rustos_syscall_failed(bytes))
    {
        return bytes;
    }

    if ((bytes % (long)sizeof(*events)) != 0)
    {
        return -RUSTOS_LINUX_EIO;
    }

    return bytes / (long)sizeof(*events);
}

#endif
