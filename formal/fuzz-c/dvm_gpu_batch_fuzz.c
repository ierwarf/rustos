// Coverage-guided host harness over the exact Linux-DVM C parser implementation.
#define main rustos_dvm_gpu_probe_main
#include "../../driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-gpu-probe.c"
#undef main

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
    struct gpu_batch_header header;
    struct gpu_source source;
    struct gpu_command command;
    size_t required = 0U;
    size_t command_offset;
    uint32_t index;

    if (parse_batch_header(data, size, &header, &required) != 0 || required > size)
        return 0;
    if (header.source_count == 1U &&
        parse_source(data + GPU_HEADER_BYTES, &source) != 0)
        return 0;
    command_offset = GPU_HEADER_BYTES + (size_t)header.source_count * GPU_SOURCE_BYTES;
    for (index = 0U; index < header.command_count; index++) {
        if (parse_command(data + command_offset + (size_t)index * GPU_COMMAND_BYTES,
                          header.source_count, &command) != 0)
            break;
    }
    return 0;
}
