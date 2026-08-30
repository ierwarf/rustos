#ifndef WINSYS_UCRTBASE_EXPORTS_H
#define WINSYS_UCRTBASE_EXPORTS_H

#include "msvcrt_exports.h"

typedef struct _onexit_table_t {
    void **first;
    void **last;
    void **end;
} _onexit_table_t;

typedef void (*_invalid_parameter_handler)(
    const WCHAR *expression,
    const WCHAR *function,
    const WCHAR *file,
    UINT line,
    ULONGLONG reserved);

int _set_new_mode(int mode);
int _configthreadlocale(int mode);
_invalid_parameter_handler _set_invalid_parameter_handler(
    _invalid_parameter_handler handler);
_invalid_parameter_handler _get_invalid_parameter_handler(void);
int setvbuf(void *stream, char *buffer, int mode, size_t size);
int _set_fmode(int mode);
int _get_fmode(int *mode);
int _set_printf_count_output(int value);
int _get_printf_count_output(void);
unsigned int _set_abort_behavior(unsigned int flags, unsigned int mask);

int _crt_atexit(void (*callback)(void));

int *__p__commode(void);
int *__p__fmode(void);
int _configure_narrow_argv(int mode);
int _initialize_narrow_environment(void);
char *_get_narrow_winmain_command_line(void);
int __stdio_common_vfprintf(
    ULONGLONG options,
    void *stream,
    const char *format,
    void *locale,
    RUSTOS_VA_LIST ap);
int __stdio_common_vfscanf(
    ULONGLONG options,
    void *stream,
    const char *format,
    void *locale,
    RUSTOS_VA_LIST ap);
int _initialize_onexit_table(_onexit_table_t *table);
int _register_onexit_function(_onexit_table_t *table, void *callback);
int _execute_onexit_table(_onexit_table_t *table);
int _initterm_e(int (**start)(void), int (**end)(void));

#endif
