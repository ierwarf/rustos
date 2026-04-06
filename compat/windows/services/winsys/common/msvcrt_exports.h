#ifndef WINSYS_MSVCRT_EXPORTS_H
#define WINSYS_MSVCRT_EXPORTS_H

#include "ntdll_exports.h"
#include "windows_runtime.h"

#define RUSTOS_EOF (-1)
#define RUSTOS_CRT_EINVAL 22
#define RUSTOS_CRT_ENOMEM 12
#define RUSTOS_CRT_EIO 5
#define RUSTOS_CRT_ERANGE 34
#define RUSTOS_HEAP_ZERO_MEMORY 0x00000008u

extern void *stdin;
extern void *stdout;
extern void *stderr;
extern char *_acmdln;
extern char **__initenv;
extern int _commode;
extern int _fmode;

int rustos_vfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap);
int rustos_vscanf(const char *format, RUSTOS_VA_LIST ap);

int __getmainargs(int *argc, char ***argv, char ***envp, int do_wildcard, void *startup_info);
int *__p___argc(void);
char ***__p___argv(void);
char ***__p__environ(void);
char **_get_initial_narrow_environment(void);
void exit(int status);
void _exit(int status);
void abort(void);
void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);
void *calloc(size_t count, size_t size);
int vfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap);
int printf(const char *format, ...);
int fprintf(void *stream, const char *format, ...);
int scanf(const char *format, ...);
int puts(const char *text);
int putchar(int ch);
int getchar(void);
char *fgets(char *buffer, int len, void *stream);
int fflush(void *stream);
long __C_specific_handler(void);
UINT ___lc_codepage_func(void);
int ___mb_cur_max_func(void);
void *__acrt_iob_func(UINT index);
void *__iob_func(void);
void __set_app_type(UINT app_type);
void *__setusermatherr(void *handler);
void _amsg_exit(int status);
int _cexit(void);
int *_errno(void);
void _initterm(void (**start)(void), void (**end)(void));
void _lock(int lock_num);
void *_onexit(void *callback);
void _unlock(int lock_num);
int fputc(int ch, void *stream);
size_t fwrite(const void *buffer, size_t size, size_t count, void *stream);
void *localeconv(void);
void *memcpy(void *dst, const void *src, size_t len);
void *memset(void *dst, int value, size_t len);
void *signal(int signum, void *handler);
char *strerror(int errnum);
size_t strlen(const char *text);
int strncmp(const char *lhs, const char *rhs, size_t len);
size_t wcslen(const WCHAR *text);
int getc(void *stream);
int isspace(int ch);
int isxdigit(int ch);
long strtol(const char *text, char **end_ptr, int base);
unsigned long strtoul(const char *text, char **end_ptr, int base);
int tolower(int ch);
int ungetc(int ch, void *stream);
unsigned long long _strtoui64(const char *text, char **end_ptr, int base);
long long _strtoi64(const char *text, char **end_ptr, int base);

#endif
