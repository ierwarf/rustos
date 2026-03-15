#ifndef UISERVER_APP_H
#define UISERVER_APP_H

#include "rustos_display.h"
#include "rustos_input.h"
#include "rustos_runtime.h"

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

enum
{
    INPUT_EVENT_BATCH = 32,
    MAX_RUNNING_PROGRAMS = 8,
    INPUT_CAPACITY = 80,
};

struct app_state
{
    struct rustos_display_info display;
    struct rustos_display_surface_create surface;
    int32_t display_fd;
    int32_t input_fd;
    int32_t runtime_fd;
    int32_t surface_fd;
    uint32_t *frame;
    uint32_t cursor_x;
    uint32_t cursor_y;
    bool left_button_down;
    uint32_t event_count;
    uint32_t input_len;
    long runtime_generation;
    struct rustos_runtime_running_program running_programs[MAX_RUNNING_PROGRAMS];
    size_t running_program_count;
};

void app_state_init(struct app_state *state);
int app_initialize(struct app_state *state);
void app_cleanup(struct app_state *state);
void app_refresh_runtime_programs(struct app_state *state);
void app_handle_input_event(struct app_state *state, const struct rustos_input_event *event);

#endif
