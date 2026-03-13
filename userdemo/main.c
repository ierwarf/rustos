#include <stddef.h>
#include <stdint.h>

#include "runtime.h"

enum {
    INPUT_BUFFER_LEN = 256,
    TOKEN_BUFFER_LEN = 256,
};

static const char BANNER[] = "console ready\r\n";
static const char NEWLINE[] = "\n";
static const char FAULT_TOKEN[] = "fault";
static const char BADWRITE_TOKEN[] = "badwrite";
static const char BADREAD_TOKEN[] = "badread";

static void write_bytes(const void *buffer, size_t len) {
    if (len == 0) {
        return;
    }

    (void)write(1, buffer, len);
}

static void write_line(const char *text, size_t len) {
    write_bytes(text, len);
    write_bytes(NEWLINE, sizeof(NEWLINE) - 1);
}

static int is_space(unsigned char byte) {
    switch (byte) {
    case ' ':
    case '\t':
    case '\n':
    case '\v':
    case '\f':
    case '\r':
        return 1;
    default:
        return 0;
    }
}

static int token_equals(const char *token, size_t token_len, const char *literal) {
    return token_len == strlen(literal) && memcmp(token, literal, token_len) == 0;
}

static void trigger_fault_sequence(const char *token, size_t token_len) {
    volatile unsigned char *bad_ptr = (volatile unsigned char *)(uintptr_t)USERDEMO_BAD_USER_PTR;

    if (token_equals(token, token_len, FAULT_TOKEN)) {
        (void)*bad_ptr;
        return;
    }

    if (token_equals(token, token_len, BADWRITE_TOKEN)) {
        (void)write(1, (const void *)(uintptr_t)USERDEMO_BAD_USER_PTR, 4);
        return;
    }

    if (token_equals(token, token_len, BADREAD_TOKEN)) {
        (void)read(0, (void *)(uintptr_t)USERDEMO_BAD_USER_PTR, 32);
    }
}

static void process_input_chunk(const char *input, size_t input_len) {
    char token[TOKEN_BUFFER_LEN];
    size_t cursor = 0;

    while (cursor < input_len) {
        size_t token_len = 0;

        while (cursor < input_len && is_space((unsigned char)input[cursor])) {
            cursor += 1;
        }
        if (cursor >= input_len) {
            return;
        }

        while (cursor < input_len && !is_space((unsigned char)input[cursor])) {
            if (token_len + 1 < sizeof(token)) {
                token[token_len] = input[cursor];
                token_len += 1;
            }
            cursor += 1;
        }

        token[token_len] = '\0';

        if (token_len == 0) {
            continue;
        }

        if (token_equals(token, token_len, FAULT_TOKEN) ||
            token_equals(token, token_len, BADWRITE_TOKEN) ||
            token_equals(token, token_len, BADREAD_TOKEN)) {
            trigger_fault_sequence(token, token_len);
            continue;
        }

        write_line(token, token_len);
    }
}

int main(int argc, char **argv) {
    char input[INPUT_BUFFER_LEN];

    (void)argc;
    (void)argv;

    write_bytes(BANNER, sizeof(BANNER) - 1);

    for (;;) {
        long read_len = read(0, input, sizeof(input));

        if (read_len <= 0) {
            sleep_ms(1);
            continue;
        }

        process_input_chunk(input, (size_t)read_len);
    }
}
