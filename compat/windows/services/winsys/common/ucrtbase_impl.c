#include "ucrtbase_exports.h"

#define RUSTOS_UCRT_EINVAL 22
#define RUSTOS_UCRT_ENABLE_PER_THREAD_LOCALE 1
#define RUSTOS_UCRT_DISABLE_PER_THREAD_LOCALE 2
#define RUSTOS_UCRT_IOFBF 0x0000
#define RUSTOS_UCRT_IOLBF 0x0040
#define RUSTOS_UCRT_IONBF 0x0004
#define RUSTOS_UCRT_O_TEXT 0x4000
#define RUSTOS_UCRT_O_BINARY 0x8000

static int rustos_new_mode;
static int rustos_locale_mode = RUSTOS_UCRT_DISABLE_PER_THREAD_LOCALE;
static _invalid_parameter_handler rustos_invalid_parameter_handler;
static int rustos_printf_count_output;
static unsigned int rustos_abort_behavior = 0x3u;

static void rustos_ucrt_set_errno(int value)
{
    int *errno_value = _errno();
    if (errno_value != NULL) {
        *errno_value = value;
    }
}

static void rustos_ucrt_invalid_parameter(void)
{
    rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
    if (rustos_invalid_parameter_handler != NULL) {
        rustos_invalid_parameter_handler(NULL, NULL, NULL, 0, 0);
    }
}

static int rustos_ucrt_invalid_result(void)
{
    rustos_ucrt_invalid_parameter();
    rustos_ucrt_set_errno(RUSTOS_UCRT_EINVAL);
    return -1;
}

int *__p__commode(void)
{
    return &_commode;
}

int *__p__fmode(void)
{
    return &_fmode;
}

int _set_new_mode(int mode)
{
    int previous;

    if (mode != 0 && mode != 1) {
        return rustos_ucrt_invalid_result();
    }
    previous = rustos_new_mode;
    rustos_new_mode = mode;
    return previous;
}

int _configthreadlocale(int mode)
{
    int previous;

    if (mode == 0) {
        return rustos_locale_mode;
    }
    if (mode != RUSTOS_UCRT_ENABLE_PER_THREAD_LOCALE
        && mode != RUSTOS_UCRT_DISABLE_PER_THREAD_LOCALE) {
        return rustos_ucrt_invalid_result();
    }
    previous = rustos_locale_mode;
    rustos_locale_mode = mode;
    return previous;
}

_invalid_parameter_handler _set_invalid_parameter_handler(
    _invalid_parameter_handler handler)
{
    _invalid_parameter_handler previous = rustos_invalid_parameter_handler;
    rustos_invalid_parameter_handler = handler;
    return previous;
}

_invalid_parameter_handler _get_invalid_parameter_handler(void)
{
    return rustos_invalid_parameter_handler;
}

int setvbuf(void *stream, char *buffer, int mode, size_t size)
{
    (void)buffer;
    if (stream == NULL
        || (mode != RUSTOS_UCRT_IOFBF
            && mode != RUSTOS_UCRT_IOLBF
            && mode != RUSTOS_UCRT_IONBF)
        || (mode != RUSTOS_UCRT_IONBF
            && (size < 2 || size > 0x7fffffffULL))) {
        return rustos_ucrt_invalid_result();
    }
    return 0;
}

int _set_fmode(int mode)
{
    if (mode != RUSTOS_UCRT_O_TEXT && mode != RUSTOS_UCRT_O_BINARY) {
        return rustos_ucrt_invalid_result();
    }
    _fmode = mode;
    return 0;
}

int _get_fmode(int *mode)
{
    if (mode == NULL) {
        return rustos_ucrt_invalid_result();
    }
    *mode = _fmode;
    return 0;
}

int _set_printf_count_output(int value)
{
    int previous;

    if (value != 0 && value != 1) {
        return rustos_ucrt_invalid_result();
    }
    previous = rustos_printf_count_output;
    rustos_printf_count_output = value;
    return previous;
}

int _get_printf_count_output(void)
{
    return rustos_printf_count_output;
}

unsigned int _set_abort_behavior(unsigned int flags, unsigned int mask)
{
    unsigned int previous = rustos_abort_behavior;
    rustos_abort_behavior = (rustos_abort_behavior & ~mask) | (flags & mask);
    return previous;
}

int _crt_atexit(void (*callback)(void))
{
    return RtlMsvcrtOnexit(callback) != 0 ? 0 : -1;
}

int _configure_narrow_argv(int mode)
{
    (void)mode;
    return 0;
}

int _initialize_narrow_environment(void)
{
    return 0;
}

char *_get_narrow_winmain_command_line(void)
{
    return _acmdln;
}

int __stdio_common_vfprintf(
    ULONGLONG options,
    void *stream,
    const char *format,
    void *locale,
    RUSTOS_VA_LIST ap)
{
    (void)options;
    (void)locale;
    return rustos_vfprintf(stream, format, ap);
}

int __stdio_common_vfscanf(
    ULONGLONG options,
    void *stream,
    const char *format,
    void *locale,
    RUSTOS_VA_LIST ap)
{
    (void)options;
    (void)stream;
    (void)locale;
    return rustos_vscanf(format, ap);
}

int _initialize_onexit_table(_onexit_table_t *table)
{
    if (table == NULL) {
        return -1;
    }
    table->first = NULL;
    table->last = NULL;
    table->end = NULL;
    return 0;
}

int _register_onexit_function(_onexit_table_t *table, void *callback)
{
    (void)table;
    return _onexit(callback) != NULL ? 0 : -1;
}

int _execute_onexit_table(_onexit_table_t *table)
{
    (void)table;
    return _cexit();
}

int _initterm_e(int (**start)(void), int (**end)(void))
{
    while (start != end) {
        if (*start != NULL) {
            int status = (*start)();
            if (status != 0) {
                return status;
            }
        }
        start++;
    }
    return 0;
}
