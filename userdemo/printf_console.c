#include <stdio.h>
#include <string.h>

int main(void) {
    char line[256];

    printf("gcc stdio console ready\r\n");
    printf("type text and press enter. ctrl+c is not implemented.\r\n");
    fflush(stdout);

    while (fgets(line, sizeof(line), stdin) != NULL) {
        size_t len = strlen(line);
        printf("echo> %s", line);
        if (len == 0 || line[len - 1] != '\n') {
            printf("\r\n");
        }
        fflush(stdout);
    }

    printf("stdin closed\r\n");
    fflush(stdout);
    return 0;
}
