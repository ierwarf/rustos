#ifndef WINSYS_UCRTBASE_EXPORTS_H
#define WINSYS_UCRTBASE_EXPORTS_H

#include "msvcrt_exports.h"

typedef struct _onexit_table_t {
    void **first;
    void **last;
    void **end;
} _onexit_table_t;

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
