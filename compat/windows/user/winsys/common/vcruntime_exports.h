#ifndef WINSYS_VCRUNTIME_EXPORTS_H
#define WINSYS_VCRUNTIME_EXPORTS_H

#include "ntdll_exports.h"

extern ULONGLONG __security_cookie;
extern ULONGLONG __security_cookie_complement;

long __C_specific_handler(void);
void __security_init_cookie(void);
void __security_check_cookie(ULONGLONG cookie);
void _initterm(void (**start)(void), void (**end)(void));
int _initterm_e(int (**start)(void), int (**end)(void));
int _crt_atexit(void (*callback)(void));
void _register_thread_local_exe_atexit_callback(void *callback);
void *__current_exception(void);
void *__current_exception_context(void);

#endif
