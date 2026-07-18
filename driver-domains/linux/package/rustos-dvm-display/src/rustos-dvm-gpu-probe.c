// SPDX-License-Identifier: MIT
// Bounded GPU executor proof for the private RustOS compositor contract.
//
// This is deliberately not an application graphics API and is not the final
// RustOS-to-DVM transport. It validates the exact fixed wire vocabulary used
// by driver-domain-protocol, executes those commands through built-in GLES 3
// shaders, waits on explicit GPU fences, verifies output pixels, and keeps the
// context alive with a bounded health submission. No raw shader, GPU address,
// or command-buffer byte is accepted from another domain.

#define _GNU_SOURCE

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>

#ifndef EGL_PLATFORM_GBM_KHR
#define EGL_PLATFORM_GBM_KHR 0x31D7
#endif
#ifndef EGL_OPENGL_ES3_BIT_KHR
#define EGL_OPENGL_ES3_BIT_KHR 0x0040
#endif
#ifndef GL_BGRA_EXT
#define GL_BGRA_EXT 0x80E1
#endif

#define GPU_BATCH_MAGIC "RSGPU001"
#define GPU_BATCH_VERSION 1U
#define GPU_HEADER_BYTES 64U
#define GPU_SOURCE_BYTES 64U
#define GPU_COMMAND_BYTES 64U
#define GPU_MAX_SOURCES 1U
#define GPU_MAX_IN_FLIGHT 3U
#define GPU_MAX_COMMANDS 512U
#define GPU_FRAME_TARGET_US 16667U
#define GPU_MAX_BUDGET_US 50000U
#define GPU_MAX_DIMENSION 8192U
#define GPU_MAX_SOURCE_BYTES (256ULL * 1024ULL * 1024ULL)
#define GPU_MAX_BATCH_SOURCE_BYTES GPU_MAX_SOURCE_BYTES
#define GPU_BATCH_FLAG_PRESENT 1U
#define GPU_SOURCE_FLAG_READ_ONLY 1U
#define GPU_SOURCE_FLAG_PREMULTIPLIED_ALPHA 2U
#define GPU_SOURCE_REQUIRED_FLAGS \
    (GPU_SOURCE_FLAG_READ_ONLY | GPU_SOURCE_FLAG_PREMULTIPLIED_ALPHA)
#define GPU_PIXEL_FORMAT_BGRA8888 1U
#define GPU_NO_SOURCE UINT32_MAX
#define GPU_COMMAND_CLEAR 1U
#define GPU_COMMAND_SOLID_QUAD 2U
#define GPU_COMMAND_TEXTURED_QUAD 3U
#define GPU_BLEND_REPLACE 1U
#define GPU_BLEND_SOURCE_OVER 2U
#define GPU_COMMAND_FLAG_CLIP_OUTPUT 1U
#define GPU_TRANSFORM_LIMIT (4 * 65536)
#define GPU_OUTPUT_WIDTH 128U
#define GPU_OUTPUT_HEIGHT 128U
#define GPU_SOURCE_WIDTH 64U
#define GPU_SOURCE_HEIGHT 64U
#define GPU_PROOF_FRAMES 120U
#define GPU_WARMUP_FRAMES 8U
#define GPU_PIPELINE_PRIME_TIMEOUT_US 500000U
#define GPU_MIN_FPS_MILLI 60000ULL
#define GPU_HEALTH_INTERVAL_SECONDS 1U
#define GPU_PROOF_RR_PRIORITY 8
#define GPU_PROOF_RTTIME_SOFT_US 50000U
#define GPU_PROOF_RTTIME_HARD_US 100000U
#ifndef GPU_EVIDENCE
#define GPU_EVIDENCE "/run/rustos-dvm/gpu-compositor-evidence-v1.env"
#endif
#ifndef GPU_EVIDENCE_TEMP
#define GPU_EVIDENCE_TEMP "/run/rustos-dvm/gpu-compositor-evidence-v1.env.tmp"
#endif
#ifndef GPU_PRIME_EVIDENCE
#define GPU_PRIME_EVIDENCE "/run/rustos-dvm/gpu-pipeline-prime-v1.env"
#endif
#ifndef GPU_PRIME_EVIDENCE_TEMP
#define GPU_PRIME_EVIDENCE_TEMP "/run/rustos-dvm/gpu-pipeline-prime-v1.env.tmp"
#endif

struct gpu_batch_header {
    uint32_t command_count;
    uint32_t context_id;
    uint32_t context_epoch;
    uint64_t submit_value;
    uint64_t acquire_value;
    uint32_t budget_us;
    uint32_t source_count;
    uint32_t flags;
};

struct gpu_source {
    uint64_t token;
    uint64_t generation;
    uint64_t acquire_value;
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
    uint32_t pixel_format;
    uint32_t flags;
    uint32_t binding_slot;
    uint64_t content_epoch;
};

struct gpu_command {
    uint32_t kind;
    uint32_t flags;
    uint32_t source_index;
    uint32_t blend_mode;
    int32_t destination_x;
    int32_t destination_y;
    uint32_t destination_width;
    uint32_t destination_height;
    uint16_t source_u;
    uint16_t source_v;
    uint16_t source_width;
    uint16_t source_height;
    uint32_t rgba;
    int32_t depth;
    int32_t rotation;
    int32_t tilt_x;
    int32_t tilt_y;
    int32_t perspective;
};

struct gpu_executor {
    int drm_fd;
    struct gbm_device *gbm;
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    GLuint program;
    GLuint output_texture;
    GLuint source_texture;
    GLuint framebuffer;
    GLuint vertex_buffer;
    GLuint vertex_array;
    GLint rect_uniform;
    GLint output_size_uniform;
    GLint color_uniform;
    GLint transform_uniform;
    GLint perspective_uniform;
    GLint uv_rect_uniform;
    GLint use_texture_uniform;
    char driver[64];
    char renderer[160];
    GLsync source_acquire_fence;
    uint64_t source_acquire_value;
    uint64_t source_acquire_completed;
    uint64_t expected_submit;
};

struct proof_scheduler_guard {
    int active;
    int saved_policy;
    struct sched_param saved_param;
    struct rlimit saved_rttime;
};

static volatile sig_atomic_t stop_requested;

static int proof_scheduler_leave(struct proof_scheduler_guard *guard) {
    struct sched_param observed;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno = 0;

    if (guard == NULL || !guard->active)
        return 0;
    if (sched_setscheduler(0, guard->saved_policy, &guard->saved_param) != 0)
        saved_errno = errno;
    if (setrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0 && saved_errno == 0)
        saved_errno = errno;
    guard->active = 0;
    observed_policy = sched_getscheduler(0);
    if ((observed_policy != guard->saved_policy || sched_getparam(0, &observed) != 0 ||
         observed.sched_priority != guard->saved_param.sched_priority ||
         getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
         observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur ||
         observed_rttime.rlim_max != guard->saved_rttime.rlim_max) &&
        saved_errno == 0)
        saved_errno = errno != 0 ? errno : EINVAL;
    if (saved_errno != 0) {
        errno = saved_errno;
        return -1;
    }
    return 0;
}

/*
 * Only the bounded, post-prime performance proof receives realtime
 * scheduling. Its priority remains below the authenticated display and input
 * relays, and the exact prior normal policy is restored before evidence is
 * published or the long-lived health loop begins. Exceeding the hard
 * RLIMIT_RTTIME is deliberately different: Linux terminates the process, the
 * init owner observes the dead PID, and no readiness evidence can exist.
 */
static int proof_scheduler_enter(struct proof_scheduler_guard *guard) {
    struct rlimit bounded_rttime = {
        .rlim_cur = GPU_PROOF_RTTIME_SOFT_US,
        .rlim_max = GPU_PROOF_RTTIME_HARD_US,
    };
    struct sched_param realtime = {.sched_priority = GPU_PROOF_RR_PRIORITY};
    struct sched_param observed;
    struct rlimit observed_rttime;
    int observed_policy;
    int saved_errno;

    if (guard == NULL) {
        errno = EINVAL;
        return -1;
    }
    memset(guard, 0, sizeof(*guard));
    guard->saved_policy = sched_getscheduler(0);
    if (guard->saved_policy < 0 || sched_getparam(0, &guard->saved_param) != 0 ||
        getrlimit(RLIMIT_RTTIME, &guard->saved_rttime) != 0)
        return -1;
    if (guard->saved_policy != SCHED_OTHER || guard->saved_param.sched_priority != 0) {
        errno = EINVAL;
        return -1;
    }
    if (setrlimit(RLIMIT_RTTIME, &bounded_rttime) != 0)
        return -1;
    if (getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
        observed_rttime.rlim_cur != bounded_rttime.rlim_cur ||
        observed_rttime.rlim_max != bounded_rttime.rlim_max) {
        saved_errno = errno != 0 ? errno : EINVAL;
        (void)setrlimit(RLIMIT_RTTIME, &guard->saved_rttime);
        errno = saved_errno;
        return -1;
    }
    if (sched_setscheduler(0, SCHED_RR, &realtime) != 0) {
        saved_errno = errno;
        (void)setrlimit(RLIMIT_RTTIME, &guard->saved_rttime);
        errno = saved_errno;
        return -1;
    }
    guard->active = 1;
    observed_policy = sched_getscheduler(0);
    if (observed_policy != SCHED_RR || sched_getparam(0, &observed) != 0 ||
        observed.sched_priority != GPU_PROOF_RR_PRIORITY ||
        getrlimit(RLIMIT_RTTIME, &observed_rttime) != 0 ||
        observed_rttime.rlim_cur != bounded_rttime.rlim_cur ||
        observed_rttime.rlim_max != bounded_rttime.rlim_max) {
        saved_errno = errno != 0 ? errno : EINVAL;
        (void)proof_scheduler_leave(guard);
        errno = saved_errno;
        return -1;
    }
    return 0;
}

static void request_stop(int signal_number) {
    (void)signal_number;
    stop_requested = 1;
}

static uint16_t read_le16(const uint8_t *bytes) {
    return (uint16_t)bytes[0] | (uint16_t)((uint16_t)bytes[1] << 8);
}

static uint32_t read_le32(const uint8_t *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) | ((uint32_t)bytes[2] << 16) |
           ((uint32_t)bytes[3] << 24);
}

static int32_t read_le_i32(const uint8_t *bytes) {
    return (int32_t)read_le32(bytes);
}

static uint64_t read_le64(const uint8_t *bytes) {
    uint64_t value = 0U;
    unsigned int index;
    for (index = 0U; index < 8U; index++)
        value |= (uint64_t)bytes[index] << (index * 8U);
    return value;
}

static void write_le16(uint8_t *bytes, uint16_t value) {
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
}

static void write_le32(uint8_t *bytes, uint32_t value) {
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
    bytes[2] = (uint8_t)(value >> 16);
    bytes[3] = (uint8_t)(value >> 24);
}

static void write_le64(uint8_t *bytes, uint64_t value) {
    unsigned int index;
    for (index = 0U; index < 8U; index++)
        bytes[index] = (uint8_t)(value >> (index * 8U));
}

static int bytes_zero(const uint8_t *bytes, size_t length) {
    size_t index;
    for (index = 0U; index < length; index++) {
        if (bytes[index] != 0U)
            return 0;
    }
    return 1;
}

static int fixed_bounded(int32_t value) {
    return value != INT32_MIN && value >= -GPU_TRANSFORM_LIMIT && value <= GPU_TRANSFORM_LIMIT;
}

static int parse_batch_header(const uint8_t *bytes, size_t length,
                              struct gpu_batch_header *header, size_t *required) {
    uint64_t source_bytes;
    uint64_t command_bytes;
    uint64_t total;
    if (bytes == NULL || header == NULL || required == NULL || length < GPU_HEADER_BYTES ||
        memcmp(bytes, GPU_BATCH_MAGIC, 8U) != 0 || read_le32(bytes + 8U) != GPU_BATCH_VERSION ||
        read_le32(bytes + 12U) != GPU_HEADER_BYTES ||
        read_le32(bytes + 16U) != GPU_COMMAND_BYTES || !bytes_zero(bytes + 60U, 4U)) {
        errno = EPROTO;
        return -1;
    }
    header->command_count = read_le32(bytes + 20U);
    header->context_id = read_le32(bytes + 24U);
    header->context_epoch = read_le32(bytes + 28U);
    header->submit_value = read_le64(bytes + 32U);
    header->acquire_value = read_le64(bytes + 40U);
    header->budget_us = read_le32(bytes + 48U);
    header->source_count = read_le32(bytes + 52U);
    header->flags = read_le32(bytes + 56U);
    if (header->command_count == 0U || header->command_count > GPU_MAX_COMMANDS ||
        header->context_id == 0U || header->context_epoch == 0U ||
        header->submit_value == 0U || header->acquire_value == 0U ||
        header->acquire_value > header->submit_value || header->budget_us == 0U ||
        header->budget_us > GPU_MAX_BUDGET_US || header->source_count > GPU_MAX_SOURCES ||
        header->flags != GPU_BATCH_FLAG_PRESENT) {
        errno = EPROTO;
        return -1;
    }
    source_bytes = (uint64_t)header->source_count * GPU_SOURCE_BYTES;
    command_bytes = (uint64_t)header->command_count * GPU_COMMAND_BYTES;
    total = GPU_HEADER_BYTES + source_bytes + command_bytes;
    if (total > SIZE_MAX || total > length) {
        errno = EMSGSIZE;
        return -1;
    }
    *required = (size_t)total;
    return 0;
}

static int parse_source(const uint8_t *bytes, struct gpu_source *source) {
    uint64_t source_bytes;
    if (bytes == NULL || source == NULL || !bytes_zero(bytes + 56U, 8U)) {
        errno = EPROTO;
        return -1;
    }
    source->token = read_le64(bytes);
    source->generation = read_le64(bytes + 8U);
    source->acquire_value = read_le64(bytes + 16U);
    source->width = read_le32(bytes + 24U);
    source->height = read_le32(bytes + 28U);
    source->stride_bytes = read_le32(bytes + 32U);
    source->pixel_format = read_le32(bytes + 36U);
    source->flags = read_le32(bytes + 40U);
    source->binding_slot = read_le32(bytes + 44U);
    source->content_epoch = read_le64(bytes + 48U);
    source_bytes = (uint64_t)source->stride_bytes * source->height;
    if (source->token == 0U || source->generation == 0U || source->acquire_value == 0U ||
        source->width == 0U || source->width > GPU_MAX_DIMENSION || source->height == 0U ||
        source->height > GPU_MAX_DIMENSION || source->stride_bytes < source->width * 4U ||
        source->stride_bytes % 4U != 0U ||
        source_bytes > GPU_MAX_SOURCE_BYTES ||
        source->pixel_format != GPU_PIXEL_FORMAT_BGRA8888 ||
        source->flags != GPU_SOURCE_REQUIRED_FLAGS ||
        source->binding_slot >= GPU_MAX_IN_FLIGHT ||
        source->content_epoch == 0U) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int parse_command(const uint8_t *bytes, uint32_t source_count,
                         struct gpu_command *command) {
    uint64_t x_end;
    uint64_t y_end;
    uint32_t source_x_end;
    uint32_t source_y_end;
    if (bytes == NULL || command == NULL) {
        errno = EINVAL;
        return -1;
    }
    command->kind = read_le32(bytes);
    command->flags = read_le32(bytes + 4U);
    command->source_index = read_le32(bytes + 8U);
    command->blend_mode = read_le32(bytes + 12U);
    command->destination_x = read_le_i32(bytes + 16U);
    command->destination_y = read_le_i32(bytes + 20U);
    command->destination_width = read_le32(bytes + 24U);
    command->destination_height = read_le32(bytes + 28U);
    command->source_u = read_le16(bytes + 32U);
    command->source_v = read_le16(bytes + 34U);
    command->source_width = read_le16(bytes + 36U);
    command->source_height = read_le16(bytes + 38U);
    command->rgba = read_le32(bytes + 40U);
    command->depth = read_le_i32(bytes + 44U);
    command->rotation = read_le_i32(bytes + 48U);
    command->tilt_x = read_le_i32(bytes + 52U);
    command->tilt_y = read_le_i32(bytes + 56U);
    command->perspective = read_le_i32(bytes + 60U);
    if ((command->flags & ~GPU_COMMAND_FLAG_CLIP_OUTPUT) != 0U ||
        (command->blend_mode != GPU_BLEND_REPLACE &&
         command->blend_mode != GPU_BLEND_SOURCE_OVER) ||
        !fixed_bounded(command->depth) || !fixed_bounded(command->rotation) ||
        !fixed_bounded(command->tilt_x) || !fixed_bounded(command->tilt_y) ||
        !fixed_bounded(command->perspective)) {
        errno = EPROTO;
        return -1;
    }
    if (command->kind == GPU_COMMAND_CLEAR) {
        if (command->flags != 0U || command->source_index != GPU_NO_SOURCE ||
            command->blend_mode != GPU_BLEND_REPLACE || command->destination_x != 0 ||
            command->destination_y != 0 || command->destination_width != 0U ||
            command->destination_height != 0U || command->source_u != 0U ||
            command->source_v != 0U || command->source_width != 0U ||
            command->source_height != 0U || command->depth != 0 || command->rotation != 0 ||
            command->tilt_x != 0 || command->tilt_y != 0 || command->perspective != 0) {
            errno = EPROTO;
            return -1;
        }
        return 0;
    }
    if (command->kind != GPU_COMMAND_SOLID_QUAD &&
        command->kind != GPU_COMMAND_TEXTURED_QUAD) {
        errno = EOPNOTSUPP;
        return -1;
    }
    if (command->flags != GPU_COMMAND_FLAG_CLIP_OUTPUT || command->destination_x < 0 ||
        command->destination_y < 0 || command->destination_width == 0U ||
        command->destination_height == 0U) {
        errno = EPROTO;
        return -1;
    }
    x_end = (uint64_t)(uint32_t)command->destination_x + command->destination_width;
    y_end = (uint64_t)(uint32_t)command->destination_y + command->destination_height;
    if (x_end > GPU_OUTPUT_WIDTH || y_end > GPU_OUTPUT_HEIGHT) {
        errno = EPROTO;
        return -1;
    }
    if (command->kind == GPU_COMMAND_SOLID_QUAD) {
        if (command->source_index != GPU_NO_SOURCE || command->source_u != 0U ||
            command->source_v != 0U || command->source_width != 0U ||
            command->source_height != 0U) {
            errno = EPROTO;
            return -1;
        }
        return 0;
    }
    source_x_end = (uint32_t)command->source_u + command->source_width;
    source_y_end = (uint32_t)command->source_v + command->source_height;
    if (command->source_index >= source_count || command->source_width == 0U ||
        command->source_height == 0U || source_x_end > UINT16_MAX ||
        source_y_end > UINT16_MAX) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int monotonic_ns(uint64_t *value) {
    struct timespec now;
    if (value == NULL || clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0) {
        errno = EIO;
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
    return 0;
}

static GLuint compile_shader(GLenum kind, const char *source) {
    GLuint shader = glCreateShader(kind);
    GLint compiled = GL_FALSE;
    if (shader == 0U)
        return 0U;
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (compiled != GL_TRUE) {
        char log[512];
        GLsizei length = 0;
        glGetShaderInfoLog(shader, sizeof(log), &length, log);
        fprintf(stderr, "rustos-dvm-gpu: shader compile failed: %.*s\n", (int)length, log);
        glDeleteShader(shader);
        return 0U;
    }
    return shader;
}

static int create_program(struct gpu_executor *executor) {
    static const char vertex_source[] =
        "#version 300 es\n"
        "layout(location=0) in vec2 a_position;\n"
        "layout(location=1) in vec2 a_uv;\n"
        "uniform vec4 u_rect;\n"
        "uniform vec2 u_output_size;\n"
        "uniform vec4 u_transform;\n"
        "uniform float u_perspective;\n"
        "out vec2 v_uv;\n"
        "void main() {\n"
        "  float c = cos(u_transform.y);\n"
        "  float s = sin(u_transform.y);\n"
        "  vec2 rotated = mat2(c, -s, s, c) * a_position;\n"
        "  float z = u_transform.x + a_position.x * u_transform.z + "
        "a_position.y * u_transform.w;\n"
        "  float perspective = max(0.25, 1.0 + z * u_perspective);\n"
        "  vec2 local = rotated / perspective;\n"
        "  vec2 pixel = u_rect.xy + (local * 0.5 + 0.5) * u_rect.zw;\n"
        "  vec2 clip = vec2(pixel.x / u_output_size.x * 2.0 - 1.0, "
        "pixel.y / u_output_size.y * 2.0 - 1.0);\n"
        "  gl_Position = vec4(clip, clamp(z, -1.0, 1.0), 1.0);\n"
        "  v_uv = a_uv;\n"
        "}\n";
    static const char fragment_source[] =
        "#version 300 es\n"
        "precision highp float;\n"
        "in vec2 v_uv;\n"
        "uniform sampler2D u_source;\n"
        "uniform vec4 u_uv_rect;\n"
        "uniform vec4 u_color;\n"
        "uniform int u_use_texture;\n"
        "out vec4 out_color;\n"
        "void main() {\n"
        "  vec2 source_uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;\n"
        "  vec4 sampled = u_use_texture != 0 ? texture(u_source, source_uv) : vec4(1.0);\n"
        "  out_color = sampled * u_color;\n"
        "}\n";
    GLuint vertex = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    GLint linked = GL_FALSE;
    if (vertex == 0U || fragment == 0U) {
        if (vertex != 0U)
            glDeleteShader(vertex);
        if (fragment != 0U)
            glDeleteShader(fragment);
        return -1;
    }
    executor->program = glCreateProgram();
    glAttachShader(executor->program, vertex);
    glAttachShader(executor->program, fragment);
    glLinkProgram(executor->program);
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    glGetProgramiv(executor->program, GL_LINK_STATUS, &linked);
    if (linked != GL_TRUE) {
        errno = EPROTO;
        return -1;
    }
    executor->rect_uniform = glGetUniformLocation(executor->program, "u_rect");
    executor->output_size_uniform = glGetUniformLocation(executor->program, "u_output_size");
    executor->color_uniform = glGetUniformLocation(executor->program, "u_color");
    executor->transform_uniform = glGetUniformLocation(executor->program, "u_transform");
    executor->perspective_uniform = glGetUniformLocation(executor->program, "u_perspective");
    executor->uv_rect_uniform = glGetUniformLocation(executor->program, "u_uv_rect");
    executor->use_texture_uniform = glGetUniformLocation(executor->program, "u_use_texture");
    if (executor->rect_uniform < 0 || executor->output_size_uniform < 0 ||
        executor->color_uniform < 0 || executor->transform_uniform < 0 ||
        executor->perspective_uniform < 0 || executor->uv_rect_uniform < 0 ||
        executor->use_texture_uniform < 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int open_drm_render_node(char *path, size_t path_size) {
    unsigned int minor;
    for (minor = 128U; minor < 192U; minor++) {
        int fd;
        if (snprintf(path, path_size, "/dev/dri/renderD%u", minor) >= (int)path_size)
            break;
        fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd >= 0)
            return fd;
    }
    errno = ENODEV;
    return -1;
}

static int renderer_contains(const char *renderer, const char *needle) {
    char lowered[192];
    size_t index;
    size_t length;
    if (renderer == NULL || needle == NULL)
        return 0;
    length = strnlen(renderer, sizeof(lowered) - 1U);
    for (index = 0U; index < length; index++) {
        char byte = renderer[index];
        lowered[index] = byte >= 'A' && byte <= 'Z' ? (char)(byte - 'A' + 'a') : byte;
    }
    lowered[length] = '\0';
    return strstr(lowered, needle) != NULL;
}

static int contains_software_renderer(const char *renderer) {
    return renderer_contains(renderer, "llvmpipe") || renderer_contains(renderer, "softpipe") ||
           renderer_contains(renderer, "swrast");
}

static int renderer_matches_amd_path(const struct gpu_executor *executor) {
    int amd_renderer;
    if (executor == NULL || contains_software_renderer(executor->renderer))
        return 0;
    amd_renderer = renderer_contains(executor->renderer, "amd") ||
                   renderer_contains(executor->renderer, "radeon");
    if (strcmp(executor->driver, "virtio_gpu") == 0)
        return amd_renderer && renderer_contains(executor->renderer, "virgl");
    return strcmp(executor->driver, "amdgpu") == 0 && amd_renderer;
}

static int open_executor(struct gpu_executor *executor) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display;
    EGLConfig config;
    EGLint config_count = 0;
    EGLint major = 0;
    EGLint minor = 0;
    const EGLint config_attributes[] = {
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT_KHR, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE,
    };
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    drmVersionPtr version;
    const GLubyte *renderer;
    char node_path[64];
    static const GLfloat vertices[] = {
        -1.0F, -1.0F, 0.0F, 0.0F,
        1.0F, -1.0F, 1.0F, 0.0F,
        -1.0F, 1.0F, 0.0F, 1.0F,
        1.0F, 1.0F, 1.0F, 1.0F,
    };
    uint8_t source_pixels[GPU_SOURCE_WIDTH * GPU_SOURCE_HEIGHT * 4U];
    unsigned int x;
    unsigned int y;

    memset(executor, 0, sizeof(*executor));
    executor->drm_fd = -1;
    executor->display = EGL_NO_DISPLAY;
    executor->context = EGL_NO_CONTEXT;
    executor->surface = EGL_NO_SURFACE;
    executor->drm_fd = open_drm_render_node(node_path, sizeof(node_path));
    if (executor->drm_fd < 0)
        return -1;
    version = drmGetVersion(executor->drm_fd);
    if (version == NULL || version->name == NULL || version->name_len == 0) {
        if (version != NULL)
            drmFreeVersion(version);
        errno = ENODEV;
        return -1;
    }
    if ((size_t)version->name_len >= sizeof(executor->driver)) {
        drmFreeVersion(version);
        errno = EOVERFLOW;
        return -1;
    }
    memcpy(executor->driver, version->name, (size_t)version->name_len);
    executor->driver[version->name_len] = '\0';
    drmFreeVersion(version);
    if (strcmp(executor->driver, "virtio_gpu") != 0 && strcmp(executor->driver, "amdgpu") != 0) {
        errno = EOPNOTSUPP;
        return -1;
    }
    executor->gbm = gbm_create_device(executor->drm_fd);
    if (executor->gbm == NULL)
        return -1;
    get_platform_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (get_platform_display == NULL) {
        errno = ENOSYS;
        return -1;
    }
    executor->display = get_platform_display(EGL_PLATFORM_GBM_KHR, executor->gbm, NULL);
    if (executor->display == EGL_NO_DISPLAY || !eglInitialize(executor->display, &major, &minor) ||
        !eglBindAPI(EGL_OPENGL_ES_API) ||
        !eglChooseConfig(executor->display, config_attributes, &config, 1, &config_count) ||
        config_count != 1) {
        errno = EIO;
        return -1;
    }
    executor->context = eglCreateContext(executor->display, config, EGL_NO_CONTEXT,
                                         context_attributes);
    if (executor->context == EGL_NO_CONTEXT ||
        !eglMakeCurrent(executor->display, EGL_NO_SURFACE, EGL_NO_SURFACE, executor->context)) {
        errno = EIO;
        return -1;
    }
    renderer = glGetString(GL_RENDERER);
    if (renderer == NULL || strnlen((const char *)renderer, sizeof(executor->renderer)) >=
                                sizeof(executor->renderer)) {
        errno = EPROTO;
        return -1;
    }
    strcpy(executor->renderer, (const char *)renderer);
    if (!renderer_matches_amd_path(executor)) {
        errno = EOPNOTSUPP;
        return -1;
    }
    if (create_program(executor) != 0)
        return -1;

    glGenTextures(1, &executor->output_texture);
    glBindTexture(GL_TEXTURE_2D, executor->output_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, GPU_OUTPUT_WIDTH, GPU_OUTPUT_HEIGHT, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &executor->framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, executor->framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           executor->output_texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        errno = EIO;
        return -1;
    }

    for (y = 0U; y < GPU_SOURCE_HEIGHT; y++) {
        for (x = 0U; x < GPU_SOURCE_WIDTH; x++) {
            size_t offset = ((size_t)y * GPU_SOURCE_WIDTH + x) * 4U;
            int alternate = ((x / 8U) + (y / 8U)) % 2U;
            source_pixels[offset] = alternate ? 0x20U : 0xe0U;
            source_pixels[offset + 1U] = alternate ? 0xd0U : 0x30U;
            source_pixels[offset + 2U] = alternate ? 0x30U : 0xd0U;
            source_pixels[offset + 3U] = 0xffU;
        }
    }
    glGenTextures(1, &executor->source_texture);
    glBindTexture(GL_TEXTURE_2D, executor->source_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, GPU_SOURCE_WIDTH, GPU_SOURCE_HEIGHT, 0,
                 GL_BGRA_EXT, GL_UNSIGNED_BYTE, source_pixels);

    glGenVertexArrays(1, &executor->vertex_array);
    glBindVertexArray(executor->vertex_array);
    glGenBuffers(1, &executor->vertex_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, executor->vertex_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(GLfloat), (void *)0);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(GLfloat),
                          (void *)(2 * sizeof(GLfloat)));
    glEnableVertexAttribArray(1);
    glViewport(0, 0, GPU_OUTPUT_WIDTH, GPU_OUTPUT_HEIGHT);
    if (glGetError() != GL_NO_ERROR) {
        errno = EIO;
        return -1;
    }
    executor->source_acquire_fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0U);
    if (executor->source_acquire_fence == NULL) {
        errno = EIO;
        return -1;
    }
    glFlush();
    executor->source_acquire_value = 1U;
    return 0;
}

static void close_executor(struct gpu_executor *executor) {
    if (executor->display != EGL_NO_DISPLAY && executor->context != EGL_NO_CONTEXT)
        (void)eglMakeCurrent(executor->display, executor->surface, executor->surface,
                             executor->context);
    if (executor->source_acquire_fence != NULL)
        glDeleteSync(executor->source_acquire_fence);
    if (executor->vertex_buffer != 0U)
        glDeleteBuffers(1, &executor->vertex_buffer);
    if (executor->vertex_array != 0U)
        glDeleteVertexArrays(1, &executor->vertex_array);
    if (executor->framebuffer != 0U)
        glDeleteFramebuffers(1, &executor->framebuffer);
    if (executor->source_texture != 0U)
        glDeleteTextures(1, &executor->source_texture);
    if (executor->output_texture != 0U)
        glDeleteTextures(1, &executor->output_texture);
    if (executor->program != 0U)
        glDeleteProgram(executor->program);
    if (executor->display != EGL_NO_DISPLAY)
        (void)eglMakeCurrent(executor->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    if (executor->display != EGL_NO_DISPLAY && executor->surface != EGL_NO_SURFACE)
        (void)eglDestroySurface(executor->display, executor->surface);
    if (executor->display != EGL_NO_DISPLAY && executor->context != EGL_NO_CONTEXT)
        (void)eglDestroyContext(executor->display, executor->context);
    if (executor->display != EGL_NO_DISPLAY)
        (void)eglTerminate(executor->display);
    if (executor->gbm != NULL)
        gbm_device_destroy(executor->gbm);
    if (executor->drm_fd >= 0)
        close(executor->drm_fd);
    memset(executor, 0, sizeof(*executor));
    executor->drm_fd = -1;
    executor->display = EGL_NO_DISPLAY;
    executor->context = EGL_NO_CONTEXT;
    executor->surface = EGL_NO_SURFACE;
}

static void unpack_color(uint32_t rgba, GLfloat color[4]) {
    color[0] = (GLfloat)(rgba & 0xffU) / 255.0F;
    color[1] = (GLfloat)((rgba >> 8) & 0xffU) / 255.0F;
    color[2] = (GLfloat)((rgba >> 16) & 0xffU) / 255.0F;
    color[3] = (GLfloat)((rgba >> 24) & 0xffU) / 255.0F;
}

static int wait_source_acquire(struct gpu_executor *executor, uint64_t acquire_value,
                               uint64_t timeout_ns) {
    GLenum wait_result;
    if (executor == NULL || acquire_value == 0U || timeout_ns == 0U) {
        errno = EINVAL;
        return -1;
    }
    if (acquire_value <= executor->source_acquire_completed)
        return 0;
    if (acquire_value != executor->source_acquire_value || executor->source_acquire_fence == NULL) {
        errno = EPROTO;
        return -1;
    }
    wait_result = glClientWaitSync(executor->source_acquire_fence, GL_SYNC_FLUSH_COMMANDS_BIT,
                                   (GLuint64)timeout_ns);
    if (wait_result != GL_ALREADY_SIGNALED && wait_result != GL_CONDITION_SATISFIED) {
        errno = wait_result == GL_TIMEOUT_EXPIRED ? ETIMEDOUT : EIO;
        return -1;
    }
    glDeleteSync(executor->source_acquire_fence);
    executor->source_acquire_fence = NULL;
    executor->source_acquire_completed = acquire_value;
    return 0;
}

// Shader translation and host pipeline creation are one-time context setup,
// not admitted frame work. Prime the built-in pipeline once with a separate,
// bounded fence so the 16.667 ms frame SLA measures steady-state submissions.
static int prime_pipeline(struct gpu_executor *executor, uint64_t started_ns,
                          uint64_t *duration_ns) {
    GLsync fence;
    GLenum wait_result;
    uint64_t completed_ns;
    uint64_t elapsed_ns;
    uint64_t remaining_ns;
    if (executor == NULL || duration_ns == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (monotonic_ns(&completed_ns) != 0 || completed_ns < started_ns)
        return -1;
    elapsed_ns = completed_ns - started_ns;
    if (elapsed_ns >= (uint64_t)GPU_PIPELINE_PRIME_TIMEOUT_US * 1000ULL) {
        errno = ETIMEDOUT;
        return -1;
    }
    remaining_ns = (uint64_t)GPU_PIPELINE_PRIME_TIMEOUT_US * 1000ULL - elapsed_ns;
    if (wait_source_acquire(executor, 1U, remaining_ns) != 0)
        return -1;
    glBindFramebuffer(GL_FRAMEBUFFER, executor->framebuffer);
    glUseProgram(executor->program);
    glBindVertexArray(executor->vertex_array);
    glUniform2f(executor->output_size_uniform, (GLfloat)GPU_OUTPUT_WIDTH,
                (GLfloat)GPU_OUTPUT_HEIGHT);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, executor->source_texture);
    glUniform1i(glGetUniformLocation(executor->program, "u_source"), 0);
    glDisable(GL_BLEND);
    glClearColor(0.0F, 0.0F, 0.0F, 1.0F);
    glClear(GL_COLOR_BUFFER_BIT);
    glUniform4f(executor->rect_uniform, 0.0F, 0.0F, (GLfloat)GPU_OUTPUT_WIDTH,
                (GLfloat)GPU_OUTPUT_HEIGHT);
    glUniform4f(executor->color_uniform, 1.0F, 1.0F, 1.0F, 1.0F);
    glUniform4f(executor->transform_uniform, 0.0F, 0.0F, 0.0F, 0.0F);
    glUniform1i(executor->use_texture_uniform, 1);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    if (glGetError() != GL_NO_ERROR) {
        errno = EIO;
        return -1;
    }
    fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0U);
    if (fence == NULL) {
        errno = EIO;
        return -1;
    }
    glFlush();
    if (monotonic_ns(&completed_ns) != 0 || completed_ns < started_ns) {
        glDeleteSync(fence);
        return -1;
    }
    elapsed_ns = completed_ns - started_ns;
    if (elapsed_ns >= (uint64_t)GPU_PIPELINE_PRIME_TIMEOUT_US * 1000ULL) {
        glDeleteSync(fence);
        errno = ETIMEDOUT;
        return -1;
    }
    remaining_ns = (uint64_t)GPU_PIPELINE_PRIME_TIMEOUT_US * 1000ULL - elapsed_ns;
    wait_result = glClientWaitSync(fence, GL_SYNC_FLUSH_COMMANDS_BIT, (GLuint64)remaining_ns);
    glDeleteSync(fence);
    if (wait_result != GL_ALREADY_SIGNALED && wait_result != GL_CONDITION_SATISFIED) {
        errno = wait_result == GL_TIMEOUT_EXPIRED ? ETIMEDOUT : EIO;
        return -1;
    }
    if (monotonic_ns(&completed_ns) != 0 || completed_ns <= started_ns) {
        errno = EIO;
        return -1;
    }
    *duration_ns = completed_ns - started_ns;
    if (*duration_ns > (uint64_t)GPU_PIPELINE_PRIME_TIMEOUT_US * 1000ULL) {
        errno = ETIMEDOUT;
        return -1;
    }
    return 0;
}

static int execute_batch(struct gpu_executor *executor, const uint8_t *bytes, size_t length,
                         uint64_t *duration_ns) {
    struct gpu_batch_header header;
    struct gpu_source sources[GPU_MAX_SOURCES];
    uint8_t source_referenced[GPU_MAX_SOURCES];
    size_t required;
    size_t command_offset;
    uint32_t index;
    uint64_t aggregate_source_bytes = 0U;
    GLsync fence;
    GLenum wait_result;
    uint64_t started_ns;
    uint64_t completed_ns;
    if (executor == NULL || duration_ns == NULL ||
        parse_batch_header(bytes, length, &header, &required) != 0 || required != length ||
        header.submit_value != executor->expected_submit + 1U) {
        if (errno == 0)
            errno = EPROTO;
        return -1;
    }
    memset(source_referenced, 0, sizeof(source_referenced));
    for (index = 0U; index < header.source_count; index++) {
        uint64_t source_bytes;
        if (parse_source(bytes + GPU_HEADER_BYTES + index * GPU_SOURCE_BYTES, &sources[index]) !=
                0 ||
            sources[index].acquire_value != header.acquire_value) {
            errno = EPROTO;
            return -1;
        }
        source_bytes = (uint64_t)sources[index].stride_bytes * sources[index].height;
        if (UINT64_MAX - aggregate_source_bytes < source_bytes ||
            aggregate_source_bytes + source_bytes > GPU_MAX_BATCH_SOURCE_BYTES) {
            errno = EMSGSIZE;
            return -1;
        }
        aggregate_source_bytes += source_bytes;
    }
    if (wait_source_acquire(executor, header.acquire_value,
                            (uint64_t)header.budget_us * 1000ULL) != 0)
        return -1;
    command_offset = GPU_HEADER_BYTES + (size_t)header.source_count * GPU_SOURCE_BYTES;
    if (monotonic_ns(&started_ns) != 0)
        return -1;
    glBindFramebuffer(GL_FRAMEBUFFER, executor->framebuffer);
    glUseProgram(executor->program);
    glBindVertexArray(executor->vertex_array);
    glUniform2f(executor->output_size_uniform, (GLfloat)GPU_OUTPUT_WIDTH,
                (GLfloat)GPU_OUTPUT_HEIGHT);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, executor->source_texture);
    glUniform1i(glGetUniformLocation(executor->program, "u_source"), 0);
    for (index = 0U; index < header.command_count; index++) {
        struct gpu_command command;
        GLfloat color[4];
        if (parse_command(bytes + command_offset + index * GPU_COMMAND_BYTES,
                          header.source_count, &command) != 0)
            return -1;
        if ((index == 0U && command.kind != GPU_COMMAND_CLEAR) ||
            (index != 0U && command.kind == GPU_COMMAND_CLEAR)) {
            errno = EPROTO;
            return -1;
        }
        if (command.kind == GPU_COMMAND_TEXTURED_QUAD)
            source_referenced[command.source_index] = 1U;
        unpack_color(command.rgba, color);
        if (command.kind == GPU_COMMAND_CLEAR) {
            glDisable(GL_BLEND);
            glClearColor(color[0], color[1], color[2], color[3]);
            glClear(GL_COLOR_BUFFER_BIT);
            continue;
        }
        if (command.blend_mode == GPU_BLEND_SOURCE_OVER) {
            glEnable(GL_BLEND);
            glBlendEquation(GL_FUNC_ADD);
            if (command.kind == GPU_COMMAND_TEXTURED_QUAD)
                glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
            else
                glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
        } else {
            glDisable(GL_BLEND);
        }
        glUniform4f(executor->rect_uniform, (GLfloat)command.destination_x,
                    (GLfloat)command.destination_y, (GLfloat)command.destination_width,
                    (GLfloat)command.destination_height);
        glUniform4f(executor->color_uniform, color[0], color[1], color[2], color[3]);
        glUniform4f(executor->transform_uniform, (GLfloat)command.depth / 65536.0F,
                    (GLfloat)command.rotation / 65536.0F,
                    (GLfloat)command.tilt_x / 65536.0F,
                    (GLfloat)command.tilt_y / 65536.0F);
        glUniform1f(executor->perspective_uniform,
                    (GLfloat)command.perspective / 65536.0F);
        glUniform4f(executor->uv_rect_uniform,
                    (GLfloat)command.source_u / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_v / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_width / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_height / (GLfloat)UINT16_MAX);
        glUniform1i(executor->use_texture_uniform,
                    command.kind == GPU_COMMAND_TEXTURED_QUAD ? 1 : 0);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    for (index = 0U; index < header.source_count; index++) {
        if (source_referenced[index] == 0U) {
            errno = EPROTO;
            return -1;
        }
    }
    if (glGetError() != GL_NO_ERROR) {
        errno = EIO;
        return -1;
    }
    fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0U);
    if (fence == NULL) {
        errno = EIO;
        return -1;
    }
    glFlush();
    wait_result = glClientWaitSync(fence, GL_SYNC_FLUSH_COMMANDS_BIT,
                                   (GLuint64)header.budget_us * 1000ULL);
    glDeleteSync(fence);
    if (wait_result != GL_ALREADY_SIGNALED && wait_result != GL_CONDITION_SATISFIED) {
        errno = wait_result == GL_TIMEOUT_EXPIRED ? ETIMEDOUT : EIO;
        return -1;
    }
    if (monotonic_ns(&completed_ns) != 0 || completed_ns <= started_ns) {
        errno = EIO;
        return -1;
    }
    *duration_ns = completed_ns - started_ns;
    if (*duration_ns > (uint64_t)header.budget_us * 1000ULL) {
        errno = ETIMEDOUT;
        return -1;
    }
    executor->expected_submit = header.submit_value;
    return 0;
}

static size_t build_proof_batch(uint8_t *bytes, size_t capacity, uint64_t submit_value,
                                int32_t rotation) {
    const size_t length = GPU_HEADER_BYTES + GPU_SOURCE_BYTES + 3U * GPU_COMMAND_BYTES;
    uint8_t *source;
    uint8_t *clear;
    uint8_t *solid;
    uint8_t *textured;
    if (bytes == NULL || capacity < length || submit_value == 0U)
        return 0U;
    memset(bytes, 0, length);
    memcpy(bytes, GPU_BATCH_MAGIC, 8U);
    write_le32(bytes + 8U, GPU_BATCH_VERSION);
    write_le32(bytes + 12U, GPU_HEADER_BYTES);
    write_le32(bytes + 16U, GPU_COMMAND_BYTES);
    write_le32(bytes + 20U, 3U);
    write_le32(bytes + 24U, 1U);
    write_le32(bytes + 28U, 1U);
    write_le64(bytes + 32U, submit_value);
    write_le64(bytes + 40U, 1U);
    write_le32(bytes + 48U, GPU_MAX_BUDGET_US);
    write_le32(bytes + 52U, 1U);
    write_le32(bytes + 56U, GPU_BATCH_FLAG_PRESENT);

    source = bytes + GPU_HEADER_BYTES;
    write_le64(source, 1U);
    write_le64(source + 8U, 2U);
    write_le64(source + 16U, 1U);
    write_le32(source + 24U, GPU_SOURCE_WIDTH);
    write_le32(source + 28U, GPU_SOURCE_HEIGHT);
    write_le32(source + 32U, GPU_SOURCE_WIDTH * 4U);
    write_le32(source + 36U, GPU_PIXEL_FORMAT_BGRA8888);
    write_le32(source + 40U, GPU_SOURCE_REQUIRED_FLAGS);
    write_le32(source + 44U, 0U);
    write_le64(source + 48U, 1U);

    clear = source + GPU_SOURCE_BYTES;
    write_le32(clear, GPU_COMMAND_CLEAR);
    write_le32(clear + 8U, GPU_NO_SOURCE);
    write_le32(clear + 12U, GPU_BLEND_REPLACE);
    write_le32(clear + 40U, 0xff181010U);

    solid = clear + GPU_COMMAND_BYTES;
    write_le32(solid, GPU_COMMAND_SOLID_QUAD);
    write_le32(solid + 4U, GPU_COMMAND_FLAG_CLIP_OUTPUT);
    write_le32(solid + 8U, GPU_NO_SOURCE);
    write_le32(solid + 12U, GPU_BLEND_REPLACE);
    write_le32(solid + 16U, 8U);
    write_le32(solid + 20U, 8U);
    write_le32(solid + 24U, 48U);
    write_le32(solid + 28U, 48U);
    write_le32(solid + 40U, 0xff2020e0U);

    textured = solid + GPU_COMMAND_BYTES;
    write_le32(textured, GPU_COMMAND_TEXTURED_QUAD);
    write_le32(textured + 4U, GPU_COMMAND_FLAG_CLIP_OUTPUT);
    write_le32(textured + 8U, 0U);
    write_le32(textured + 12U, GPU_BLEND_SOURCE_OVER);
    write_le32(textured + 16U, 72U);
    write_le32(textured + 20U, 24U);
    write_le32(textured + 24U, 48U);
    write_le32(textured + 28U, 80U);
    write_le16(textured + 36U, UINT16_MAX);
    write_le16(textured + 38U, UINT16_MAX);
    write_le32(textured + 40U, UINT32_MAX);
    write_le32(textured + 44U, 8192U);
    write_le32(textured + 48U, (uint32_t)rotation);
    write_le32(textured + 52U, 4096U);
    write_le32(textured + 56U, (uint32_t)-2048);
    write_le32(textured + 60U, 2048U);
    return length;
}

static int contract_negative_selftest(void) {
    uint8_t batch[GPU_HEADER_BYTES + GPU_SOURCE_BYTES + 3U * GPU_COMMAND_BYTES];
    uint8_t mutated[sizeof(batch)];
    struct gpu_batch_header header;
    struct gpu_source source;
    struct gpu_command command;
    size_t required;
    size_t length = build_proof_batch(batch, sizeof(batch), 1U, 0);
    size_t command_offset = GPU_HEADER_BYTES + GPU_SOURCE_BYTES;
    if (length != sizeof(batch) || parse_batch_header(batch, length, &header, &required) != 0 ||
        required != length || parse_source(batch + GPU_HEADER_BYTES, &source) != 0 ||
        parse_command(batch + command_offset, header.source_count, &command) != 0) {
        errno = EPROTO;
        return -1;
    }

    memcpy(mutated, batch, length);
    mutated[0] ^= 1U;
    if (parse_batch_header(mutated, length, &header, &required) == 0)
        goto false_accept;
    memcpy(mutated, batch, length);
    write_le32(mutated + 40U + GPU_HEADER_BYTES, 0U);
    if (parse_source(mutated + GPU_HEADER_BYTES, &source) == 0)
        goto false_accept;
    memcpy(mutated, batch, length);
    write_le32(mutated + command_offset, 99U);
    if (parse_command(mutated + command_offset, 1U, &command) == 0)
        goto false_accept;
    memcpy(mutated, batch, length);
    write_le32(mutated + command_offset + GPU_COMMAND_BYTES + 24U, GPU_OUTPUT_WIDTH + 1U);
    if (parse_command(mutated + command_offset + GPU_COMMAND_BYTES, 1U, &command) == 0)
        goto false_accept;
    memcpy(mutated, batch, length);
    write_le64(mutated + 40U, 2U);
    if (parse_batch_header(mutated, length, &header, &required) == 0)
        goto false_accept;
    return 0;

false_accept:
    errno = EPROTO;
    return -1;
}

static uint64_t fnv1a64(const uint8_t *bytes, size_t length) {
    uint64_t hash = 1469598103934665603ULL;
    size_t index;
    for (index = 0U; index < length; index++) {
        hash ^= bytes[index];
        hash *= 1099511628211ULL;
    }
    return hash;
}

static int verify_output(uint64_t *frame_hash) {
    uint8_t *pixels;
    size_t length = GPU_OUTPUT_WIDTH * GPU_OUTPUT_HEIGHT * 4U;
    size_t background = 0U;
    size_t solid = ((size_t)16U * GPU_OUTPUT_WIDTH + 16U) * 4U;
    size_t textured = ((size_t)64U * GPU_OUTPUT_WIDTH + 96U) * 4U;
    pixels = malloc(length);
    if (pixels == NULL)
        return -1;
    glReadPixels(0, 0, GPU_OUTPUT_WIDTH, GPU_OUTPUT_HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    if (glGetError() != GL_NO_ERROR || pixels[background] != 0x10U ||
        pixels[background + 1U] != 0x10U || pixels[background + 2U] != 0x18U ||
        pixels[background + 3U] != 0xffU || pixels[solid] < 0xc0U ||
        pixels[solid + 1U] > 0x40U || pixels[solid + 2U] > 0x40U ||
        (pixels[textured] == pixels[background] &&
         pixels[textured + 1U] == pixels[background + 1U] &&
         pixels[textured + 2U] == pixels[background + 2U])) {
        free(pixels);
        errno = EILSEQ;
        return -1;
    }
    *frame_hash = fnv1a64(pixels, length);
    free(pixels);
    return 0;
}

static int write_all(int fd, const void *buffer, size_t length) {
    const uint8_t *cursor = buffer;
    while (length != 0U) {
        ssize_t written = write(fd, cursor, length);
        if (written < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        cursor += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static void sanitize(char *text) {
    size_t index;
    for (index = 0U; text[index] != '\0'; index++) {
        char byte = text[index];
        if (!((byte >= 'a' && byte <= 'z') || (byte >= 'A' && byte <= 'Z') ||
              (byte >= '0' && byte <= '9') || byte == '_' || byte == '-' || byte == '.'))
            text[index] = '_';
    }
}

// Publish the bounded pipeline-prime result before the longer steady-state
// proof.  The init script uses this independently so a blocked EGL/GL setup
// cannot be mistaken for a merely slow first frame.
static int publish_prime_evidence(const struct gpu_executor *executor, uint64_t prime_us) {
    char evidence[512];
    char renderer[sizeof(executor->renderer)];
    int length;
    int fd;
    int saved;
    strcpy(renderer, executor->renderer);
    sanitize(renderer);
    length = snprintf(evidence, sizeof(evidence),
                      "GPU_PIPELINE_PRIME_SCHEMA=1\nCONTRACT_VERSION=1\nDRM_DRIVER=%s\n"
                      "GL_RENDERER=%s\nGPU_PIPELINE_PRIME_US=%llu\n"
                      "GPU_PIPELINE_PRIME_TIMEOUT_US=%u\nEXPLICIT_ACQUIRE_FENCE=yes\n",
                      executor->driver, renderer, (unsigned long long)prime_us,
                      GPU_PIPELINE_PRIME_TIMEOUT_US);
    if (length <= 0 || (size_t)length >= sizeof(evidence)) {
        errno = EOVERFLOW;
        return -1;
    }
    fd = open(GPU_PRIME_EVIDENCE_TEMP, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW,
              0600);
    if (fd < 0)
        return -1;
    if (fchmod(fd, 0600) != 0 || write_all(fd, evidence, (size_t)length) != 0 || fsync(fd) != 0) {
        saved = errno == 0 ? EIO : errno;
        close(fd);
        unlink(GPU_PRIME_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    if (close(fd) != 0 || rename(GPU_PRIME_EVIDENCE_TEMP, GPU_PRIME_EVIDENCE) != 0) {
        saved = errno == 0 ? EIO : errno;
        unlink(GPU_PRIME_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    return 0;
}

static int publish_evidence(const struct gpu_executor *executor, uint64_t prime_us,
                            uint64_t fps_milli, uint64_t average_us, uint64_t maximum_us,
                            uint64_t wall_maximum_us, uint64_t frame_hash_a,
                            uint64_t frame_hash_b, int performance_target_met) {
    char evidence[1024];
    char renderer[sizeof(executor->renderer)];
    int length;
    int fd;
    int saved;
    strcpy(renderer, executor->renderer);
    sanitize(renderer);
    length = snprintf(
        evidence, sizeof(evidence),
        "GPU_COMPOSITOR_EVIDENCE_SCHEMA=1\nCONTRACT_VERSION=1\nDRM_DRIVER=%s\n"
        "GL_RENDERER=%s\nFIXED_COMMANDS=clear,solid-quad,textured-quad\n"
        "EXPLICIT_GPU_FENCE=yes\nEXPLICIT_ACQUIRE_FENCE=yes\nRAW_COMMANDS=no\nAPPLICATION_SHADERS=no\n"
        "NEGATIVE_CONTRACT_CASES=5\n"
        "DEVICE_WRITE_TO_RUSTOS_SOURCE=no\nSOFTWARE_RENDERER=no\nPROOF_FRAMES=%u\n"
        "GPU_PIPELINE_PRIME_US=%llu\nGPU_PIPELINE_PRIME_TIMEOUT_US=%u\n"
        "GPU_COMPLETION_FPS_MILLI=%llu\nGPU_COMPLETION_US_AVG=%llu\n"
        "GPU_COMPLETION_US_MAX=%llu\nWALL_FRAME_US_MAX=%llu\nPERFORMANCE_TARGET_MET=%s\n"
        "PROOF_SCHEDULER_POLICY=rr\nPROOF_SCHEDULER_PRIORITY=%u\n"
        "PROOF_RTTIME_SOFT_US=%u\nPROOF_RTTIME_HARD_US=%u\n"
        "PROOF_RTTIME_HARD_ACTION=terminate\n"
        "PROOF_SCHEDULER_RESTORED=normal\n"
        "FRAME_HASH_A=%016llx\n"
        "FRAME_HASH_B=%016llx\nFRAME_HASH_STABLE=yes\nFRAME_HASH_DYNAMIC=yes\n"
        "SOURCE_MODE=synthetic-read-only-contract\nPUBLIC_USERSPACE_ABI=no\n"
        "RUSTOS_UI_CONNECTED=no\nSCANOUT_CONNECTED=no\nPHYSICAL_ZERO_COPY=no\n",
        executor->driver, renderer, GPU_PROOF_FRAMES, (unsigned long long)prime_us,
        GPU_PIPELINE_PRIME_TIMEOUT_US, (unsigned long long)fps_milli,
        (unsigned long long)average_us, (unsigned long long)maximum_us,
        (unsigned long long)wall_maximum_us, performance_target_met ? "yes" : "no",
        GPU_PROOF_RR_PRIORITY, GPU_PROOF_RTTIME_SOFT_US, GPU_PROOF_RTTIME_HARD_US,
        (unsigned long long)frame_hash_a,
        (unsigned long long)frame_hash_b);
    if (length <= 0 || (size_t)length >= sizeof(evidence)) {
        errno = EOVERFLOW;
        return -1;
    }
    fd = open(GPU_EVIDENCE_TEMP, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0)
        return -1;
    if (fchmod(fd, 0600) != 0 || write_all(fd, evidence, (size_t)length) != 0 || fsync(fd) != 0) {
        saved = errno == 0 ? EIO : errno;
        close(fd);
        unlink(GPU_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    if (close(fd) != 0 || rename(GPU_EVIDENCE_TEMP, GPU_EVIDENCE) != 0) {
        saved = errno == 0 ? EIO : errno;
        unlink(GPU_EVIDENCE_TEMP);
        errno = saved;
        return -1;
    }
    return 0;
}

static int run_initial_proof(struct gpu_executor *executor, uint64_t *fps_milli,
                             uint64_t *average_us, uint64_t *maximum_us,
                             uint64_t *wall_maximum_us, uint64_t *frame_hash_a,
                             uint64_t *frame_hash_b, int *performance_target_met) {
    uint8_t batch[GPU_HEADER_BYTES + GPU_SOURCE_BYTES + 3U * GPU_COMMAND_BYTES];
    uint64_t total_ns = 0U;
    uint64_t max_ns = 0U;
    uint64_t wall_started_ns;
    uint64_t wall_previous_ns;
    uint64_t wall_completed_ns;
    uint64_t wall_total_ns;
    uint64_t wall_max_ns = 0U;
    uint64_t repeated_hash;
    uint64_t duration_ns;
    unsigned int frame;
    size_t length;
    for (frame = 0U; frame < GPU_WARMUP_FRAMES; frame++) {
        length = build_proof_batch(batch, sizeof(batch), executor->expected_submit + 1U,
                                   (frame & 1U) == 0U ? 4096 : -4096);
        if (length == 0U || execute_batch(executor, batch, length, &duration_ns) != 0) {
            int saved = errno == 0 ? EIO : errno;
            fprintf(stderr, "rustos-dvm-gpu: proof stage=warmup frame=%u errno=%d\n", frame,
                    saved);
            errno = saved;
            return -1;
        }
    }
    length = build_proof_batch(batch, sizeof(batch), executor->expected_submit + 1U, 4096);
    if (length == 0U || execute_batch(executor, batch, length, &duration_ns) != 0 ||
        verify_output(frame_hash_a) != 0) {
        errno = errno == 0 ? EIO : errno;
        return -1;
    }
    length = build_proof_batch(batch, sizeof(batch), executor->expected_submit + 1U, -4096);
    if (length == 0U || execute_batch(executor, batch, length, &duration_ns) != 0 ||
        verify_output(frame_hash_b) != 0) {
        errno = errno == 0 ? EIO : errno;
        return -1;
    }
    length = build_proof_batch(batch, sizeof(batch), executor->expected_submit + 1U, 4096);
    if (length == 0U || execute_batch(executor, batch, length, &duration_ns) != 0 ||
        verify_output(&repeated_hash) != 0 || repeated_hash != *frame_hash_a ||
        *frame_hash_a == *frame_hash_b) {
        errno = EILSEQ;
        return -1;
    }
    if (monotonic_ns(&wall_started_ns) != 0)
        return -1;
    wall_previous_ns = wall_started_ns;
    for (frame = 0U; frame < GPU_PROOF_FRAMES; frame++) {
        length = build_proof_batch(batch, sizeof(batch), executor->expected_submit + 1U,
                                   (frame & 1U) == 0U ? 4096 : -4096);
        if (length == 0U || execute_batch(executor, batch, length, &duration_ns) != 0) {
            int saved = errno == 0 ? EIO : errno;
            fprintf(stderr, "rustos-dvm-gpu: proof stage=measured frame=%u errno=%d\n", frame,
                    saved);
            errno = saved;
            return -1;
        }
        if (monotonic_ns(&wall_completed_ns) != 0 || wall_completed_ns <= wall_previous_ns) {
            errno = EIO;
            return -1;
        }
        if (UINT64_MAX - total_ns < duration_ns) {
            errno = EOVERFLOW;
            return -1;
        }
        total_ns += duration_ns;
        if (duration_ns > max_ns)
            max_ns = duration_ns;
        if (wall_completed_ns - wall_previous_ns > wall_max_ns)
            wall_max_ns = wall_completed_ns - wall_previous_ns;
        wall_previous_ns = wall_completed_ns;
    }
    if (wall_previous_ns <= wall_started_ns || total_ns == 0U ||
        verify_output(&repeated_hash) != 0 || repeated_hash != *frame_hash_b)
        return -1;
    wall_total_ns = wall_previous_ns - wall_started_ns;
    *fps_milli = (uint64_t)GPU_PROOF_FRAMES * 1000000000000ULL / wall_total_ns;
    *average_us = (total_ns / GPU_PROOF_FRAMES + 999U) / 1000U;
    *maximum_us = (max_ns + 999U) / 1000U;
    *wall_maximum_us = (wall_max_ns + 999U) / 1000U;
    *performance_target_met = *fps_milli >= GPU_MIN_FPS_MILLI &&
        *maximum_us <= GPU_FRAME_TARGET_US && *wall_maximum_us <= GPU_FRAME_TARGET_US;
    return 0;
}

static int serve(void) {
    struct gpu_executor executor;
    struct proof_scheduler_guard proof_scheduler = {0};
    uint8_t batch[GPU_HEADER_BYTES + GPU_SOURCE_BYTES + 3U * GPU_COMMAND_BYTES];
    uint64_t fps_milli;
    uint64_t prime_started_ns;
    uint64_t prime_ns;
    uint64_t average_us;
    uint64_t maximum_us;
    uint64_t wall_maximum_us;
    uint64_t frame_hash_a;
    uint64_t frame_hash_b;
    uint64_t duration_ns;
    uint64_t health_sequence = 0U;
    int performance_target_met = 0;
    size_t length;
    signal(SIGTERM, request_stop);
    signal(SIGINT, request_stop);
    unlink(GPU_EVIDENCE);
    unlink(GPU_EVIDENCE_TEMP);
    unlink(GPU_PRIME_EVIDENCE);
    unlink(GPU_PRIME_EVIDENCE_TEMP);
    if (contract_negative_selftest() != 0) {
        fprintf(stderr, "rustos-dvm-gpu: contract negative selftest failed errno=%d\n", errno);
        return -1;
    }
    if (monotonic_ns(&prime_started_ns) != 0 || open_executor(&executor) != 0) {
        fprintf(stderr, "rustos-dvm-gpu: executor unavailable errno=%d\n", errno);
        close_executor(&executor);
        return -1;
    }
    if (prime_pipeline(&executor, prime_started_ns, &prime_ns) != 0) {
        fprintf(stderr, "rustos-dvm-gpu: pipeline prime failed driver=%s renderer=%s errno=%d\n",
                executor.driver, executor.renderer, errno);
        close_executor(&executor);
        return -1;
    }
    if (publish_prime_evidence(&executor, (prime_ns + 999U) / 1000U) != 0) {
        fprintf(stderr, "rustos-dvm-gpu: pipeline prime evidence unavailable errno=%d\n", errno);
        close_executor(&executor);
        unlink(GPU_PRIME_EVIDENCE);
        return -1;
    }
    if (proof_scheduler_enter(&proof_scheduler) != 0) {
        fprintf(stderr,
                "rustos-dvm-gpu: proof scheduler unavailable policy=rr priority=%u errno=%d\n",
                GPU_PROOF_RR_PRIORITY, errno);
        close_executor(&executor);
        unlink(GPU_PRIME_EVIDENCE);
        return -1;
    }
    if (run_initial_proof(&executor, &fps_milli, &average_us, &maximum_us,
                          &wall_maximum_us, &frame_hash_a, &frame_hash_b,
                          &performance_target_met) != 0) {
        int saved = errno == 0 ? EIO : errno;
        if (proof_scheduler_leave(&proof_scheduler) != 0)
            fprintf(stderr, "rustos-dvm-gpu: proof scheduler restore failed errno=%d\n", errno);
        fprintf(stderr, "rustos-dvm-gpu: proof failed driver=%s renderer=%s errno=%d\n",
                executor.driver, executor.renderer, saved);
        close_executor(&executor);
        unlink(GPU_EVIDENCE);
        unlink(GPU_PRIME_EVIDENCE);
        errno = saved;
        return -1;
    }
    if (proof_scheduler_leave(&proof_scheduler) != 0) {
        fprintf(stderr, "rustos-dvm-gpu: proof scheduler restore failed errno=%d\n", errno);
        close_executor(&executor);
        unlink(GPU_EVIDENCE);
        unlink(GPU_PRIME_EVIDENCE);
        return -1;
    }
    if (publish_evidence(&executor, (prime_ns + 999U) / 1000U, fps_milli, average_us,
                         maximum_us, wall_maximum_us, frame_hash_a, frame_hash_b,
                         performance_target_met) != 0) {
        fprintf(stderr, "rustos-dvm-gpu: evidence publish failed errno=%d\n", errno);
        close_executor(&executor);
        unlink(GPU_EVIDENCE);
        unlink(GPU_PRIME_EVIDENCE);
        return -1;
    }
    fprintf(stderr,
            "rustos-dvm-gpu: ready contract=1 driver=%s renderer=%s commands=3 "
            "gpu-fence=1 acquire-fence=1 prime_us=%llu frames=%u fps_milli=%llu "
            "avg_us=%llu max_us=%llu wall_max_us=%llu frame_hash_a=%016llx "
            "frame_hash_b=%016llx hash-stable=1 hash-dynamic=1 negative=5 software=0 "
            "scheduler=rr priority=%u rttime-soft-us=%u rttime-hard-us=%u "
            "rttime-hard-action=terminate "
            "scheduler-restored=normal "
            "performance-target=%d hardware=amd scope-public-abi=0 scope-ui-connected=0 "
            "scope-scanout=0\n",
            executor.driver, executor.renderer, (unsigned long long)((prime_ns + 999U) / 1000U),
            GPU_PROOF_FRAMES,
            (unsigned long long)fps_milli, (unsigned long long)average_us,
            (unsigned long long)maximum_us, (unsigned long long)wall_maximum_us,
            (unsigned long long)frame_hash_a, (unsigned long long)frame_hash_b,
            GPU_PROOF_RR_PRIORITY, GPU_PROOF_RTTIME_SOFT_US, GPU_PROOF_RTTIME_HARD_US,
            performance_target_met);
    while (!stop_requested) {
        sleep(GPU_HEALTH_INTERVAL_SECONDS);
        if (stop_requested)
            break;
        length = build_proof_batch(batch, sizeof(batch), executor.expected_submit + 1U, 2048);
        if (length == 0U || execute_batch(&executor, batch, length, &duration_ns) != 0) {
            fprintf(stderr, "rustos-dvm-gpu: context lost errno=%d\n", errno);
            unlink(GPU_EVIDENCE);
            unlink(GPU_PRIME_EVIDENCE);
            close_executor(&executor);
            return -1;
        }
        health_sequence++;
        fprintf(stderr,
                "rustos-dvm-gpu: health sequence=%llu completion_us=%llu acquire-fence=1\n",
                (unsigned long long)health_sequence,
                (unsigned long long)((duration_ns + 999U) / 1000U));
    }
    unlink(GPU_EVIDENCE);
    unlink(GPU_EVIDENCE_TEMP);
    unlink(GPU_PRIME_EVIDENCE);
    unlink(GPU_PRIME_EVIDENCE_TEMP);
    close_executor(&executor);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2 || strcmp(argv[1], "serve") != 0) {
        fprintf(stderr, "usage: %s serve\n", argv[0]);
        return EXIT_FAILURE;
    }
    return serve() == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
