#include "vcruntime_exports.h"

ULONGLONG __security_cookie = 0x00002B992DDFA232ULL;
ULONGLONG __security_cookie_complement = ~0x00002B992DDFA232ULL;

long __C_specific_handler(void)
{
    return 0;
}

void __security_init_cookie(void)
{
    if (__security_cookie == 0 || __security_cookie == 0x00002B992DDFA232ULL) {
        __security_cookie = 0xBADC0FFEE0DDF00DULL;
    }
    __security_cookie_complement = ~__security_cookie;
}

void __security_check_cookie(ULONGLONG cookie)
{
    if (cookie != __security_cookie) {
        RtlExitUserProcess(0);
    }
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

int _crt_atexit(void (*callback)(void))
{
    return RtlMsvcrtOnexit(callback) != 0 ? 0 : -1;
}

void _register_thread_local_exe_atexit_callback(void *callback)
{
    (void)callback;
}

void *__current_exception(void)
{
    return NULL;
}

void *__current_exception_context(void)
{
    return NULL;
}
