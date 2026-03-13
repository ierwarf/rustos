#include <stdint.h>

#include "runtime.h"

int main(int argc, char **argv);

_Noreturn void _start(uint64_t argc, char **argv) {
    exit(main((int)argc, argv));
}
