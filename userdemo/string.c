#include "runtime.h"

void *memcpy(void *dest, const void *src, size_t len) {
    unsigned char *dest_bytes = (unsigned char *)dest;
    const unsigned char *src_bytes = (const unsigned char *)src;
    size_t index;

    for (index = 0; index < len; index += 1) {
        dest_bytes[index] = src_bytes[index];
    }

    return dest;
}

void *memmove(void *dest, const void *src, size_t len) {
    unsigned char *dest_bytes = (unsigned char *)dest;
    const unsigned char *src_bytes = (const unsigned char *)src;
    size_t index;

    if (dest_bytes == src_bytes || len == 0) {
        return dest;
    }

    if (dest_bytes < src_bytes) {
        for (index = 0; index < len; index += 1) {
            dest_bytes[index] = src_bytes[index];
        }
    } else {
        for (index = len; index > 0; index -= 1) {
            dest_bytes[index - 1] = src_bytes[index - 1];
        }
    }

    return dest;
}

void *memset(void *dest, int value, size_t len) {
    unsigned char *dest_bytes = (unsigned char *)dest;
    size_t index;

    for (index = 0; index < len; index += 1) {
        dest_bytes[index] = (unsigned char)value;
    }

    return dest;
}

int memcmp(const void *lhs, const void *rhs, size_t len) {
    const unsigned char *lhs_bytes = (const unsigned char *)lhs;
    const unsigned char *rhs_bytes = (const unsigned char *)rhs;
    size_t index;

    for (index = 0; index < len; index += 1) {
        if (lhs_bytes[index] != rhs_bytes[index]) {
            return (int)lhs_bytes[index] - (int)rhs_bytes[index];
        }
    }

    return 0;
}

size_t strlen(const char *text) {
    size_t len = 0;

    while (text[len] != '\0') {
        len += 1;
    }

    return len;
}
