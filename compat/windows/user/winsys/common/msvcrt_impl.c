#include "msvcrt_exports.h"

void *stdin = NULL;
void *stdout = NULL;
void *stderr = NULL;
char *_acmdln = (char *)0;
char **__initenv = (char **)0;
int _commode = 0;
int _fmode = 0;

#define RUSTOS_EOF (-1)
#define RUSTOS_CRT_EINVAL 22
#define RUSTOS_CRT_ENOMEM 12
#define RUSTOS_CRT_EIO 5
#define RUSTOS_CRT_ERANGE 34
#define RUSTOS_HEAP_ZERO_MEMORY 0x00000008u

static UINT rustos_msvcrt_app_type = 0;
static void *rustos_msvcrt_user_matherr = NULL;
static void *rustos_signal_handlers[32];

static int rustos_ascii_isspace(int ch)
{
    return ch == ' ' || (ch >= '\t' && ch <= '\r');
}

static void rustos_sync_runtime_globals(void)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL) {
        return;
    }
    stdin = (void *)runtime->stdin_file_ptr;
    stdout = (void *)runtime->stdout_file_ptr;
    stderr = (void *)runtime->stderr_file_ptr;
    _acmdln = (char *)runtime->command_line_a_ptr;
    __initenv = (char **)runtime->initial_narrow_environment_ptr;
    if (runtime->commode_ptr != 0) {
        _commode = *(int *)runtime->commode_ptr;
    }
    if (runtime->fmode_ptr != 0) {
        _fmode = *(int *)runtime->fmode_ptr;
    }
}

static void rustos_set_errno_value(int value)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL || runtime->errno_ptr == 0) {
        return;
    }
    *(volatile int *)runtime->errno_ptr = value;
}

static void rustos_exit_process(UINT status)
{
    RtlExitUserProcess(status);
}

static void *rustos_process_heap(void)
{
    RustosPebLite *peb = rustos_current_peb();
    if (peb != NULL && peb->process_heap != 0) {
        return (void *)peb->process_heap;
    }
    return (void *)RUSTOS_HANDLE_PROCESS_HEAP;
}

static char *rustos_strerror_message(int errnum)
{
    switch (errnum) {
    case RUSTOS_CRT_EINVAL:
        return "Invalid argument";
    case RUSTOS_CRT_ENOMEM:
        return "Not enough memory";
    case RUSTOS_CRT_EIO:
        return "Input/output error";
    case RUSTOS_CRT_ERANGE:
        return "Result too large";
    default:
        return "Unknown error";
    }
}

static int rustos_ascii_isxdigit(int ch)
{
    return (ch >= '0' && ch <= '9')
        || (ch >= 'a' && ch <= 'f')
        || (ch >= 'A' && ch <= 'F');
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

static unsigned long long rustos_parse_unsigned_integer(
    const char *text,
    char **end_ptr,
    int base,
    int *negative,
    int *overflowed)
{
    const char *cursor = text;
    const char *digits_start;
    unsigned long long value = 0;
    unsigned long long max_before_mul;
    unsigned long long max_remainder;
    int sign = 0;
    int digit;
    if (negative != NULL) {
        *negative = 0;
    }
    if (overflowed != NULL) {
        *overflowed = 0;
    }
    if (text == NULL) {
        if (end_ptr != NULL) {
            *end_ptr = NULL;
        }
        return 0;
    }
    while (rustos_ascii_isspace((unsigned char)*cursor)) {
        cursor++;
    }
    if (*cursor == '+' || *cursor == '-') {
        sign = *cursor == '-';
        cursor++;
    }
    if (base == 0) {
        if (cursor[0] == '0'
            && (cursor[1] == 'x' || cursor[1] == 'X')
            && rustos_ascii_isxdigit((unsigned char)cursor[2])) {
            base = 16;
            cursor += 2;
        } else if (cursor[0] == '0') {
            base = 8;
        } else {
            base = 10;
        }
    } else if (base == 16
        && cursor[0] == '0'
        && (cursor[1] == 'x' || cursor[1] == 'X')
        && rustos_ascii_isxdigit((unsigned char)cursor[2])) {
        cursor += 2;
    }
    if (base < 2 || base > 36) {
        if (end_ptr != NULL) {
            *end_ptr = (char *)text;
        }
        return 0;
    }
    digits_start = cursor;
    max_before_mul = ~0ULL / (unsigned)base;
    max_remainder = ~0ULL % (unsigned)base;
    while ((digit = rustos_ascii_digit_value((unsigned char)*cursor)) >= 0 && digit < base) {
        if (value > max_before_mul
            || (value == max_before_mul && (unsigned)digit > max_remainder)) {
            value = ~0ULL;
            if (overflowed != NULL) {
                *overflowed = 1;
            }
        } else if (value != ~0ULL) {
            value = value * (unsigned)base + (unsigned)digit;
        }
        cursor++;
    }
    if (end_ptr != NULL) {
        *end_ptr = (char *)(cursor == digits_start ? text : cursor);
    }
    if (negative != NULL) {
        *negative = sign;
    }
    return value;
}

int rustos_vfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap)
{
    rustos_sync_runtime_globals();
    return RtlMsvcrtVfprintf(stream, format, ap);
}

int rustos_vscanf(const char *format, RUSTOS_VA_LIST ap)
{
    rustos_sync_runtime_globals();
    return RtlMsvcrtVscanf(format, ap);
}

int __getmainargs(int *argc, char ***argv, char ***envp, int do_wildcard, void *startup_info)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    rustos_sync_runtime_globals();
    (void)do_wildcard;
    (void)startup_info;
    if (runtime == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return -1;
    }
    if (argc != NULL) {
        *argc = *(int *)runtime->argc_ptr;
    }
    if (argv != NULL) {
        *argv = (char **)runtime->argv_ptr;
    }
    if (envp != NULL) {
        *envp = (char **)runtime->environ_ptr;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return 0;
}

int *__p___argc(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (int *)runtime->argc_ptr : NULL;
}

char ***__p___argv(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (char ***)runtime->argv_ptr_ptr : NULL;
}

char ***__p__environ(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (char ***)runtime->environ_ptr_ptr : NULL;
}

char **_get_initial_narrow_environment(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (char **)runtime->initial_narrow_environment_ptr : NULL;
}

void exit(int status)
{
    rustos_exit_process((UINT)status);
}

void _exit(int status)
{
    rustos_exit_process((UINT)status);
}

void abort(void)
{
    rustos_exit_process(3u);
}

void *malloc(size_t size)
{
    rustos_sync_runtime_globals();
    return RtlAllocateHeap(rustos_process_heap(), 0, size == 0 ? 1 : size);
}

void free(void *ptr)
{
    if (ptr == NULL) {
        return;
    }
    rustos_sync_runtime_globals();
    RtlFreeHeap(rustos_process_heap(), 0, ptr);
}

void *realloc(void *ptr, size_t size)
{
    rustos_sync_runtime_globals();
    return RtlReAllocateHeap(rustos_process_heap(), 0, ptr, size == 0 ? 1 : size);
}

void *calloc(size_t count, size_t size)
{
    rustos_sync_runtime_globals();
    return RtlAllocateHeap(rustos_process_heap(), RUSTOS_HEAP_ZERO_MEMORY, count * size);
}

int vfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap)
{
    return rustos_vfprintf(stream, format, ap);
}

int printf(const char *format, ...)
{
    int result;
    RUSTOS_VA_LIST ap;
    rustos_sync_runtime_globals();
    __builtin_ms_va_start(ap, format);
    result = rustos_vfprintf(stdout, format, ap);
    __builtin_ms_va_end(ap);
    return result;
}

int fprintf(void *stream, const char *format, ...)
{
    int result;
    RUSTOS_VA_LIST ap;
    rustos_sync_runtime_globals();
    __builtin_ms_va_start(ap, format);
    result = rustos_vfprintf(stream, format, ap);
    __builtin_ms_va_end(ap);
    return result;
}

int scanf(const char *format, ...)
{
    int result;
    RUSTOS_VA_LIST ap;
    rustos_sync_runtime_globals();
    __builtin_ms_va_start(ap, format);
    result = rustos_vscanf(format, ap);
    __builtin_ms_va_end(ap);
    return result;
}

int puts(const char *text)
{
    return RtlMsvcrtPuts(text);
}

int putchar(int ch)
{
    return RtlMsvcrtPutchar(ch);
}

int getchar(void)
{
    return RtlMsvcrtGetchar();
}

char *fgets(char *buffer, int len, void *stream)
{
    return RtlMsvcrtFgets(buffer, (UINT)(unsigned int)len, stream);
}

int fflush(void *stream)
{
    return RtlMsvcrtFflush(stream);
}

long __C_specific_handler(void)
{
    return 0;
}

UINT ___lc_codepage_func(void)
{
    return 65001u;
}

int ___mb_cur_max_func(void)
{
    return 4;
}

void *__acrt_iob_func(UINT index)
{
    rustos_sync_runtime_globals();
    switch (index) {
    case 0:
        return stdin;
    case 1:
        return stdout;
    case 2:
        return stderr;
    default:
        return NULL;
    }
}

void *__iob_func(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (void *)runtime->iob_array_ptr : NULL;
}

void __set_app_type(UINT app_type)
{
    rustos_msvcrt_app_type = app_type;
}

void *__setusermatherr(void *handler)
{
    void *previous = rustos_msvcrt_user_matherr;
    rustos_msvcrt_user_matherr = handler;
    return previous;
}

void _amsg_exit(int status)
{
    rustos_exit_process((UINT)status);
}

int _cexit(void)
{
    rustos_set_errno_value(0);
    return 0;
}

int *_errno(void)
{
    rustos_sync_runtime_globals();
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (int *)runtime->errno_ptr : NULL;
}

void _initterm(void (**start)(void), void (**end)(void))
{
    while (start != end) {
        if (*start != NULL) {
            (*start)();
        }
        start++;
    }
}

void _lock(int lock_num)
{
    (void)lock_num;
}

void *_onexit(void *callback)
{
    return RtlMsvcrtOnexit(callback);
}

void _unlock(int lock_num)
{
    (void)lock_num;
}

int fputc(int ch, void *stream)
{
    return RtlMsvcrtFputc(ch, stream);
}

size_t fwrite(const void *buffer, size_t size, size_t count, void *stream)
{
    return RtlMsvcrtFwrite(buffer, size, count, stream);
}

void *localeconv(void)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    return runtime != NULL ? (void *)runtime->localeconv_ptr : NULL;
}

void *memcpy(void *dst, const void *src, size_t len)
{
    unsigned char *out = (unsigned char *)dst;
    const unsigned char *in = (const unsigned char *)src;
    size_t index;
    for (index = 0; index < len; index++) {
        out[index] = in[index];
    }
    return dst;
}

void *memset(void *dst, int value, size_t len)
{
    unsigned char *out = (unsigned char *)dst;
    size_t index;
    for (index = 0; index < len; index++) {
        out[index] = (unsigned char)value;
    }
    return dst;
}

void *signal(int signum, void *handler)
{
    void *previous;
    if (signum < 0
        || signum >= (int)(sizeof(rustos_signal_handlers) / sizeof(rustos_signal_handlers[0]))) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        rustos_set_errno_value(RUSTOS_CRT_EINVAL);
        return (void *)-1;
    }
    previous = rustos_signal_handlers[signum];
    rustos_signal_handlers[signum] = handler;
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    rustos_set_errno_value(0);
    return previous;
}

char *strerror(int errnum)
{
    return rustos_strerror_message(errnum);
}

size_t strlen(const char *text)
{
    size_t len = 0;
    while (text[len] != '\0') {
        len++;
    }
    return len;
}

int strncmp(const char *lhs, const char *rhs, size_t len)
{
    size_t index;
    for (index = 0; index < len; index++) {
        unsigned char left = (unsigned char)lhs[index];
        unsigned char right = (unsigned char)rhs[index];
        if (left != right) {
            return (int)left - (int)right;
        }
        if (left == 0) {
            return 0;
        }
    }
    return 0;
}

size_t wcslen(const WCHAR *text)
{
    size_t len = 0;
    while (text[len] != 0) {
        len++;
    }
    return len;
}

int getc(void *stream)
{
    return RtlMsvcrtGetc(stream);
}

int isspace(int ch)
{
    return rustos_ascii_isspace((unsigned char)ch);
}

int isxdigit(int ch)
{
    return rustos_ascii_isxdigit((unsigned char)ch);
}

long strtol(const char *text, char **end_ptr, int base)
{
    int negative = 0;
    int overflowed = 0;
    unsigned long long value =
        rustos_parse_unsigned_integer(text, end_ptr, base, &negative, &overflowed);
    if (overflowed) {
        return negative ? (long)(-1L - __LONG_MAX__) : __LONG_MAX__;
    }
    if (negative) {
        return (long)(-(long long)value);
    }
    return (long)value;
}

unsigned long strtoul(const char *text, char **end_ptr, int base)
{
    int negative = 0;
    int overflowed = 0;
    unsigned long long value =
        rustos_parse_unsigned_integer(text, end_ptr, base, &negative, &overflowed);
    if (negative) {
        return (unsigned long)(0u - (unsigned long)value);
    }
    return (unsigned long)value;
}

int tolower(int ch)
{
    return rustos_ascii_tolower((unsigned char)ch);
}

int ungetc(int ch, void *stream)
{
    return RtlMsvcrtUngetc(ch, stream);
}

unsigned long long _strtoui64(const char *text, char **end_ptr, int base)
{
    int negative = 0;
    return rustos_parse_unsigned_integer(text, end_ptr, base, &negative, NULL);
}

long long _strtoi64(const char *text, char **end_ptr, int base)
{
    int negative = 0;
    int overflowed = 0;
    unsigned long long value =
        rustos_parse_unsigned_integer(text, end_ptr, base, &negative, &overflowed);
    if (overflowed) {
        return negative ? (-1LL - __LONG_LONG_MAX__) : __LONG_LONG_MAX__;
    }
    if (negative) {
        return -(long long)value;
    }
    return (long long)value;
}
