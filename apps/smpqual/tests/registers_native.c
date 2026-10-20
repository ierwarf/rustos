/* Host ABI/comparator sanity and fault sensitivity only. This is NOT evidence
 * that RustOS executed any privileged context switch or return instruction. */
#include <stdint.h>
#include <stdio.h>
extern unsigned long hecs_register_probe(uint64_t seed, unsigned long operation);
int main(void) {
    const unsigned long operations[] = {186, 24, 35, 0};
    for (unsigned int path = 0; path < 4; ++path) {
        for (uint64_t iteration = 0; iteration < 256; ++iteration) {
            uint64_t seed = UINT64_C(0x123456789abcdef0) ^ ((uint64_t)path << 32) ^ iteration;
            unsigned long mismatch = hecs_register_probe(seed, operations[path]);
            if (mismatch != 0) {
                fprintf(stderr, "path=%lu iteration=%llu mismatch=%lu\n", operations[path],
                        (unsigned long long)iteration, mismatch);
                return 1;
            }
        }
    }
    puts("native assembly ABI/comparator passed; NOT RustOS hardware evidence");
    return 0;
}
