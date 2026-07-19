// SPDX-License-Identifier: MIT
#define _GNU_SOURCE

#include "rustos-dvm-gpu-runtime.h"

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

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
#define GPU_MAX_COMMANDS 512U
#define GPU_MAX_BATCH_BYTES (GPU_HEADER_BYTES + GPU_SOURCE_BYTES + GPU_MAX_COMMANDS * GPU_COMMAND_BYTES)
#define GPU_MAX_BUDGET_US 50000U
#define GPU_MAX_DIMENSION 8192U
#define GPU_MAX_SOURCE_BYTES (256ULL * 1024ULL * 1024ULL)
#define GPU_BATCH_FLAG_PRESENT 1U
#define GPU_SOURCE_REQUIRED_FLAGS 3U
#define GPU_PIXEL_FORMAT_BGRA8888 1U
#define GPU_NO_SOURCE UINT32_MAX
#define GPU_COMMAND_CLEAR 1U
#define GPU_COMMAND_SOLID_QUAD 2U
#define GPU_COMMAND_TEXTURED_QUAD 3U
#define GPU_BLEND_REPLACE 1U
#define GPU_BLEND_SOURCE_OVER 2U
#define GPU_COMMAND_FLAG_CLIP_OUTPUT 1U
#define GPU_TRANSFORM_LIMIT (4 * 65536)
#define GPU_OUTPUT_COUNT 3U

typedef void (*rustos_gl_egl_image_target_texture_fn)(GLenum target, void *image);

struct gpu_batch_header {
    uint32_t command_count;
    uint32_t context_id;
    uint32_t context_epoch;
    uint64_t submit_value;
    uint64_t acquire_value;
    uint32_t budget_us;
};

struct gpu_source {
    uint64_t token;
    uint64_t generation;
    uint64_t acquire_value;
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
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

struct gpu_output {
    struct gbm_bo *bo;
    uint32_t framebuffer_id;
};

struct rustos_gpu_runtime {
    int drm_fd;
    int addfb2_modifiers;
    struct gbm_device *gbm;
    struct gbm_surface *gbm_surface;
    EGLDisplay egl_display;
    EGLContext egl_context;
    EGLSurface egl_surface;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLWAITSYNCKHRPROC wait_sync;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_native_fence;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    rustos_gl_egl_image_target_texture_fn image_target_texture;
    GLuint program;
    GLuint source_texture;
    GLuint dmabuf_source_textures[GPU_OUTPUT_COUNT];
    EGLImageKHR dmabuf_source_images[GPU_OUTPUT_COUNT];
    int dmabuf_sources_ready;
    GLuint vertex_buffer;
    GLuint vertex_array;
    GLint rect_uniform;
    GLint output_size_uniform;
    GLint color_uniform;
    GLint transform_uniform;
    GLint perspective_uniform;
    GLint uv_rect_uniform;
    GLint use_texture_uniform;
    uint32_t output_width;
    uint32_t output_height;
    uint32_t atlas_width;
    uint32_t atlas_height;
    uint32_t atlas_stride_bytes;
    uint64_t expected_submit;
    uint64_t completed_acquire;
    const char *stage;
    uint64_t last_content_epoch;
    uint64_t last_generation;
    uint64_t last_sequence;
    uint32_t context_id;
    uint32_t context_epoch;
    struct gpu_output outputs[GPU_OUTPUT_COUNT];
    uint32_t front_output;
    char driver[64];
    char renderer[160];
};

static uint16_t read_le16(const uint8_t *bytes) {
    return (uint16_t)bytes[0] | (uint16_t)((uint16_t)bytes[1] << 8);
}

static uint32_t read_le32(const uint8_t *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
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

static int monotonic_ns(uint64_t *value) {
    struct timespec now;
    if (value == NULL || clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0) {
        errno = EIO;
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
    return 0;
}

static int extension_present(const char *extensions, const char *name) {
    size_t name_len;
    const char *cursor;
    if (extensions == NULL || name == NULL || *name == '\0' || strchr(name, ' ') != NULL)
        return 0;
    name_len = strlen(name);
    cursor = extensions;
    while ((cursor = strstr(cursor, name)) != NULL) {
        if ((cursor == extensions || cursor[-1] == ' ') &&
            (cursor[name_len] == '\0' || cursor[name_len] == ' '))
            return 1;
        cursor += name_len;
    }
    return 0;
}

static int contains_software_renderer(const char *renderer) {
    char lowered[160];
    size_t index;
    size_t length;
    if (renderer == NULL)
        return 1;
    length = strnlen(renderer, sizeof(lowered));
    if (length == sizeof(lowered))
        return 1;
    for (index = 0U; index < length; index++) {
        char byte = renderer[index];
        lowered[index] = byte >= 'A' && byte <= 'Z' ? (char)(byte - 'A' + 'a') : byte;
    }
    lowered[length] = '\0';
    return strstr(lowered, "llvmpipe") != NULL || strstr(lowered, "softpipe") != NULL ||
           strstr(lowered, "swrast") != NULL;
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
        glDeleteShader(shader);
        errno = EPROTO;
        return 0U;
    }
    return shader;
}

static int create_program(struct rustos_gpu_runtime *runtime) {
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
        " float c=cos(u_transform.y); float s=sin(u_transform.y);\n"
        " vec2 rotated=mat2(c,-s,s,c)*a_position;\n"
        " float z=u_transform.x+a_position.x*u_transform.z+a_position.y*u_transform.w;\n"
        " float p=max(0.25,1.0+z*u_perspective);\n"
        " vec2 local=rotated/p;\n"
        " vec2 pixel=u_rect.xy+(local*0.5+0.5)*u_rect.zw;\n"
        " vec2 clip=vec2(pixel.x/u_output_size.x*2.0-1.0,1.0-pixel.y/u_output_size.y*2.0);\n"
        " gl_Position=vec4(clip,clamp(z,-1.0,1.0),1.0); v_uv=a_uv;\n"
        "}\n";
    static const char fragment_source[] =
        "#version 300 es\n"
        "precision highp float;\n"
        "in vec2 v_uv; uniform sampler2D u_source; uniform vec4 u_uv_rect;\n"
        "uniform vec4 u_color; uniform int u_use_texture; out vec4 out_color;\n"
        "void main(){ vec2 uv=u_uv_rect.xy+v_uv*u_uv_rect.zw;\n"
        " vec4 sampled=u_use_texture!=0?texture(u_source,uv):vec4(1.0);\n"
        " out_color=sampled*u_color; }\n";
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
    runtime->program = glCreateProgram();
    glAttachShader(runtime->program, vertex);
    glAttachShader(runtime->program, fragment);
    glLinkProgram(runtime->program);
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    glGetProgramiv(runtime->program, GL_LINK_STATUS, &linked);
    if (linked != GL_TRUE) {
        errno = EPROTO;
        return -1;
    }
    runtime->rect_uniform = glGetUniformLocation(runtime->program, "u_rect");
    runtime->output_size_uniform = glGetUniformLocation(runtime->program, "u_output_size");
    runtime->color_uniform = glGetUniformLocation(runtime->program, "u_color");
    runtime->transform_uniform = glGetUniformLocation(runtime->program, "u_transform");
    runtime->perspective_uniform = glGetUniformLocation(runtime->program, "u_perspective");
    runtime->uv_rect_uniform = glGetUniformLocation(runtime->program, "u_uv_rect");
    runtime->use_texture_uniform = glGetUniformLocation(runtime->program, "u_use_texture");
    if (runtime->rect_uniform < 0 || runtime->output_size_uniform < 0 ||
        runtime->color_uniform < 0 || runtime->transform_uniform < 0 ||
        runtime->perspective_uniform < 0 || runtime->uv_rect_uniform < 0 ||
        runtime->use_texture_uniform < 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int select_egl_config(EGLDisplay display, EGLConfig *selected) {
    EGLConfig configs[64];
    EGLint count = 0;
    EGLint index;
    const EGLint attributes[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT_KHR,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_NONE,
    };
    if (!eglChooseConfig(display, attributes, configs, 64, &count) || count <= 0) {
        errno = EOPNOTSUPP;
        return -1;
    }
    for (index = 0; index < count; index++) {
        EGLint visual = 0;
        if (eglGetConfigAttrib(display, configs[index], EGL_NATIVE_VISUAL_ID, &visual) &&
            (uint32_t)visual == GBM_FORMAT_XRGB8888) {
            *selected = configs[index];
            return 0;
        }
    }
    errno = EOPNOTSUPP;
    return -1;
}

static int add_framebuffer(struct rustos_gpu_runtime *runtime, struct gbm_bo *bo,
                           uint32_t *framebuffer_id) {
    uint32_t handles[4] = {0U};
    uint32_t strides[4] = {0U};
    uint32_t offsets[4] = {0U};
    uint64_t modifiers[4] = {0U};
    uint64_t modifier;
    if (gbm_bo_get_plane_count(bo) != 1) {
        errno = EOPNOTSUPP;
        return -1;
    }
    handles[0] = gbm_bo_get_handle_for_plane(bo, 0).u32;
    strides[0] = gbm_bo_get_stride_for_plane(bo, 0);
    offsets[0] = gbm_bo_get_offset(bo, 0);
    modifier = gbm_bo_get_modifier(bo);
    if (handles[0] == 0U || strides[0] < runtime->output_width * 4U) {
        errno = EPROTO;
        return -1;
    }
    if (modifier != DRM_FORMAT_MOD_INVALID && runtime->addfb2_modifiers) {
        runtime->stage = "gpu-prime-addfb2-modifier";
        modifiers[0] = modifier;
        return drmModeAddFB2WithModifiers(runtime->drm_fd, runtime->output_width,
                                          runtime->output_height, DRM_FORMAT_XRGB8888,
                                          handles, strides, offsets, modifiers,
                                          framebuffer_id, DRM_MODE_FB_MODIFIERS);
    }
    if (modifier != DRM_FORMAT_MOD_INVALID && modifier != DRM_FORMAT_MOD_LINEAR) {
        runtime->stage = "gpu-prime-addfb2-unrepresentable-modifier";
        errno = EOPNOTSUPP;
        return -1;
    }
    runtime->stage = "gpu-prime-addfb2-linear";
    return drmModeAddFB2(runtime->drm_fd, runtime->output_width, runtime->output_height,
                         DRM_FORMAT_XRGB8888, handles, strides, offsets,
                         framebuffer_id, 0U);
}

static int lock_output(struct rustos_gpu_runtime *runtime, struct rustos_gpu_frame *frame) {
    struct gbm_bo *bo = gbm_surface_lock_front_buffer(runtime->gbm_surface);
    uint32_t index;
    if (bo == NULL) {
        EGLint egl_error = eglGetError();
        runtime->stage = egl_error == EGL_BAD_SURFACE
            ? "gpu-prime-no-front-buffer"
            : "gpu-prime-lock-output";
        errno = egl_error == EGL_BAD_SURFACE ? EPROTO : EIO;
        return -1;
    }
    for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
        if (runtime->outputs[index].bo == bo)
            break;
    }
    if (index == GPU_OUTPUT_COUNT) {
        for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
            if (runtime->outputs[index].bo == NULL)
                break;
        }
        if (index == GPU_OUTPUT_COUNT) {
            gbm_surface_release_buffer(runtime->gbm_surface, bo);
            errno = EBUSY;
            return -1;
        }
        if (add_framebuffer(runtime, bo, &runtime->outputs[index].framebuffer_id) != 0) {
            int saved = errno == 0 ? EIO : errno;
            gbm_surface_release_buffer(runtime->gbm_surface, bo);
            errno = saved;
            return -1;
        }
        runtime->outputs[index].bo = bo;
    }
    frame->framebuffer_id = runtime->outputs[index].framebuffer_id;
    frame->output_index = index;
    return 0;
}

static int native_fence(struct rustos_gpu_runtime *runtime) {
    const EGLint attributes[] = {
        EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID,
        EGL_NONE,
    };
    EGLSyncKHR sync = runtime->create_sync(runtime->egl_display,
                                           EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    int fd;
    if (sync == EGL_NO_SYNC_KHR) {
        errno = EIO;
        return -1;
    }
    glFlush();
    fd = runtime->dup_native_fence(runtime->egl_display, sync);
    (void)runtime->destroy_sync(runtime->egl_display, sync);
    if (fd == EGL_NO_NATIVE_FENCE_FD_ANDROID) {
        errno = EIO;
        return -1;
    }
    return fd;
}

static int fixed_bounded(int32_t value) {
    return value != INT32_MIN && value >= -GPU_TRANSFORM_LIMIT && value <= GPU_TRANSFORM_LIMIT;
}

static int parse_batch(const struct rustos_gpu_runtime *runtime, const uint8_t *bytes,
                       size_t length, struct gpu_batch_header *header,
                       struct gpu_source *source) {
    uint64_t source_bytes;
    size_t required;
    if (runtime == NULL || bytes == NULL || header == NULL || source == NULL ||
        length < GPU_HEADER_BYTES + GPU_SOURCE_BYTES || length > GPU_MAX_BATCH_BYTES ||
        memcmp(bytes, GPU_BATCH_MAGIC, 8U) != 0 ||
        read_le32(bytes + 8U) != GPU_BATCH_VERSION ||
        read_le32(bytes + 12U) != GPU_HEADER_BYTES ||
        read_le32(bytes + 16U) != GPU_COMMAND_BYTES ||
        read_le32(bytes + 52U) != 1U || read_le32(bytes + 56U) != GPU_BATCH_FLAG_PRESENT ||
        memcmp(bytes + 60U, "\0\0\0\0", 4U) != 0) {
        errno = EPROTO;
        return -1;
    }
    header->command_count = read_le32(bytes + 20U);
    header->context_id = read_le32(bytes + 24U);
    header->context_epoch = read_le32(bytes + 28U);
    header->submit_value = read_le64(bytes + 32U);
    header->acquire_value = read_le64(bytes + 40U);
    header->budget_us = read_le32(bytes + 48U);
    required = GPU_HEADER_BYTES + GPU_SOURCE_BYTES +
               (size_t)header->command_count * GPU_COMMAND_BYTES;
    if (header->command_count == 0U || header->command_count > GPU_MAX_COMMANDS ||
        header->context_id == 0U || header->context_epoch == 0U ||
        header->submit_value == 0U || header->acquire_value == 0U ||
        header->acquire_value > header->submit_value || header->budget_us == 0U ||
        header->budget_us > GPU_MAX_BUDGET_US || required != length) {
        errno = EPROTO;
        return -1;
    }
    bytes += GPU_HEADER_BYTES;
    if (memcmp(bytes + 56U, "\0\0\0\0\0\0\0\0", 8U) != 0 ||
        read_le32(bytes + 36U) != GPU_PIXEL_FORMAT_BGRA8888 ||
        read_le32(bytes + 40U) != GPU_SOURCE_REQUIRED_FLAGS) {
        errno = EPROTO;
        return -1;
    }
    source->token = read_le64(bytes);
    source->generation = read_le64(bytes + 8U);
    source->acquire_value = read_le64(bytes + 16U);
    source->width = read_le32(bytes + 24U);
    source->height = read_le32(bytes + 28U);
    source->stride_bytes = read_le32(bytes + 32U);
    source->binding_slot = read_le32(bytes + 44U);
    source->content_epoch = read_le64(bytes + 48U);
    source_bytes = (uint64_t)source->stride_bytes * source->height;
    if (source->token == 0U || source->generation == 0U ||
        source->acquire_value == 0U || source->acquire_value > header->acquire_value ||
        source->width != runtime->atlas_width || source->height != runtime->atlas_height ||
        source->stride_bytes != runtime->atlas_stride_bytes ||
        source->binding_slot >= GPU_OUTPUT_COUNT || source->content_epoch == 0U ||
        source_bytes == 0U || source_bytes > GPU_MAX_SOURCE_BYTES) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int parse_command(const struct rustos_gpu_runtime *runtime, const uint8_t *bytes,
                         uint32_t index, struct gpu_command *command) {
    uint64_t x_end;
    uint64_t y_end;
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
    if (!fixed_bounded(command->depth) || !fixed_bounded(command->rotation) ||
        !fixed_bounded(command->tilt_x) || !fixed_bounded(command->tilt_y) ||
        !fixed_bounded(command->perspective) ||
        (command->blend_mode != GPU_BLEND_REPLACE &&
         command->blend_mode != GPU_BLEND_SOURCE_OVER)) {
        errno = EPROTO;
        return -1;
    }
    if (command->kind == GPU_COMMAND_CLEAR) {
        if (index != 0U || command->flags != 0U || command->source_index != GPU_NO_SOURCE ||
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
    if (index == 0U || (command->kind != GPU_COMMAND_SOLID_QUAD &&
                        command->kind != GPU_COMMAND_TEXTURED_QUAD) ||
        command->flags != GPU_COMMAND_FLAG_CLIP_OUTPUT || command->destination_x < 0 ||
        command->destination_y < 0 || command->destination_width == 0U ||
        command->destination_height == 0U) {
        errno = EPROTO;
        return -1;
    }
    x_end = (uint64_t)(uint32_t)command->destination_x + command->destination_width;
    y_end = (uint64_t)(uint32_t)command->destination_y + command->destination_height;
    if (x_end > runtime->output_width || y_end > runtime->output_height) {
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
    } else if (command->source_index != 0U || command->source_width == 0U ||
               command->source_height == 0U ||
               (uint32_t)command->source_u + command->source_width > UINT16_MAX ||
               (uint32_t)command->source_v + command->source_height > UINT16_MAX) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static void unpack_color(uint32_t rgba, GLfloat color[4]) {
    color[0] = (GLfloat)(rgba & 0xffU) / 255.0F;
    color[1] = (GLfloat)((rgba >> 8) & 0xffU) / 255.0F;
    color[2] = (GLfloat)((rgba >> 16) & 0xffU) / 255.0F;
    color[3] = (GLfloat)((rgba >> 24) & 0xffU) / 255.0F;
}

static int finish_frame(struct rustos_gpu_runtime *runtime, struct rustos_gpu_frame *frame) {
    int fence_fd;
    runtime->stage = "gpu-render-native-fence";
    fence_fd = native_fence(runtime);
    if (fence_fd < 0)
        return -1;
    runtime->stage = "gpu-render-egl-swap";
    if (!eglSwapBuffers(runtime->egl_display, runtime->egl_surface)) {
        close(fence_fd);
        errno = EIO;
        return -1;
    }
    runtime->stage = "gpu-render-lock-output";
    if (lock_output(runtime, frame) != 0) {
        close(fence_fd);
        return -1;
    }
    frame->in_fence_fd = fence_fd;
    runtime->stage = "gpu-render-ready";
    return 0;
}

static int reject_source_acquire_fence(int source_acquire_fence_fd, int error) {
    if (source_acquire_fence_fd >= 0)
        close(source_acquire_fence_fd);
    if (error != 0)
        errno = error;
    else if (errno == 0)
        errno = EPROTO;
    return -1;
}

static int wait_external_source_acquire(struct rustos_gpu_runtime *runtime,
                                        int source_acquire_fence_fd) {
    const EGLint attributes[] = {
        EGL_SYNC_NATIVE_FENCE_FD_ANDROID, source_acquire_fence_fd,
        EGL_NONE,
    };
    EGLSyncKHR sync;
    EGLint result;
    if (runtime == NULL || runtime->wait_sync == NULL ||
        source_acquire_fence_fd < 0) {
        return reject_source_acquire_fence(source_acquire_fence_fd, EINVAL);
    }
    runtime->stage = "gpu-batch-external-acquire-import";
    sync = runtime->create_sync(runtime->egl_display,
                                EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    if (sync == EGL_NO_SYNC_KHR) {
        close(source_acquire_fence_fd);
        errno = EIO;
        return -1;
    }
    /* A successful native-fence import transfers ownership of the fd to EGL.
     * eglWaitSyncKHR inserts the producer dependency into the current GPU
     * command stream without turning the relay into a CPU-side busy waiter. */
    runtime->stage = "gpu-batch-external-acquire-wait";
    result = runtime->wait_sync(runtime->egl_display, sync, 0);
    if (!runtime->destroy_sync(runtime->egl_display, sync) || result != EGL_TRUE) {
        errno = EIO;
        return -1;
    }
    return 0;
}

int rustos_gpu_runtime_open(int drm_fd, uint32_t output_width, uint32_t output_height,
                            uint32_t atlas_width, uint32_t atlas_height,
                            uint32_t atlas_stride_bytes,
                            struct rustos_gpu_runtime **runtime_out) {
    struct rustos_gpu_runtime *runtime;
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display;
    PFNEGLCREATEPLATFORMWINDOWSURFACEEXTPROC create_platform_window_surface;
    EGLConfig config;
    EGLint major = 0;
    EGLint minor = 0;
    EGLint render_buffer = EGL_NONE;
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    const char *egl_extensions;
    const char *gl_extensions;
    const GLubyte *renderer;
    uint64_t addfb2_modifiers = 0U;
    drmVersionPtr version;
    static const GLfloat vertices[] = {
        -1.0F, -1.0F, 0.0F, 0.0F,
         1.0F, -1.0F, 1.0F, 0.0F,
        -1.0F,  1.0F, 0.0F, 1.0F,
         1.0F,  1.0F, 1.0F, 1.0F,
    };
    if (runtime_out == NULL || drm_fd < 0 || output_width == 0U || output_height == 0U ||
        output_width > GPU_MAX_DIMENSION || output_height > GPU_MAX_DIMENSION ||
        atlas_width == 0U || atlas_height == 0U || atlas_width > GPU_MAX_DIMENSION ||
        atlas_height > GPU_MAX_DIMENSION || atlas_stride_bytes < atlas_width * 4U ||
        (uint64_t)atlas_stride_bytes * atlas_height > GPU_MAX_SOURCE_BYTES) {
        errno = EINVAL;
        return -1;
    }
    runtime = calloc(1U, sizeof(*runtime));
    if (runtime == NULL)
        return -1;
    runtime->drm_fd = drm_fd;
    runtime->egl_display = EGL_NO_DISPLAY;
    runtime->egl_context = EGL_NO_CONTEXT;
    runtime->egl_surface = EGL_NO_SURFACE;
    runtime->front_output = UINT32_MAX;
    runtime->output_width = output_width;
    runtime->output_height = output_height;
    runtime->atlas_width = atlas_width;
    runtime->atlas_height = atlas_height;
    runtime->atlas_stride_bytes = atlas_stride_bytes;
    runtime->stage = "gpu-open-driver";
    version = drmGetVersion(drm_fd);
    if (version == NULL || version->name == NULL || version->name_len == 0U ||
        (size_t)version->name_len >= sizeof(runtime->driver))
        goto fail;
    memcpy(runtime->driver, version->name, (size_t)version->name_len);
    runtime->driver[version->name_len] = '\0';
    drmFreeVersion(version);
    version = NULL;
    if (strcmp(runtime->driver, "virtio_gpu") != 0 && strcmp(runtime->driver, "amdgpu") != 0) {
        errno = EOPNOTSUPP;
        goto fail;
    }
    if (drmGetCap(drm_fd, DRM_CAP_ADDFB2_MODIFIERS, &addfb2_modifiers) != 0)
        goto fail;
    runtime->addfb2_modifiers = addfb2_modifiers != 0U;
    runtime->gbm = gbm_create_device(drm_fd);
    if (runtime->gbm == NULL)
        goto fail;
    runtime->gbm_surface = gbm_surface_create(runtime->gbm, output_width, output_height,
                                              GBM_FORMAT_XRGB8888,
                                              GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
    if (runtime->gbm_surface == NULL)
        goto fail;
    get_platform_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    create_platform_window_surface = (PFNEGLCREATEPLATFORMWINDOWSURFACEEXTPROC)
        eglGetProcAddress("eglCreatePlatformWindowSurfaceEXT");
    if (get_platform_display == NULL || create_platform_window_surface == NULL) {
        errno = ENOSYS;
        goto fail;
    }
    runtime->egl_display = get_platform_display(EGL_PLATFORM_GBM_KHR, runtime->gbm, NULL);
    if (runtime->egl_display == EGL_NO_DISPLAY ||
        !eglInitialize(runtime->egl_display, &major, &minor) ||
        !eglBindAPI(EGL_OPENGL_ES_API) || select_egl_config(runtime->egl_display, &config) != 0)
        goto fail;
    egl_extensions = eglQueryString(runtime->egl_display, EGL_EXTENSIONS);
    if (!extension_present(egl_extensions, "EGL_ANDROID_native_fence_sync")) {
        errno = EOPNOTSUPP;
        goto fail;
    }
    runtime->create_sync =
        (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    runtime->destroy_sync =
        (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    runtime->dup_native_fence = (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)
        eglGetProcAddress("eglDupNativeFenceFDANDROID");
    if (runtime->create_sync == NULL || runtime->destroy_sync == NULL ||
        runtime->dup_native_fence == NULL) {
        errno = ENOSYS;
        goto fail;
    }
    runtime->egl_context = eglCreateContext(runtime->egl_display, config, EGL_NO_CONTEXT,
                                             context_attributes);
    runtime->egl_surface = create_platform_window_surface(runtime->egl_display, config,
                                                           runtime->gbm_surface, NULL);
    if (runtime->egl_context == EGL_NO_CONTEXT || runtime->egl_surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(runtime->egl_display, runtime->egl_surface,
                        runtime->egl_surface, runtime->egl_context)) {
        errno = EIO;
        goto fail;
    }
    runtime->stage = "gpu-egl-presentation-contract";
    if (!eglQuerySurface(runtime->egl_display, runtime->egl_surface,
                         EGL_RENDER_BUFFER, &render_buffer) ||
        render_buffer != EGL_BACK_BUFFER || !eglSwapInterval(runtime->egl_display, 0)) {
        errno = EPROTO;
        goto fail;
    }
    renderer = glGetString(GL_RENDERER);
    if (renderer == NULL || strnlen((const char *)renderer, sizeof(runtime->renderer)) >=
                                sizeof(runtime->renderer)) {
        errno = EPROTO;
        goto fail;
    }
    strcpy(runtime->renderer, (const char *)renderer);
    if (contains_software_renderer(runtime->renderer)) {
        errno = EOPNOTSUPP;
        goto fail;
    }
    gl_extensions = (const char *)glGetString(GL_EXTENSIONS);
    if (!extension_present(gl_extensions, "GL_EXT_texture_format_BGRA8888")) {
        errno = EOPNOTSUPP;
        goto fail;
    }
    if (create_program(runtime) != 0)
        goto fail;
    glGenTextures(1, &runtime->source_texture);
    glBindTexture(GL_TEXTURE_2D, runtime->source_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, (GLsizei)atlas_width, (GLsizei)atlas_height,
                 0, GL_BGRA_EXT, GL_UNSIGNED_BYTE, NULL);
    glGenVertexArrays(1, &runtime->vertex_array);
    glBindVertexArray(runtime->vertex_array);
    glGenBuffers(1, &runtime->vertex_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, runtime->vertex_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(GLfloat), (void *)0);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(GLfloat),
                          (void *)(2 * sizeof(GLfloat)));
    glEnableVertexAttribArray(1);
    glViewport(0, 0, (GLsizei)output_width, (GLsizei)output_height);
    if (glGetError() != GL_NO_ERROR) {
        errno = EIO;
        goto fail;
    }
    runtime->stage = "gpu-runtime-ready";
    *runtime_out = runtime;
    return 0;
fail:
    if (version != NULL)
        drmFreeVersion(version);
    rustos_gpu_runtime_close(runtime);
    return -1;
}

int rustos_gpu_runtime_import_dmabuf_sources(struct rustos_gpu_runtime *runtime,
                                             const int *source_fds,
                                             size_t source_count) {
    const char *egl_extensions;
    const char *gl_extensions;
    size_t index;
    if (runtime == NULL || source_fds == NULL || source_count != GPU_OUTPUT_COUNT ||
        runtime->dmabuf_sources_ready || strcmp(runtime->driver, "amdgpu") != 0) {
        errno = EINVAL;
        return -1;
    }
    runtime->stage = "gpu-dmabuf-extension-contract";
    egl_extensions = eglQueryString(runtime->egl_display, EGL_EXTENSIONS);
    gl_extensions = (const char *)glGetString(GL_EXTENSIONS);
    if (!extension_present(egl_extensions, "EGL_KHR_image_base") ||
        !extension_present(egl_extensions, "EGL_EXT_image_dma_buf_import") ||
        !extension_present(egl_extensions, "EGL_KHR_wait_sync") ||
        !extension_present(gl_extensions, "GL_OES_EGL_image")) {
        errno = EOPNOTSUPP;
        return -1;
    }
    runtime->create_image =
        (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    runtime->destroy_image =
        (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    runtime->wait_sync =
        (PFNEGLWAITSYNCKHRPROC)eglGetProcAddress("eglWaitSyncKHR");
    runtime->image_target_texture = (rustos_gl_egl_image_target_texture_fn)
        eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (runtime->create_image == NULL || runtime->destroy_image == NULL ||
        runtime->wait_sync == NULL ||
        runtime->image_target_texture == NULL) {
        errno = ENOSYS;
        return -1;
    }
    for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
        const EGLint attributes[] = {
            EGL_WIDTH, (EGLint)runtime->atlas_width,
            EGL_HEIGHT, (EGLint)runtime->atlas_height,
            EGL_LINUX_DRM_FOURCC_EXT, (EGLint)DRM_FORMAT_ARGB8888,
            EGL_DMA_BUF_PLANE0_FD_EXT, source_fds[index],
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)runtime->atlas_stride_bytes,
            EGL_NONE,
        };
        if (source_fds[index] < 0) {
            errno = EBADF;
            goto fail;
        }
        runtime->stage = "gpu-dmabuf-egl-image";
        runtime->dmabuf_source_images[index] = runtime->create_image(
            runtime->egl_display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL,
            attributes);
        if (runtime->dmabuf_source_images[index] == EGL_NO_IMAGE_KHR) {
		EGLint egl_error = eglGetError();
		fprintf(stderr,
		        "rustos-dvm-display: DMA-BUF EGL import failed slot=%zu egl_error=0x%x\n",
		        index, egl_error);
            errno = EIO;
            goto fail;
        }
        glGenTextures(1, &runtime->dmabuf_source_textures[index]);
        glBindTexture(GL_TEXTURE_2D, runtime->dmabuf_source_textures[index]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        runtime->image_target_texture(
            GL_TEXTURE_2D, (void *)runtime->dmabuf_source_images[index]);
        if (runtime->dmabuf_source_textures[index] == 0U ||
            glGetError() != GL_NO_ERROR) {
            errno = EIO;
            goto fail;
        }
    }
    runtime->dmabuf_sources_ready = 1;
    runtime->stage = "gpu-dmabuf-ready";
    return 0;
fail:
    for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
        if (runtime->dmabuf_source_textures[index] != 0U) {
            glDeleteTextures(1, &runtime->dmabuf_source_textures[index]);
            runtime->dmabuf_source_textures[index] = 0U;
        }
        if (runtime->dmabuf_source_images[index] != EGL_NO_IMAGE_KHR) {
            (void)runtime->destroy_image(runtime->egl_display,
                                         runtime->dmabuf_source_images[index]);
            runtime->dmabuf_source_images[index] = EGL_NO_IMAGE_KHR;
        }
    }
    return -1;
}

int rustos_gpu_runtime_render_prime(struct rustos_gpu_runtime *runtime,
                                    struct rustos_gpu_frame *frame) {
    uint8_t *pixels;
    size_t atlas_bytes;
    if (runtime == NULL || frame == NULL) {
        errno = EINVAL;
        return -1;
    }
    atlas_bytes = (size_t)runtime->atlas_stride_bytes * runtime->atlas_height;
    if (runtime->atlas_height != 0U &&
        atlas_bytes / runtime->atlas_height != runtime->atlas_stride_bytes) {
        errno = EOVERFLOW;
        return -1;
    }
    pixels = calloc(1U, atlas_bytes);
    if (pixels == NULL)
        return -1;
    memset(frame, 0, sizeof(*frame));
    frame->in_fence_fd = -1;
    frame->budget_us = RUSTOS_GPU_PIPELINE_PRIME_BUDGET_US;
    runtime->stage = "gpu-prime-workload-clock";
    if (monotonic_ns(&frame->render_started_ns) != 0)
        goto fail;
    /* Prime must not sample RustOS-owned DMA-BUF pixels before the first
     * validated producer release. Exercise the GPU/KMS pipeline with the
     * private zero-filled texture; the first real frame proves DMA-BUF use. */
    runtime->stage = "gpu-prime-internal-source";
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, runtime->source_texture);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
    glPixelStorei(GL_UNPACK_ROW_LENGTH,
                  (GLint)(runtime->atlas_stride_bytes / 4U));
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, (GLsizei)runtime->atlas_width,
                    (GLsizei)runtime->atlas_height, GL_BGRA_EXT,
                    GL_UNSIGNED_BYTE, pixels);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
    free(pixels);
    pixels = NULL;
    runtime->stage = "gpu-prime-textured-draw";
    glUseProgram(runtime->program);
    glBindVertexArray(runtime->vertex_array);
    glUniform2f(runtime->output_size_uniform, (GLfloat)runtime->output_width,
                (GLfloat)runtime->output_height);
    glUniform4f(runtime->rect_uniform, 0.0F, 0.0F,
                (GLfloat)runtime->output_width, (GLfloat)runtime->output_height);
    glUniform4f(runtime->color_uniform, 1.0F, 1.0F, 1.0F, 1.0F);
    glUniform4f(runtime->transform_uniform, 0.0F, 0.0F, 0.0F, 0.0F);
    glUniform1f(runtime->perspective_uniform, 0.0F);
    glUniform4f(runtime->uv_rect_uniform, 0.0F, 0.0F,
                (GLfloat)runtime->output_width / (GLfloat)runtime->atlas_width,
                (GLfloat)runtime->output_height / (GLfloat)runtime->atlas_height);
    glUniform1i(runtime->use_texture_uniform, 1);
    glDisable(GL_BLEND);
    glClearColor(0.0F, 0.0F, 0.0F, 1.0F);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    if (glGetError() != GL_NO_ERROR) {
        errno = EIO;
        goto fail;
    }
    return finish_frame(runtime, frame);
fail:
    free(pixels);
    return -1;
}

int rustos_gpu_runtime_render_batch(struct rustos_gpu_runtime *runtime,
                                    const uint8_t *atlas_pixels, size_t atlas_bytes,
                                    const struct rustos_gpu_damage *damage,
                                    uint32_t damage_count,
                                    const uint8_t *batch, size_t batch_bytes,
                                    uint32_t binding_slot, uint64_t generation,
                                    uint64_t sequence, int source_acquire_fence_fd,
                                    struct rustos_gpu_frame *frame) {
    struct gpu_batch_header header;
    struct gpu_source source;
    uint32_t index;
    int source_referenced = 0;
    GLsync acquire_fence;
    GLenum wait_result;
    if (runtime != NULL)
        runtime->stage = "gpu-batch-validate";
    if (runtime == NULL)
        return reject_source_acquire_fence(source_acquire_fence_fd, EINVAL);
    if ((runtime->dmabuf_sources_ready && source_acquire_fence_fd < 0) ||
        (!runtime->dmabuf_sources_ready && source_acquire_fence_fd >= 0) ||
        atlas_pixels == NULL ||
        (damage_count != 0U && damage == NULL) || damage_count > 64U ||
        batch == NULL || frame == NULL ||
        generation == 0U || sequence == 0U ||
        atlas_bytes != (size_t)runtime->atlas_stride_bytes * runtime->atlas_height ||
        parse_batch(runtime, batch, batch_bytes, &header, &source) != 0 ||
        source.binding_slot != binding_slot || source.generation != generation ||
        generation <= runtime->last_generation ||
        sequence <= runtime->last_sequence ||
        header.submit_value != runtime->expected_submit + 1U ||
        header.acquire_value <= runtime->completed_acquire ||
        source.content_epoch <= runtime->last_content_epoch ||
        (runtime->expected_submit != 0U &&
         (header.context_id != runtime->context_id ||
          header.context_epoch != runtime->context_epoch))) {
        return reject_source_acquire_fence(source_acquire_fence_fd,
                                           errno == 0 ? EPROTO : 0);
    }
    for (index = 0U; index < damage_count; index++) {
        uint64_t x_end = (uint64_t)damage[index].x + damage[index].width;
        uint64_t y_end = (uint64_t)damage[index].y + damage[index].height;
        uint32_t prior;
        if (damage[index].width == 0U || damage[index].height == 0U ||
            x_end > runtime->atlas_width || y_end > runtime->atlas_height) {
            return reject_source_acquire_fence(source_acquire_fence_fd, EPROTO);
        }
        for (prior = 0U; prior < index; prior++) {
            if ((uint64_t)damage[prior].x < (uint64_t)damage[index].x + damage[index].width &&
                (uint64_t)damage[index].x < (uint64_t)damage[prior].x + damage[prior].width &&
                (uint64_t)damage[prior].y < (uint64_t)damage[index].y + damage[index].height &&
                (uint64_t)damage[index].y < (uint64_t)damage[prior].y + damage[prior].height) {
                return reject_source_acquire_fence(source_acquire_fence_fd, EPROTO);
            }
        }
    }
    if (runtime->expected_submit == 0U &&
        (damage_count != 1U || damage[0].x != 0U || damage[0].y != 0U ||
         damage[0].width != runtime->atlas_width ||
         damage[0].height != runtime->atlas_height)) {
        return reject_source_acquire_fence(source_acquire_fence_fd, EPROTO);
    }
    memset(frame, 0, sizeof(*frame));
    if (runtime->expected_submit == 0U) {
        runtime->context_id = header.context_id;
        runtime->context_epoch = header.context_epoch;
    }
    frame->in_fence_fd = -1;
    frame->context_id = header.context_id;
    frame->context_epoch = header.context_epoch;
    frame->submit_value = header.submit_value;
    frame->generation = generation;
    frame->sequence = sequence;
    frame->source_slot = binding_slot;
    frame->budget_us = header.budget_us;
    runtime->stage = runtime->dmabuf_sources_ready
        ? "gpu-batch-dmabuf-acquire"
        : "gpu-batch-atlas-upload";
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, runtime->dmabuf_sources_ready
        ? runtime->dmabuf_source_textures[binding_slot]
        : runtime->source_texture);
    if (runtime->dmabuf_sources_ready) {
        if (wait_external_source_acquire(runtime, source_acquire_fence_fd) != 0)
            return -1;
        source_acquire_fence_fd = -1;
    } else {
        glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
        glPixelStorei(GL_UNPACK_ROW_LENGTH,
                      (GLint)(runtime->atlas_stride_bytes / 4U));
        for (index = 0U; index < damage_count; index++) {
            const uint8_t *pixels = atlas_pixels +
                (size_t)damage[index].y * runtime->atlas_stride_bytes +
                (size_t)damage[index].x * 4U;
            glTexSubImage2D(GL_TEXTURE_2D, 0, (GLint)damage[index].x,
                            (GLint)damage[index].y, (GLsizei)damage[index].width,
                            (GLsizei)damage[index].height, GL_BGRA_EXT,
                            GL_UNSIGNED_BYTE, pixels);
        }
        glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
    }
    if (!runtime->dmabuf_sources_ready) {
        runtime->stage = "gpu-batch-upload-fence";
        acquire_fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0U);
        if (acquire_fence == NULL) {
            errno = EIO;
            return -1;
        }
        glFlush();
        wait_result = glClientWaitSync(acquire_fence, GL_SYNC_FLUSH_COMMANDS_BIT,
                                       (GLuint64)header.budget_us * 1000ULL);
        glDeleteSync(acquire_fence);
        if (wait_result != GL_ALREADY_SIGNALED &&
            wait_result != GL_CONDITION_SATISFIED) {
            errno = wait_result == GL_TIMEOUT_EXPIRED ? ETIMEDOUT : EIO;
            return -1;
        }
    }
    runtime->completed_acquire = header.acquire_value;
    runtime->last_content_epoch = source.content_epoch;
    runtime->stage = "gpu-batch-render-clock";
    if (monotonic_ns(&frame->render_started_ns) != 0)
        return -1;
    runtime->stage = "gpu-batch-command-draw";
    glUseProgram(runtime->program);
    glBindVertexArray(runtime->vertex_array);
    glUniform2f(runtime->output_size_uniform, (GLfloat)runtime->output_width,
                (GLfloat)runtime->output_height);
    glUniform1i(glGetUniformLocation(runtime->program, "u_source"), 0);
    for (index = 0U; index < header.command_count; index++) {
        const uint8_t *encoded = batch + GPU_HEADER_BYTES + GPU_SOURCE_BYTES +
                                 (size_t)index * GPU_COMMAND_BYTES;
        struct gpu_command command;
        GLfloat color[4];
        if (parse_command(runtime, encoded, index, &command) != 0)
            return -1;
        if (command.kind == GPU_COMMAND_TEXTURED_QUAD)
            source_referenced = 1;
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
            glBlendFunc(command.kind == GPU_COMMAND_TEXTURED_QUAD ? GL_ONE : GL_SRC_ALPHA,
                        GL_ONE_MINUS_SRC_ALPHA);
        } else {
            glDisable(GL_BLEND);
        }
        glUniform4f(runtime->rect_uniform, (GLfloat)command.destination_x,
                    (GLfloat)command.destination_y, (GLfloat)command.destination_width,
                    (GLfloat)command.destination_height);
        glUniform4f(runtime->color_uniform, color[0], color[1], color[2], color[3]);
        glUniform4f(runtime->transform_uniform, (GLfloat)command.depth / 65536.0F,
                    (GLfloat)command.rotation / 65536.0F,
                    (GLfloat)command.tilt_x / 65536.0F,
                    (GLfloat)command.tilt_y / 65536.0F);
        glUniform1f(runtime->perspective_uniform,
                    (GLfloat)command.perspective / 65536.0F);
        glUniform4f(runtime->uv_rect_uniform,
                    (GLfloat)command.source_u / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_v / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_width / (GLfloat)UINT16_MAX,
                    (GLfloat)command.source_height / (GLfloat)UINT16_MAX);
        glUniform1i(runtime->use_texture_uniform,
                    command.kind == GPU_COMMAND_TEXTURED_QUAD ? 1 : 0);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    if (!source_referenced || glGetError() != GL_NO_ERROR) {
        errno = EPROTO;
        return -1;
    }
    if (finish_frame(runtime, frame) != 0)
        return -1;
    runtime->expected_submit = header.submit_value;
    runtime->last_generation = generation;
    runtime->last_sequence = sequence;
    return 0;
}

void rustos_gpu_runtime_presented(struct rustos_gpu_runtime *runtime,
                                  uint32_t output_index) {
    uint32_t previous;
    if (runtime == NULL || output_index >= GPU_OUTPUT_COUNT ||
        runtime->outputs[output_index].bo == NULL)
        return;
    previous = runtime->front_output;
    runtime->front_output = output_index;
    if (previous != UINT32_MAX && previous != output_index &&
        runtime->outputs[previous].bo != NULL) {
        if (runtime->outputs[previous].framebuffer_id != 0U)
            (void)drmModeRmFB(runtime->drm_fd,
                              runtime->outputs[previous].framebuffer_id);
        gbm_surface_release_buffer(runtime->gbm_surface, runtime->outputs[previous].bo);
        runtime->outputs[previous].bo = NULL;
        runtime->outputs[previous].framebuffer_id = 0U;
    }
}

void rustos_gpu_runtime_close(struct rustos_gpu_runtime *runtime) {
    uint32_t index;
    if (runtime == NULL)
        return;
    if (runtime->egl_display != EGL_NO_DISPLAY && runtime->egl_context != EGL_NO_CONTEXT &&
        runtime->egl_surface != EGL_NO_SURFACE)
        (void)eglMakeCurrent(runtime->egl_display, runtime->egl_surface,
                             runtime->egl_surface, runtime->egl_context);
    if (runtime->vertex_buffer != 0U)
        glDeleteBuffers(1, &runtime->vertex_buffer);
    if (runtime->vertex_array != 0U)
        glDeleteVertexArrays(1, &runtime->vertex_array);
    if (runtime->source_texture != 0U)
        glDeleteTextures(1, &runtime->source_texture);
    for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
        if (runtime->dmabuf_source_textures[index] != 0U)
            glDeleteTextures(1, &runtime->dmabuf_source_textures[index]);
        if (runtime->dmabuf_source_images[index] != EGL_NO_IMAGE_KHR &&
            runtime->destroy_image != NULL)
            (void)runtime->destroy_image(runtime->egl_display,
                                         runtime->dmabuf_source_images[index]);
    }
    if (runtime->program != 0U)
        glDeleteProgram(runtime->program);
    for (index = 0U; index < GPU_OUTPUT_COUNT; index++) {
        if (runtime->outputs[index].framebuffer_id != 0U)
            (void)drmModeRmFB(runtime->drm_fd, runtime->outputs[index].framebuffer_id);
        if (runtime->outputs[index].bo != NULL && runtime->gbm_surface != NULL)
            gbm_surface_release_buffer(runtime->gbm_surface, runtime->outputs[index].bo);
    }
    if (runtime->egl_display != EGL_NO_DISPLAY)
        (void)eglMakeCurrent(runtime->egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE,
                             EGL_NO_CONTEXT);
    if (runtime->egl_display != EGL_NO_DISPLAY && runtime->egl_surface != EGL_NO_SURFACE)
        (void)eglDestroySurface(runtime->egl_display, runtime->egl_surface);
    if (runtime->egl_display != EGL_NO_DISPLAY && runtime->egl_context != EGL_NO_CONTEXT)
        (void)eglDestroyContext(runtime->egl_display, runtime->egl_context);
    if (runtime->egl_display != EGL_NO_DISPLAY)
        (void)eglTerminate(runtime->egl_display);
    if (runtime->gbm_surface != NULL)
        gbm_surface_destroy(runtime->gbm_surface);
    if (runtime->gbm != NULL)
        gbm_device_destroy(runtime->gbm);
    free(runtime);
}

const char *rustos_gpu_runtime_driver(const struct rustos_gpu_runtime *runtime) {
    return runtime == NULL ? "unavailable" : runtime->driver;
}

const char *rustos_gpu_runtime_renderer(const struct rustos_gpu_runtime *runtime) {
    return runtime == NULL ? "unavailable" : runtime->renderer;
}

const char *rustos_gpu_runtime_stage(const struct rustos_gpu_runtime *runtime) {
    return runtime == NULL || runtime->stage == NULL ? "gpu-runtime-unknown" : runtime->stage;
}

int rustos_gpu_runtime_uses_dmabuf_sources(const struct rustos_gpu_runtime *runtime) {
    return runtime != NULL && runtime->dmabuf_sources_ready;
}
