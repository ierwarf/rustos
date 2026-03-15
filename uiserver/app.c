#include "app.h"

#include <string.h>

static void clamp_cursor(struct app_state *state)
{
    if (state->display.width == 0 || state->display.height == 0)
    {
        return;
    }

    if (state->cursor_x >= state->display.width)
    {
        state->cursor_x = state->display.width - 1;
    }
    if (state->cursor_y >= state->display.height)
    {
        state->cursor_y = state->display.height - 1;
    }
}

void app_state_init(struct app_state *state)
{
    memset(state, 0, sizeof(*state));
    state->display_fd = -1;
    state->input_fd = -1;
    state->runtime_fd = -1;
    state->surface_fd = -1;
}

int app_initialize(struct app_state *state)
{
    long result;

    result = rustos_display_open();
    if (rustos_syscall_failed(result))
    {
        return 10;
    }
    state->display_fd = (int32_t)result;

    result = rustos_input_open();
    if (rustos_syscall_failed(result))
    {
        return 11;
    }
    state->input_fd = (int32_t)result;

    result = rustos_runtime_open();
    if (rustos_syscall_failed(result))
    {
        return 19;
    }
    state->runtime_fd = (int32_t)result;

    if (rustos_display_get_info(state->display_fd, &state->display) != 0)
    {
        return 12;
    }

    if (state->display.bytes_per_pixel != 4 ||
        state->display.pixel_format != RUSTOS_PIXEL_FORMAT_BGRA8888)
    {
        return 13;
    }

    state->surface.width = state->display.width;
    state->surface.height = state->display.height;
    state->surface.pixel_format = RUSTOS_PIXEL_FORMAT_BGRA8888;
    state->surface.flags = 0;

    if (rustos_display_create_surface(state->display_fd, &state->surface) != 0)
    {
        return 14;
    }

    state->surface_fd = (int32_t)state->surface.handle;
    if (state->surface_fd < 0 ||
        state->surface.width != state->display.width ||
        state->surface.height != state->display.height ||
        state->surface.bytes_per_pixel != state->display.bytes_per_pixel ||
        state->surface.pixel_format != state->display.pixel_format ||
        state->surface.mapping_len == 0)
    {
        return 15;
    }

    if (rustos_display_map_surface(
            state->surface_fd,
            state->surface.mapping_len,
            (void **)&state->frame) != 0)
    {
        return 16;
    }

    state->cursor_x = state->display.width / 2;
    state->cursor_y = state->display.height / 2;
    app_refresh_runtime_programs(state);
    return 0;
}

void app_cleanup(struct app_state *state)
{
    if (state->frame != NULL && state->surface.mapping_len != 0)
    {
        rustos_display_unmap_surface(state->frame, state->surface.mapping_len);
        state->frame = NULL;
    }

    if (state->surface_fd >= 0)
    {
        rustos_linux_close(state->surface_fd);
        state->surface_fd = -1;
    }

    if (state->runtime_fd >= 0)
    {
        rustos_linux_close(state->runtime_fd);
        state->runtime_fd = -1;
    }

    if (state->input_fd >= 0)
    {
        rustos_linux_close(state->input_fd);
        state->input_fd = -1;
    }

    if (state->display_fd >= 0)
    {
        rustos_linux_close(state->display_fd);
        state->display_fd = -1;
    }
}

void app_refresh_runtime_programs(struct app_state *state)
{
    long generation;
    long count;
    uint64_t generation_value = 0;

    generation = rustos_runtime_generation(state->runtime_fd, &generation_value);
    if (generation < 0 || (long)generation_value == state->runtime_generation)
    {
        return;
    }

    count = rustos_runtime_snapshot_running_programs(
        state->runtime_fd,
        state->running_programs,
        MAX_RUNNING_PROGRAMS);
    if (count < 0)
    {
        return;
    }

    state->runtime_generation = (long)generation_value;
    state->running_program_count = (size_t)count;
}

void app_handle_input_event(struct app_state *state, const struct rustos_input_event *event)
{
    state->event_count++;

    switch (event->kind)
    {
    case RUSTOS_INPUT_KIND_KEYBOARD:
        if (event->action == RUSTOS_INPUT_ACTION_RELEASED)
        {
            return;
        }

        if (event->text == '\r' || event->text == '\n')
        {
            state->input_len = 0;
            return;
        }

        if (event->text >= 0x20 && event->text <= 0x7e)
        {
            if (state->input_len < INPUT_CAPACITY)
            {
                state->input_len++;
            }
        }
        return;

    case RUSTOS_INPUT_KIND_POINTER_MOTION:
    {
        int next_x = (int)state->cursor_x + event->value0;
        int next_y = (int)state->cursor_y + event->value1;

        if (next_x < 0)
        {
            next_x = 0;
        }
        if (next_y < 0)
        {
            next_y = 0;
        }

        state->cursor_x = (uint32_t)next_x;
        state->cursor_y = (uint32_t)next_y;
        clamp_cursor(state);
        return;
    }

    case RUSTOS_INPUT_KIND_POINTER_BUTTON:
        if (event->code == RUSTOS_POINTER_BUTTON_LEFT)
        {
            state->left_button_down = event->action == RUSTOS_INPUT_ACTION_PRESSED;
        }
        return;

    default:
        return;
    }
}
