/* Run this PE inside RustOS to exercise the installed msvcrt shim. */
#include "../../../compat/windows/services/winsys/common/msvcrt_exports.h"

_Static_assert(sizeof(long) == 4 && sizeof(void *) == 8, "requires Windows LLP64");

int main(void)
{
    char *end;
    char buffer[2];
    int *error = _errno();
    if (error == NULL) return 1;

    *error = 0;
    char overlap[8] = {'a', 'b', 'c', 'd', 'e', 'f', 'g', 0};
    if (memmove(overlap + 1, overlap, 6) != overlap + 1
        || memcmp(overlap, "aabcdef", 8) != 0) return 11;
    if (memmove(overlap, overlap + 1, 6) != overlap
        || memcmp(overlap, "abcdeff", 8) != 0) return 12;
    if (calloc((size_t)-1, 2) != NULL || *error != 12) return 2;
    if (fgets(buffer, -1, NULL) != NULL) return 3;

    *error = 0;
    if (strtol("2147483648!", &end, 10) != 2147483647L
        || *end != '!' || *error != 34) return 4;
    *error = 0;
    if (strtol("-2147483648", &end, 10) != (-2147483647L - 1)
        || *end != '\0' || *error != 0) return 5;
    *error = 0;
    if (strtoul("-4294967296", &end, 10) != 0xffffffffUL
        || *end != '\0' || *error != 34) return 6;
    *error = 0;
    if (_strtoi64("-9223372036854775808", &end, 10) != (-9223372036854775807LL - 1)
        || *end != '\0' || *error != 0) return 7;
    *error = 0;
    if (_strtoi64("9223372036854775808", &end, 10) != 9223372036854775807LL
        || *end != '\0' || *error != 34) return 8;
    *error = 0;
    if (_strtoui64("-1", &end, 10) != ~0ULL || *end != '\0' || *error != 0) return 9;
    *error = 0;
    if (_strtoui64("-18446744073709551616", &end, 10) != ~0ULL
        || *end != '\0' || *error != 34) return 10;
    return 0;
}
