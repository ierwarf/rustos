#include "../common/ntdll_exports.h"
#include "../common/ntdll_syscall.h"
#include "../common/windows_runtime.h"

#define RUSTOS_NTDLL_EOF (-1)
#define RUSTOS_NTDLL_TLS_SLOT_COUNT 64u
#define RUSTOS_NTDLL_TLS_OUT_OF_INDEXES 0xffffffffu
#define RUSTOS_NTDLL_ONEXIT_CAPACITY 64u
#define RUSTOS_NTDLL_SCANF_BUFFER_CAPACITY 512u
#define RUSTOS_NTDLL_HEAP_BLOCK_CAPACITY 256u
#define RUSTOS_NTDLL_MEM_COMMIT 0x1000u
#define RUSTOS_NTDLL_MEM_RESERVE 0x2000u
#define RUSTOS_NTDLL_MEM_RELEASE 0x8000u
#define RUSTOS_NTDLL_PAGE_READWRITE 0x0004u
#define RUSTOS_NTDLL_HEAP_ZERO_MEMORY 0x00000008u
#define RUSTOS_NTDLL_ERROR_NOT_ENOUGH_MEMORY 8u

typedef struct RustosCriticalSectionLite {
    ULONGLONG debug_info;
    LONG lock_count;
    LONG recursion_count;
    ULONGLONG owning_thread;
    ULONGLONG lock_semaphore;
    ULONGLONG spin_count;
} RustosCriticalSectionLite;

typedef enum RustosFormatLength {
    RUSTOS_FORMAT_LENGTH_DEFAULT,
    RUSTOS_FORMAT_LENGTH_CHAR,
    RUSTOS_FORMAT_LENGTH_SHORT,
    RUSTOS_FORMAT_LENGTH_LONG,
    RUSTOS_FORMAT_LENGTH_LONGLONG,
    RUSTOS_FORMAT_LENGTH_SIZE,
} RustosFormatLength;

typedef void (*RustosOnExitCallback)(void);

typedef struct RustosHeapBlock {
    void *base;
    SIZE_T size;
} RustosHeapBlock;

static ULONGLONG rustos_tls_bitmap = 0;
static void *rustos_tls_values[RUSTOS_NTDLL_TLS_SLOT_COUNT];
static void *rustos_unhandled_exception_filter = NULL;
static int rustos_ungetc_byte = RUSTOS_NTDLL_EOF;
static char rustos_scanf_input[RUSTOS_NTDLL_SCANF_BUFFER_CAPACITY];
static size_t rustos_scanf_input_len = 0;
static size_t rustos_scanf_input_index = 0;
static RustosOnExitCallback rustos_onexit_callbacks[RUSTOS_NTDLL_ONEXIT_CAPACITY];
static UINT rustos_onexit_count = 0;
static RustosHeapBlock rustos_heap_blocks[RUSTOS_NTDLL_HEAP_BLOCK_CAPACITY];

static void *rustos_file_handle_from_stream(void *stream)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL || stream == NULL) {
        return NULL;
    }
    if ((ULONGLONG)stream == runtime->stdin_file_ptr) {
        return (void *)RUSTOS_HANDLE_STDIN;
    }
    if ((ULONGLONG)stream == runtime->stdout_file_ptr) {
        return (void *)RUSTOS_HANDLE_STDOUT;
    }
    if ((ULONGLONG)stream == runtime->stderr_file_ptr) {
        return (void *)RUSTOS_HANDLE_STDERR;
    }
    return NULL;
}

static int rustos_stream_is_stdin(void *stream)
{
    return rustos_file_handle_from_stream(stream) == (void *)RUSTOS_HANDLE_STDIN;
}

static int rustos_stream_is_output(void *stream)
{
    void *handle = rustos_file_handle_from_stream(stream);
    return handle == (void *)RUSTOS_HANDLE_STDOUT || handle == (void *)RUSTOS_HANDLE_STDERR;
}

static int rustos_write_all(void *stream, const char *buffer, size_t len)
{
    void *handle = rustos_file_handle_from_stream(stream);
    size_t written_total = 0;
    if (buffer == NULL || handle == NULL || handle == (void *)RUSTOS_HANDLE_STDIN) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return -1;
    }
    while (written_total < len) {
        DWORD chunk = (DWORD)(len - written_total);
        DWORD written = 0;
        if (!NtWriteFile(
                handle,
                buffer + written_total,
                chunk,
                &written,
                NULL)
            || written == 0) {
            return -1;
        }
        written_total += (size_t)written;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (int)written_total;
}

static int rustos_write_literal(void *stream, const char *text)
{
    size_t len = 0;
    while (text[len] != '\0') {
        len++;
    }
    return rustos_write_all(stream, text, len);
}

static int rustos_ascii_isspace(int ch)
{
    return ch == ' ' || (ch >= '\t' && ch <= '\r');
}

static int rustos_ascii_digit_value(int ch)
{
    if (ch >= '0' && ch <= '9') {
        return ch - '0';
    }
    if (ch >= 'a' && ch <= 'z') {
        return ch - 'a' + 10;
    }
    if (ch >= 'A' && ch <= 'Z') {
        return ch - 'A' + 10;
    }
    return -1;
}

static int rustos_heap_handle_valid(void *heap)
{
    RustosPebLite *peb = rustos_current_peb();
    if (heap == (void *)RUSTOS_HANDLE_PROCESS_HEAP) {
        return TRUE;
    }
    return peb != NULL && heap == (void *)peb->process_heap;
}

static RustosHeapBlock *rustos_find_heap_block(void *base)
{
    UINT index;
    for (index = 0; index < RUSTOS_NTDLL_HEAP_BLOCK_CAPACITY; index++) {
        if (rustos_heap_blocks[index].base == base) {
            return &rustos_heap_blocks[index];
        }
    }
    return NULL;
}

static RustosHeapBlock *rustos_reserve_heap_block_slot(void)
{
    UINT index;
    for (index = 0; index < RUSTOS_NTDLL_HEAP_BLOCK_CAPACITY; index++) {
        if (rustos_heap_blocks[index].base == NULL) {
            return &rustos_heap_blocks[index];
        }
    }
    return NULL;
}

static void rustos_copy_bytes(void *dst, const void *src, SIZE_T len)
{
    BYTE *out = (BYTE *)dst;
    const BYTE *in = (const BYTE *)src;
    SIZE_T index;
    for (index = 0; index < len; index++) {
        out[index] = in[index];
    }
}

static void *rustos_allocate_heap_block(SIZE_T size)
{
    RustosHeapBlock *slot;
    void *base;
    if (size == 0) {
        size = 1;
    }
    base = NtAllocateVirtualMemory(
        NULL,
        size,
        RUSTOS_NTDLL_MEM_COMMIT | RUSTOS_NTDLL_MEM_RESERVE,
        RUSTOS_NTDLL_PAGE_READWRITE);
    if (base == NULL) {
        if (rustos_get_last_error() == RUSTOS_ERROR_SUCCESS) {
            rustos_set_last_error(RUSTOS_NTDLL_ERROR_NOT_ENOUGH_MEMORY);
        }
        return NULL;
    }
    slot = rustos_reserve_heap_block_slot();
    if (slot == NULL) {
        NtFreeVirtualMemory(base, 0, RUSTOS_NTDLL_MEM_RELEASE);
        rustos_set_last_error(RUSTOS_NTDLL_ERROR_NOT_ENOUGH_MEMORY);
        return NULL;
    }
    slot->base = base;
    slot->size = size;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return base;
}

static int rustos_read_console_byte(void)
{
    BYTE byte = 0;
    DWORD read = 0;
    if (rustos_ungetc_byte != RUSTOS_NTDLL_EOF) {
        int value = rustos_ungetc_byte;
        rustos_ungetc_byte = RUSTOS_NTDLL_EOF;
        return value;
    }
    if (!NtReadFile((void *)RUSTOS_HANDLE_STDIN, &byte, 1, &read, NULL) || read == 0) {
        return RUSTOS_NTDLL_EOF;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (int)byte;
}

static size_t rustos_read_console_line(char *buffer, size_t capacity)
{
    size_t len = 0;
    if (buffer == NULL || capacity == 0) {
        return 0;
    }
    while (len + 1 < capacity) {
        int ch = rustos_read_console_byte();
        if (ch == RUSTOS_NTDLL_EOF) {
            break;
        }
        buffer[len++] = (char)ch;
        if (ch == '\n') {
            break;
        }
    }
    buffer[len] = '\0';
    return len;
}

static int rustos_scanf_refill_input(void)
{
    rustos_scanf_input_len =
        rustos_read_console_line(rustos_scanf_input, sizeof(rustos_scanf_input));
    rustos_scanf_input_index = 0;
    return rustos_scanf_input_len != 0;
}

static int rustos_scanf_peek_input(void)
{
    while (rustos_scanf_input_index >= rustos_scanf_input_len) {
        if (!rustos_scanf_refill_input()) {
            return RUSTOS_NTDLL_EOF;
        }
    }
    return (unsigned char)rustos_scanf_input[rustos_scanf_input_index];
}

static int rustos_scanf_take_input(void)
{
    int ch = rustos_scanf_peek_input();
    if (ch != RUSTOS_NTDLL_EOF) {
        rustos_scanf_input_index++;
    }
    return ch;
}

static void rustos_scanf_skip_input_whitespace(void)
{
    for (;;) {
        int ch = rustos_scanf_peek_input();
        if (ch == RUSTOS_NTDLL_EOF || !rustos_ascii_isspace(ch)) {
            return;
        }
        (void)rustos_scanf_take_input();
    }
}

typedef enum RustosScanfIntegerKind {
    RUSTOS_SCANF_INTEGER_SIGNED_DECIMAL,
    RUSTOS_SCANF_INTEGER_AUTO,
    RUSTOS_SCANF_INTEGER_UNSIGNED_DECIMAL,
    RUSTOS_SCANF_INTEGER_HEX,
} RustosScanfIntegerKind;

static int rustos_scanf_integer_accepts(
    RustosScanfIntegerKind kind,
    const char *token,
    size_t len,
    int ch)
{
    int signed_kind = kind == RUSTOS_SCANF_INTEGER_SIGNED_DECIMAL
        || kind == RUSTOS_SCANF_INTEGER_AUTO;
    size_t begin = (signed_kind && len > 0 && (token[0] == '+' || token[0] == '-')) ? 1u : 0u;
    size_t digits = len - begin;
    int hex_mode;
    if (ch == '+' || ch == '-') {
        return len == 0 && signed_kind;
    }
    if ((kind == RUSTOS_SCANF_INTEGER_HEX || kind == RUSTOS_SCANF_INTEGER_AUTO)
        && digits == 1 && token[begin] == '0' && (ch == 'x' || ch == 'X')) {
        return TRUE;
    }
    hex_mode = kind == RUSTOS_SCANF_INTEGER_HEX
        || (kind == RUSTOS_SCANF_INTEGER_AUTO && digits >= 2 && token[begin] == '0'
            && (token[begin + 1] == 'x' || token[begin + 1] == 'X'));
    if (hex_mode) {
        return rustos_ascii_digit_value(ch) >= 0 && rustos_ascii_digit_value(ch) < 16;
    }
    return ch >= '0' && ch <= '9';
}

/* Reads only the characters one integer conversion can consume, so "12,34"
 * parses as "%d,%d" and "12abc" leaves "abc" in the input like C scanf. */
static size_t rustos_scanf_read_integer_token(
    char *dst,
    size_t capacity,
    size_t width,
    RustosScanfIntegerKind kind)
{
    size_t len = 0;
    if (dst == NULL || capacity == 0) {
        return 0;
    }
    while (len + 1 < capacity) {
        int ch = rustos_scanf_peek_input();
        if (ch == RUSTOS_NTDLL_EOF || rustos_ascii_isspace(ch)) {
            break;
        }
        if (width != 0 && len >= width) {
            break;
        }
        if (!rustos_scanf_integer_accepts(kind, dst, len, ch)) {
            break;
        }
        dst[len++] = (char)rustos_scanf_take_input();
    }
    dst[len] = '\0';
    return len;
}

static int rustos_write_char(void *stream, char ch)
{
    return rustos_write_all(stream, &ch, 1);
}

typedef struct RustosFormatSpec {
    int left;
    int plus;
    int space;
    int alt;
    int zero;
    int width;
    int precision; /* -1 when absent */
} RustosFormatSpec;

static int rustos_write_repeat(void *stream, char ch, size_t count)
{
    char block[16];
    size_t index;
    size_t total = 0;
    for (index = 0; index < sizeof(block); index++) {
        block[index] = ch;
    }
    while (count != 0) {
        size_t chunk = count < sizeof(block) ? count : sizeof(block);
        if (rustos_write_all(stream, block, chunk) < 0) {
            return -1;
        }
        total += chunk;
        count -= chunk;
    }
    return (int)total;
}

/* Emits width padding, prefix, precision zeros and body as one printf field. */
static int rustos_write_field(
    void *stream,
    const RustosFormatSpec *spec,
    const char *prefix,
    size_t prefix_len,
    size_t zeros,
    const char *body,
    size_t body_len,
    int zero_pad_ok)
{
    size_t total = prefix_len + zeros + body_len;
    size_t pad = spec->width > 0 && (size_t)spec->width > total ? (size_t)spec->width - total : 0;
    int written = 0;
    int part;
    if (!spec->left && spec->zero && zero_pad_ok) {
        zeros += pad;
        pad = 0;
    }
    if (!spec->left && pad != 0) {
        part = rustos_write_repeat(stream, ' ', pad);
        if (part < 0) {
            return -1;
        }
        written += part;
    }
    if (prefix_len != 0) {
        part = rustos_write_all(stream, prefix, prefix_len);
        if (part < 0) {
            return -1;
        }
        written += part;
    }
    if (zeros != 0) {
        part = rustos_write_repeat(stream, '0', zeros);
        if (part < 0) {
            return -1;
        }
        written += part;
    }
    if (body_len != 0) {
        part = rustos_write_all(stream, body, body_len);
        if (part < 0) {
            return -1;
        }
        written += part;
    }
    if (spec->left && pad != 0) {
        part = rustos_write_repeat(stream, ' ', pad);
        if (part < 0) {
            return -1;
        }
        written += part;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return written;
}

static int rustos_write_integer(
    void *stream,
    const RustosFormatSpec *spec,
    unsigned long long magnitude,
    int is_signed,
    int negative,
    unsigned base,
    int upper)
{
    char digits[24];
    char reversed[24];
    char prefix[2];
    size_t prefix_len = 0;
    size_t count = 0;
    size_t zeros = 0;
    size_t index;
    const char *alphabet = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    if (base < 2 || base > 16) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return -1;
    }
    if (is_signed) {
        if (negative) {
            prefix[prefix_len++] = '-';
        } else if (spec->plus) {
            prefix[prefix_len++] = '+';
        } else if (spec->space) {
            prefix[prefix_len++] = ' ';
        }
    } else if (spec->alt && base == 16 && magnitude != 0) {
        prefix[prefix_len++] = '0';
        prefix[prefix_len++] = upper ? 'X' : 'x';
    }
    if (magnitude == 0) {
        if (spec->precision != 0) {
            reversed[count++] = '0';
        }
    } else {
        while (magnitude != 0) {
            reversed[count++] = alphabet[magnitude % base];
            magnitude /= base;
        }
    }
    for (index = 0; index < count; index++) {
        digits[index] = reversed[count - 1 - index];
    }
    if (spec->precision >= 0 && (size_t)spec->precision > count) {
        zeros = (size_t)spec->precision - count;
    }
    return rustos_write_field(
        stream, spec, prefix, prefix_len, zeros, digits, count, spec->precision < 0);
}

static const char *rustos_parse_scanf_width(const char *cursor, size_t *width)
{
    size_t value = 0;
    while (*cursor >= '0' && *cursor <= '9') {
        value = value * 10u + (size_t)(*cursor - '0');
        cursor++;
    }
    *width = value;
    return cursor;
}

static const char *rustos_parse_length(const char *cursor, RustosFormatLength *length)
{
    *length = RUSTOS_FORMAT_LENGTH_DEFAULT;
    if (cursor[0] == 'l' && cursor[1] == 'l') {
        *length = RUSTOS_FORMAT_LENGTH_LONGLONG;
        return cursor + 2;
    }
    if (cursor[0] == 'l') {
        *length = RUSTOS_FORMAT_LENGTH_LONG;
        return cursor + 1;
    }
    if (cursor[0] == 'z' || cursor[0] == 'j' || cursor[0] == 't') {
        *length = RUSTOS_FORMAT_LENGTH_SIZE;
        return cursor + 1;
    }
    if (cursor[0] == 'I' && cursor[1] == '6' && cursor[2] == '4') {
        *length = RUSTOS_FORMAT_LENGTH_LONGLONG;
        return cursor + 3;
    }
    if (cursor[0] == 'I' && cursor[1] == '3' && cursor[2] == '2') {
        *length = RUSTOS_FORMAT_LENGTH_LONG;
        return cursor + 3;
    }
    if (cursor[0] == 'h') {
        *length = cursor[1] == 'h' ? RUSTOS_FORMAT_LENGTH_CHAR : RUSTOS_FORMAT_LENGTH_SHORT;
        return cursor + (cursor[1] == 'h' ? 2 : 1);
    }
    return cursor;
}

static int rustos_vfprintf_internal(void *stream, const char *format, RUSTOS_VA_LIST ap)
{
    int written_total = 0;
    const char *cursor = format;
    const char *literal_start = format;
    if (!rustos_stream_is_output(stream) || format == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return -1;
    }

    while (*cursor != '\0') {
        RustosFormatLength length = RUSTOS_FORMAT_LENGTH_DEFAULT;
        RustosFormatSpec spec;
        const char *spec_cursor;
        char conv;
        int written;

        if (*cursor != '%') {
            cursor++;
            continue;
        }

        if (cursor > literal_start) {
            written = rustos_write_all(
                stream,
                literal_start,
                (size_t)(cursor - literal_start));
            if (written < 0) {
                return -1;
            }
            written_total += written;
        }

        cursor++;
        if (*cursor == '%') {
            if (rustos_write_char(stream, '%') < 0) {
                return -1;
            }
            written_total++;
            cursor++;
            literal_start = cursor;
            continue;
        }

        spec.left = 0;
        spec.plus = 0;
        spec.space = 0;
        spec.alt = 0;
        spec.zero = 0;
        spec.width = 0;
        spec.precision = -1;
        for (;; cursor++) {
            if (*cursor == '-') {
                spec.left = 1;
            } else if (*cursor == '+') {
                spec.plus = 1;
            } else if (*cursor == ' ') {
                spec.space = 1;
            } else if (*cursor == '#') {
                spec.alt = 1;
            } else if (*cursor == '0') {
                spec.zero = 1;
            } else {
                break;
            }
        }
        if (*cursor == '*') {
            spec.width = __builtin_va_arg(ap, int);
            if (spec.width < 0) {
                spec.left = 1;
                spec.width = spec.width == (-2147483647 - 1) ? 0x7fffffff : -spec.width;
            }
            cursor++;
        } else {
            while (*cursor >= '0' && *cursor <= '9') {
                if (spec.width > (0x7fffffff - 9) / 10) {
                    rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                    return -1;
                }
                spec.width = spec.width * 10 + (*cursor - '0');
                cursor++;
            }
        }
        if (*cursor == '.') {
            cursor++;
            spec.precision = 0;
            if (*cursor == '*') {
                spec.precision = __builtin_va_arg(ap, int);
                cursor++;
            } else {
                while (*cursor >= '0' && *cursor <= '9') {
                    if (spec.precision > (0x7fffffff - 9) / 10) {
                        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                        return -1;
                    }
                    spec.precision = spec.precision * 10 + (*cursor - '0');
                    cursor++;
                }
            }
            if (spec.precision < 0) {
                spec.precision = -1;
            }
        }
        spec_cursor = rustos_parse_length(cursor, &length);
        conv = *spec_cursor;
        if (conv == '\0') {
            rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
            return -1;
        }

        switch (conv) {
        case 'd':
        case 'i': {
            long long value;
            if (length == RUSTOS_FORMAT_LENGTH_LONGLONG || length == RUSTOS_FORMAT_LENGTH_SIZE) {
                value = __builtin_va_arg(ap, long long);
            } else if (length == RUSTOS_FORMAT_LENGTH_LONG) {
                value = __builtin_va_arg(ap, long);
            } else if (length == RUSTOS_FORMAT_LENGTH_SHORT) {
                value = (short)__builtin_va_arg(ap, int);
            } else if (length == RUSTOS_FORMAT_LENGTH_CHAR) {
                value = (signed char)__builtin_va_arg(ap, int);
            } else {
                value = __builtin_va_arg(ap, int);
            }
            written = rustos_write_integer(
                stream,
                &spec,
                value < 0 ? 0ull - (unsigned long long)value : (unsigned long long)value,
                TRUE,
                value < 0,
                10u,
                FALSE);
            break;
        }
        case 'u':
        case 'x':
        case 'X': {
            unsigned long long value;
            if (length == RUSTOS_FORMAT_LENGTH_LONGLONG || length == RUSTOS_FORMAT_LENGTH_SIZE) {
                value = __builtin_va_arg(ap, unsigned long long);
            } else if (length == RUSTOS_FORMAT_LENGTH_LONG) {
                value = __builtin_va_arg(ap, unsigned long);
            } else if (length == RUSTOS_FORMAT_LENGTH_SHORT) {
                value = (unsigned short)__builtin_va_arg(ap, unsigned int);
            } else if (length == RUSTOS_FORMAT_LENGTH_CHAR) {
                value = (unsigned char)__builtin_va_arg(ap, unsigned int);
            } else {
                value = __builtin_va_arg(ap, unsigned int);
            }
            written = rustos_write_integer(
                stream,
                &spec,
                value,
                FALSE,
                FALSE,
                conv == 'u' ? 10u : 16u,
                conv == 'X');
            break;
        }
        case 'c': {
            char ch = (char)__builtin_va_arg(ap, int);
            written = rustos_write_field(stream, &spec, NULL, 0, 0, &ch, 1, FALSE);
            break;
        }
        case 's': {
            const char *text = __builtin_va_arg(ap, const char *);
            size_t text_len = 0;
            if (text == NULL) {
                text = "(null)";
            }
            while (text[text_len] != '\0'
                && (spec.precision < 0 || text_len < (size_t)spec.precision)) {
                text_len++;
            }
            written = rustos_write_field(stream, &spec, NULL, 0, 0, text, text_len, FALSE);
            break;
        }
        case 'p': {
            RustosFormatSpec pointer_spec = spec;
            unsigned long long value =
                (unsigned long long)(ULONGLONG)__builtin_va_arg(ap, void *);
            pointer_spec.alt = 1;
            pointer_spec.precision = -1;
            if (value == 0) {
                written = rustos_write_field(stream, &pointer_spec, "0x", 2, 0, "0", 1, FALSE);
            } else {
                written = rustos_write_integer(
                    stream, &pointer_spec, value, FALSE, FALSE, 16u, FALSE);
            }
            break;
        }
        default:
            rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
            return -1;
        }

        if (written < 0) {
            return -1;
        }
        written_total += written;
        cursor = spec_cursor + 1;
        literal_start = cursor;
    }

    if (cursor > literal_start) {
        int written = rustos_write_all(stream, literal_start, (size_t)(cursor - literal_start));
        if (written < 0) {
            return -1;
        }
        written_total += written;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return written_total;
}

static int rustos_parse_unsigned_token(
    const char *token,
    int base,
    unsigned long long *value)
{
    unsigned long long parsed = 0;
    size_t index = 0;
    if (token == NULL || value == NULL || base < 2 || base > 16) {
        return FALSE;
    }
    if (token[0] == '\0') {
        return FALSE;
    }
    if (base == 16
        && token[0] == '0'
        && (token[1] == 'x' || token[1] == 'X')) {
        index = 2;
    }
    if (token[index] == '\0') {
        return FALSE;
    }
    while (token[index] != '\0') {
        int digit = rustos_ascii_digit_value((unsigned char)token[index]);
        if (digit < 0 || digit >= base) {
            return FALSE;
        }
        parsed = parsed * (unsigned long long)base + (unsigned long long)digit;
        index++;
    }
    *value = parsed;
    return TRUE;
}

static int rustos_parse_signed_token(const char *token, long long *value, int auto_base)
{
    int negative = FALSE;
    int base = auto_base ? 0 : 10;
    unsigned long long parsed = 0;
    if (token == NULL || value == NULL || token[0] == '\0') {
        return FALSE;
    }
    if (*token == '+' || *token == '-') {
        negative = *token == '-';
        token++;
    }
    if (*token == '\0') {
        return FALSE;
    }
    if (base == 0) {
        if (token[0] == '0' && (token[1] == 'x' || token[1] == 'X')) {
            base = 16;
        } else if (token[0] == '0') {
            base = 8;
        } else {
            base = 10;
        }
    }
    if (!rustos_parse_unsigned_token(token, base, &parsed)) {
        return FALSE;
    }
    *value = negative ? -(long long)parsed : (long long)parsed;
    return TRUE;
}

static int rustos_vscanf_internal(const char *format, RUSTOS_VA_LIST ap)
{
    const char *fmt = format;
    int assigned = 0;
    if (format == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }

    while (*fmt != '\0') {
        RustosFormatLength length = RUSTOS_FORMAT_LENGTH_DEFAULT;
        const char *spec_cursor;
        size_t field_width = 0;
        char spec;
        if (*fmt == '%') {
            fmt++;
            if (*fmt == '\0') {
                rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                return RUSTOS_NTDLL_EOF;
            }
            if (*fmt == '%') {
                if (rustos_scanf_take_input() != '%') {
                    return assigned;
                }
                fmt++;
                continue;
            }
            if (*fmt == '*') {
                rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                return RUSTOS_NTDLL_EOF;
            }
            fmt = rustos_parse_scanf_width(fmt, &field_width);
            spec_cursor = rustos_parse_length(fmt, &length);
            spec = *spec_cursor;
            if (spec != 'c') {
                rustos_scanf_skip_input_whitespace();
            }
            switch (spec) {
            case 'd':
            case 'i': {
                char token[128];
                size_t width = field_width;
                long long value = 0;
                if (width == 0 || width >= sizeof(token)) {
                    width = sizeof(token) - 1;
                }
                if (rustos_scanf_read_integer_token(
                        token,
                        sizeof(token),
                        width,
                        spec == 'i' ? RUSTOS_SCANF_INTEGER_AUTO
                                    : RUSTOS_SCANF_INTEGER_SIGNED_DECIMAL) == 0
                    || !rustos_parse_signed_token(token, &value, spec == 'i')) {
                    return assigned;
                }
                if (length == RUSTOS_FORMAT_LENGTH_LONGLONG || length == RUSTOS_FORMAT_LENGTH_SIZE) {
                    *(__builtin_va_arg(ap, long long *)) = value;
                } else if (length == RUSTOS_FORMAT_LENGTH_LONG) {
                    *(__builtin_va_arg(ap, long *)) = (long)value;
                } else if (length == RUSTOS_FORMAT_LENGTH_SHORT) {
                    *(__builtin_va_arg(ap, short *)) = (short)value;
                } else if (length == RUSTOS_FORMAT_LENGTH_CHAR) {
                    *(__builtin_va_arg(ap, signed char *)) = (signed char)value;
                } else {
                    *(__builtin_va_arg(ap, int *)) = (int)value;
                }
                assigned++;
                break;
            }
            case 'u':
            case 'x': {
                char token[128];
                size_t width = field_width;
                unsigned long long value = 0;
                int base = spec == 'x' ? 16 : 10;
                if (width == 0 || width >= sizeof(token)) {
                    width = sizeof(token) - 1;
                }
                if (rustos_scanf_read_integer_token(
                        token,
                        sizeof(token),
                        width,
                        spec == 'x' ? RUSTOS_SCANF_INTEGER_HEX
                                    : RUSTOS_SCANF_INTEGER_UNSIGNED_DECIMAL) == 0
                    || !rustos_parse_unsigned_token(token, base, &value)) {
                    return assigned;
                }
                if (length == RUSTOS_FORMAT_LENGTH_LONGLONG || length == RUSTOS_FORMAT_LENGTH_SIZE) {
                    *(__builtin_va_arg(ap, unsigned long long *)) = value;
                } else if (length == RUSTOS_FORMAT_LENGTH_LONG) {
                    *(__builtin_va_arg(ap, unsigned long *)) = (unsigned long)value;
                } else if (length == RUSTOS_FORMAT_LENGTH_SHORT) {
                    *(__builtin_va_arg(ap, unsigned short *)) = (unsigned short)value;
                } else if (length == RUSTOS_FORMAT_LENGTH_CHAR) {
                    *(__builtin_va_arg(ap, unsigned char *)) = (unsigned char)value;
                } else {
                    *(__builtin_va_arg(ap, unsigned int *)) = (unsigned int)value;
                }
                assigned++;
                break;
            }
            case 'c': {
                size_t count = field_width == 0 ? 1 : field_width;
                size_t index;
                char *out;
                out = __builtin_va_arg(ap, char *);
                if (out == NULL) {
                    rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                    return RUSTOS_NTDLL_EOF;
                }
                for (index = 0; index < count; index++) {
                    int ch = rustos_scanf_take_input();
                    if (ch == RUSTOS_NTDLL_EOF) {
                        return assigned == 0 && index == 0 ? RUSTOS_NTDLL_EOF : assigned;
                    }
                    out[index] = (char)ch;
                }
                assigned++;
                break;
            }
            case 's': {
                char *out;
                size_t copied = 0;
                if (rustos_scanf_peek_input() == RUSTOS_NTDLL_EOF) {
                    return assigned;
                }
                out = __builtin_va_arg(ap, char *);
                if (out == NULL) {
                    rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                    return RUSTOS_NTDLL_EOF;
                }
                for (;;) {
                    int ch = rustos_scanf_peek_input();
                    if (ch == RUSTOS_NTDLL_EOF || rustos_ascii_isspace(ch)) {
                        break;
                    }
                    if (field_width != 0 && copied >= field_width) {
                        break;
                    }
                    out[copied++] = (char)rustos_scanf_take_input();
                }
                if (copied == 0) {
                    return assigned;
                }
                out[copied] = '\0';
                assigned++;
                break;
            }
            default:
                rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
                return RUSTOS_NTDLL_EOF;
            }
            fmt = spec_cursor + 1;
            continue;
        }
        if (rustos_ascii_isspace((unsigned char)*fmt)) {
            while (rustos_ascii_isspace((unsigned char)*fmt)) {
                fmt++;
            }
            rustos_scanf_skip_input_whitespace();
            continue;
        }
        if (rustos_scanf_take_input() != (unsigned char)*fmt) {
            return assigned;
        }
        fmt++;
    }

    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return assigned;
}

void RtlExitUserProcess(UINT status)
{
    while (rustos_onexit_count != 0) {
        RustosOnExitCallback callback = rustos_onexit_callbacks[--rustos_onexit_count];
        rustos_onexit_callbacks[rustos_onexit_count] = NULL;
        if (callback != NULL) {
            callback();
        }
    }
    ntdll_syscall1(NTDLL_API_RtlExitUserProcess, status);
    for (;;) {
    }
}

void *RtlAllocateHeap(void *heap, ULONG flags, SIZE_T size)
{
    void *base;
    (void)flags;
    if (!rustos_heap_handle_valid(heap)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return NULL;
    }
    base = rustos_allocate_heap_block(size);
    if (base == NULL) {
        return NULL;
    }
    if ((flags & RUSTOS_NTDLL_HEAP_ZERO_MEMORY) != 0) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    }
    return base;
}

BYTE RtlFreeHeap(void *heap, ULONG flags, void *base)
{
    RustosHeapBlock *block;
    (void)flags;
    if (!rustos_heap_handle_valid(heap)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return FALSE;
    }
    if (base == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return TRUE;
    }
    block = rustos_find_heap_block(base);
    if (block == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    if (!NtFreeVirtualMemory(base, 0, RUSTOS_NTDLL_MEM_RELEASE)) {
        return FALSE;
    }
    block->base = NULL;
    block->size = 0;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

void *RtlReAllocateHeap(void *heap, ULONG flags, void *base, SIZE_T size)
{
    RustosHeapBlock *block;
    void *new_base;
    SIZE_T copy_len;
    if (!rustos_heap_handle_valid(heap)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return NULL;
    }
    if (base == NULL) {
        return RtlAllocateHeap(heap, flags, size);
    }
    if (size == 0) {
        size = 1;
    }
    block = rustos_find_heap_block(base);
    if (block == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
    if (size <= block->size) {
        block->size = size;
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return base;
    }
    new_base = rustos_allocate_heap_block(size);
    if (new_base == NULL) {
        return NULL;
    }
    copy_len = block->size < size ? block->size : size;
    rustos_copy_bytes(new_base, base, copy_len);
    if (!NtFreeVirtualMemory(base, 0, RUSTOS_NTDLL_MEM_RELEASE)) {
        /* The old block stays valid and owned by the caller; drop the copy. */
        RustosHeapBlock *new_block = rustos_find_heap_block(new_base);
        if (new_block != NULL) {
            new_block->base = NULL;
            new_block->size = 0;
        }
        NtFreeVirtualMemory(new_base, 0, RUSTOS_NTDLL_MEM_RELEASE);
        rustos_set_last_error(RUSTOS_NTDLL_ERROR_NOT_ENOUGH_MEMORY);
        return NULL;
    }
    block->base = NULL;
    block->size = 0;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return new_base;
}

void RtlDeleteCriticalSection(void *critical_section)
{
    RustosCriticalSectionLite *state = (RustosCriticalSectionLite *)critical_section;
    if (state == NULL) {
        return;
    }
    state->debug_info = 0;
    state->lock_count = 0;
    state->recursion_count = 0;
    state->owning_thread = 0;
    state->lock_semaphore = 0;
    state->spin_count = 0;
}

void RtlEnterCriticalSection(void *critical_section)
{
    RustosCriticalSectionLite *state = (RustosCriticalSectionLite *)critical_section;
    ULONGLONG tid = (ULONGLONG)rustos_current_thread_id_value();
    if (state == NULL) {
        return;
    }
    for (;;) {
        if (state->owning_thread == 0) {
            state->owning_thread = tid;
            state->lock_count = 0;
            state->recursion_count = 1;
            return;
        }
        if (state->owning_thread == tid) {
            state->lock_count += 1;
            state->recursion_count += 1;
            return;
        }
        NtDelayExecution(0);
    }
}

void RtlInitializeCriticalSection(void *critical_section)
{
    RustosCriticalSectionLite *state = (RustosCriticalSectionLite *)critical_section;
    if (state == NULL) {
        return;
    }
    state->debug_info = 0;
    state->lock_count = -1;
    state->recursion_count = 0;
    state->owning_thread = 0;
    state->lock_semaphore = 0;
    state->spin_count = 0;
}

void RtlLeaveCriticalSection(void *critical_section)
{
    RustosCriticalSectionLite *state = (RustosCriticalSectionLite *)critical_section;
    ULONGLONG tid = (ULONGLONG)rustos_current_thread_id_value();
    if (state == NULL || state->owning_thread != tid || state->recursion_count <= 0) {
        return;
    }
    state->recursion_count -= 1;
    if (state->recursion_count == 0) {
        state->owning_thread = 0;
        state->lock_count = -1;
    } else {
        state->lock_count -= 1;
    }
}

void *RtlSetUnhandledExceptionFilter(void *filter)
{
    void *previous = rustos_unhandled_exception_filter;
    rustos_unhandled_exception_filter = filter;
    return previous;
}

DWORD RtlTlsAlloc(void)
{
    DWORD index;
    for (index = 0; index < RUSTOS_NTDLL_TLS_SLOT_COUNT; index++) {
        ULONGLONG bit = 1ull << index;
        if ((rustos_tls_bitmap & bit) == 0) {
            rustos_tls_bitmap |= bit;
            rustos_tls_values[index] = NULL;
            rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
            return index;
        }
    }
    rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
    return RUSTOS_NTDLL_TLS_OUT_OF_INDEXES;
}

BOOL RtlTlsFree(DWORD index)
{
    ULONGLONG bit;
    if (index >= RUSTOS_NTDLL_TLS_SLOT_COUNT) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    bit = 1ull << index;
    if ((rustos_tls_bitmap & bit) == 0) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_tls_bitmap &= ~bit;
    rustos_tls_values[index] = NULL;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

void *RtlTlsGetValue(DWORD index)
{
    DWORD saved_last_error = rustos_get_last_error();
    if (index >= RUSTOS_NTDLL_TLS_SLOT_COUNT || (rustos_tls_bitmap & (1ull << index)) == 0) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
    rustos_set_last_error(saved_last_error);
    return rustos_tls_values[index];
}

BOOL RtlTlsSetValue(DWORD index, void *value)
{
    if (index >= RUSTOS_NTDLL_TLS_SLOT_COUNT || (rustos_tls_bitmap & (1ull << index)) == 0) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_tls_values[index] = value;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

int RtlMsvcrtPuts(const char *text)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    int written;
    if (runtime == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }
    written = rustos_write_literal((void *)runtime->stdout_file_ptr, text != NULL ? text : "(null)");
    if (written < 0 || rustos_write_char((void *)runtime->stdout_file_ptr, '\n') < 0) {
        return RUSTOS_NTDLL_EOF;
    }
    return written + 1;
}

int RtlMsvcrtPutchar(int ch)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }
    if (rustos_write_char((void *)runtime->stdout_file_ptr, (char)ch) < 0) {
        return RUSTOS_NTDLL_EOF;
    }
    return ch & 0xff;
}

int RtlMsvcrtGetchar(void)
{
    return rustos_read_console_byte();
}

char *RtlMsvcrtFgets(char *buffer, UINT len, void *stream)
{
    UINT copied = 0;
    if (buffer == NULL || len == 0 || !rustos_stream_is_stdin(stream)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
    while (copied + 1 < len) {
        int ch = rustos_read_console_byte();
        if (ch == RUSTOS_NTDLL_EOF) {
            break;
        }
        buffer[copied++] = (char)ch;
        if (ch == '\n') {
            break;
        }
    }
    if (copied == 0) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return NULL;
    }
    buffer[copied] = '\0';
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return buffer;
}

int RtlMsvcrtVfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap)
{
    return rustos_vfprintf_internal(stream, format, ap);
}

int RtlMsvcrtFflush(void *stream)
{
    if (stream == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return 0;
    }
    if (!rustos_stream_is_output(stream)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return 0;
}

void *RtlMsvcrtOnexit(void *callback)
{
    if (callback == NULL || rustos_onexit_count >= RUSTOS_NTDLL_ONEXIT_CAPACITY) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
    rustos_onexit_callbacks[rustos_onexit_count++] = (RustosOnExitCallback)callback;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return callback;
}

int RtlMsvcrtFputc(int ch, void *stream)
{
    if (!rustos_stream_is_output(stream) || rustos_write_char(stream, (char)ch) < 0) {
        return RUSTOS_NTDLL_EOF;
    }
    return ch & 0xff;
}

size_t RtlMsvcrtFwrite(const void *buffer, size_t size, size_t count, void *stream)
{
    size_t total;
    if (!rustos_stream_is_output(stream)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    total = size * count;
    if (total == 0) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return count;
    }
    if (rustos_write_all(stream, (const char *)buffer, total) < 0) {
        return 0;
    }
    return count;
}

int RtlMsvcrtGetc(void *stream)
{
    if (!rustos_stream_is_stdin(stream)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }
    return rustos_read_console_byte();
}

int RtlMsvcrtUngetc(int ch, void *stream)
{
    if (!rustos_stream_is_stdin(stream)
        || ch == RUSTOS_NTDLL_EOF
        || rustos_ungetc_byte != RUSTOS_NTDLL_EOF) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return RUSTOS_NTDLL_EOF;
    }
    rustos_ungetc_byte = ch & 0xff;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return ch & 0xff;
}

int RtlMsvcrtVscanf(const char *format, RUSTOS_VA_LIST ap)
{
    return rustos_vscanf_internal(format, ap);
}
