#include "render.h"

enum
{
    COLOR_BG_TOP = 0x00152038,
    COLOR_BG_BOTTOM = 0x000b101f,
    COLOR_PANEL = 0x00151d2c,
    COLOR_PANEL_EDGE = 0x00315d9a,
    COLOR_TASKBAR = 0x000d141a,
    COLOR_SLOT_IDLE = 0x00203049,
    COLOR_SLOT_ACTIVE = 0x004dc4f5,
    COLOR_SLOT_WARN = 0x00ffbf5f,
    COLOR_CURSOR = 0x00ffffff,
    COLOR_CURSOR_SHADOW = 0x00060a13,
};

static void put_pixel(
    uint32_t *frame,
    uint32_t stride_pixels,
    uint32_t width,
    uint32_t height,
    int x,
    int y,
    uint32_t color)
{
    if (x < 0 || y < 0 || (uint32_t)x >= width || (uint32_t)y >= height)
    {
        return;
    }

    frame[(size_t)y * stride_pixels + (size_t)x] = color;
}

static void fill_rect(
    uint32_t *frame,
    uint32_t stride_pixels,
    uint32_t width,
    uint32_t height,
    int x,
    int y,
    int rect_w,
    int rect_h,
    uint32_t color)
{
    int start_x = x < 0 ? 0 : x;
    int start_y = y < 0 ? 0 : y;
    int end_x = x + rect_w;
    int end_y = y + rect_h;

    if (rect_w <= 0 || rect_h <= 0 || end_x <= 0 || end_y <= 0)
    {
        return;
    }
    if ((uint32_t)start_x >= width || (uint32_t)start_y >= height)
    {
        return;
    }

    if ((uint32_t)end_x > width)
    {
        end_x = (int)width;
    }
    if ((uint32_t)end_y > height)
    {
        end_y = (int)height;
    }

    for (int row = start_y; row < end_y; row++)
    {
        uint32_t *dst = frame + (size_t)row * stride_pixels + (size_t)start_x;
        for (int col = start_x; col < end_x; col++)
        {
            *dst++ = color;
        }
    }
}

static void draw_vertical_gradient(
    uint32_t *frame,
    uint32_t stride_pixels,
    uint32_t width,
    uint32_t height)
{
    uint32_t top_b = COLOR_BG_TOP & 0xffu;
    uint32_t top_g = (COLOR_BG_TOP >> 8) & 0xffu;
    uint32_t top_r = (COLOR_BG_TOP >> 16) & 0xffu;
    uint32_t bot_b = COLOR_BG_BOTTOM & 0xffu;
    uint32_t bot_g = (COLOR_BG_BOTTOM >> 8) & 0xffu;
    uint32_t bot_r = (COLOR_BG_BOTTOM >> 16) & 0xffu;

    for (uint32_t y = 0; y < height; y++)
    {
        uint32_t denom = height > 1 ? (height - 1) : 1;
        uint32_t b = top_b + ((bot_b - top_b) * y) / denom;
        uint32_t g = top_g + ((bot_g - top_g) * y) / denom;
        uint32_t r = top_r + ((bot_r - top_r) * y) / denom;
        uint32_t color = (r << 16) | (g << 8) | b;
        uint32_t *row = frame + (size_t)y * stride_pixels;
        for (uint32_t x = 0; x < width; x++)
        {
            row[x] = color;
        }
    }
}

static void draw_cursor(
    uint32_t *frame,
    uint32_t stride_pixels,
    uint32_t width,
    uint32_t height,
    const struct app_state *state)
{
    static const uint8_t CURSOR_MASK[12] = {
        0x80, 0xc0, 0xe0, 0xf0, 0xf8, 0xf0, 0xd8, 0x98, 0x18, 0x18, 0x18, 0x00};

    int base_x = (int)state->cursor_x;
    int base_y = (int)state->cursor_y;

    for (int row = 0; row < 12; row++)
    {
        for (int col = 0; col < 8; col++)
        {
            if ((CURSOR_MASK[row] & (0x80u >> col)) == 0)
            {
                continue;
            }

            put_pixel(
                frame,
                stride_pixels,
                width,
                height,
                base_x + col + 1,
                base_y + row + 1,
                COLOR_CURSOR_SHADOW);
        }
    }

    for (int row = 0; row < 12; row++)
    {
        for (int col = 0; col < 8; col++)
        {
            if ((CURSOR_MASK[row] & (0x80u >> col)) == 0)
            {
                continue;
            }

            put_pixel(
                frame,
                stride_pixels,
                width,
                height,
                base_x + col,
                base_y + row,
                COLOR_CURSOR);
        }
    }
}

void render_frame(struct app_state *state)
{
    uint32_t width = state->surface.width;
    uint32_t height = state->surface.height;
    uint32_t stride_pixels = state->surface.stride_bytes / 4;
    uint32_t *frame = state->frame;
    int taskbar_y = (int)height - 56;
    int slot_y = 136;

    draw_vertical_gradient(frame, stride_pixels, width, height);

    fill_rect(frame, stride_pixels, width, height, 0, 0, (int)width, 72, COLOR_PANEL);
    fill_rect(frame, stride_pixels, width, height, 0, taskbar_y, (int)width, 56, COLOR_TASKBAR);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        40,
        108,
        (int)width - 80,
        (int)height - 196,
        COLOR_PANEL);
    fill_rect(frame, stride_pixels, width, height, 40, 108, (int)width - 80, 2, COLOR_PANEL_EDGE);
    fill_rect(frame, stride_pixels, width, height, 40, 108, 2, (int)height - 196, COLOR_PANEL_EDGE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        (int)width - 42,
        108,
        2,
        (int)height - 196,
        COLOR_PANEL_EDGE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        40,
        (int)height - 90,
        (int)width - 80,
        2,
        COLOR_PANEL_EDGE);

    fill_rect(frame, stride_pixels, width, height, 56, 24, 88, 24, COLOR_SLOT_ACTIVE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        164,
        24,
        88,
        24,
        state->left_button_down ? COLOR_SLOT_WARN : COLOR_SLOT_IDLE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        272,
        24,
        88,
        24,
        state->event_count != 0 ? COLOR_SLOT_ACTIVE : COLOR_SLOT_IDLE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        380,
        24,
        88,
        24,
        state->input_len != 0 ? COLOR_SLOT_ACTIVE : COLOR_SLOT_IDLE);

    for (size_t index = 0; index < MAX_RUNNING_PROGRAMS; index++)
    {
        uint32_t color = index < state->running_program_count ? COLOR_SLOT_ACTIVE : COLOR_SLOT_IDLE;
        fill_rect(frame, stride_pixels, width, height, 64, slot_y + (int)index * 36, 320, 24, color);
    }

    for (size_t index = 0; index < MAX_RUNNING_PROGRAMS; index++)
    {
        uint32_t color = index < state->running_program_count ? COLOR_SLOT_ACTIVE : COLOR_SLOT_IDLE;
        fill_rect(
            frame,
            stride_pixels,
            width,
            height,
            56 + (int)index * 148,
            taskbar_y + 12,
            132,
            28,
            color);
    }

    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        64,
        (int)height - 152,
        (int)(width > 160 ? width - 128 : width / 2),
        8,
        COLOR_SLOT_IDLE);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        64,
        (int)height - 152,
        (int)(state->input_len * 8),
        8,
        COLOR_SLOT_ACTIVE);

    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        440,
        136,
        (int)((state->event_count % 96) * 6),
        20,
        COLOR_SLOT_WARN);
    fill_rect(
        frame,
        stride_pixels,
        width,
        height,
        440,
        176,
        (int)((state->runtime_generation % 96) * 6),
        20,
        COLOR_SLOT_ACTIVE);

    draw_cursor(frame, stride_pixels, width, height, state);
}
