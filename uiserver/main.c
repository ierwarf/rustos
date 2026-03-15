#include "app.h"
#include "render.h"

#include <time.h>

static const struct timespec IDLE_SLEEP = {
    .tv_sec = 0,
    .tv_nsec = 16 * 1000 * 1000,
};
static int present_frame(struct app_state *state)
{
    return rustos_display_present(state->display_fd, state->surface.handle) == 0 ? 0 : 19;
}

static int run_event_loop(
    struct app_state *state,
    struct rustos_input_event *events,
    size_t event_capacity)
{
    for (;;)
    {
        bool changed = false;
        long read_count = rustos_input_read(state->input_fd, events, event_capacity);
        if (read_count < 0)
        {
            return 18;
        }

        for (long index = 0; index < read_count; index++)
        {
            app_handle_input_event(state, &events[index]);
            changed = true;
        }

        {
            long previous_generation = state->runtime_generation;
            app_refresh_runtime_programs(state);
            changed |= previous_generation != state->runtime_generation;
        }

        if (changed)
        {
            render_frame(state);
            {
                int present_result = present_frame(state);
                if (present_result != 0)
                {
                    return present_result;
                }
            }
            continue;
        }

        nanosleep(&IDLE_SLEEP, NULL);
    }
}

int main(void)
{
    struct app_state state;
    struct rustos_input_event events[INPUT_EVENT_BATCH];
    int exit_code;

    app_state_init(&state);

    exit_code = app_initialize(&state);
    if (exit_code == 0)
    {
        render_frame(&state);
        exit_code = present_frame(&state);
    }
    if (exit_code == 0)
    {
        exit_code = run_event_loop(&state, events, INPUT_EVENT_BATCH);
    }

    app_cleanup(&state);
    return exit_code;
}
