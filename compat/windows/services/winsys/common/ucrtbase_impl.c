#include "ucrtbase_exports.h"

int *__p__commode(void)
{
    return &_commode;
}

int *__p__fmode(void)
{
    return &_fmode;
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
