#include <stddef.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

enum {
    INPUT_BUFFER_LEN = 256,
};

static const char BANNER_PREFIX[] = "musl static echo ready: ";
static const char NEWLINE[] = "\n";

static int write_all(int fd, const char *buffer, size_t len) {
    while (len > 0) {
        ssize_t written = write(fd, buffer, len);

        if (written < 0) {
            return -1;
        }

        buffer += (size_t)written;
        len -= (size_t)written;
    }

    return 0;
}

static void idle_once(void) {
    static const struct timespec request = {
        .tv_sec = 0,
        .tv_nsec = 1 * 1000 * 1000,
    };

    (void)nanosleep(&request, NULL);
}

int main(int argc, char **argv) {
    char input[INPUT_BUFFER_LEN];
    const char *program_name = (argc > 0 && argv[0] != NULL) ? argv[0] : "userdemo";

    if (write_all(STDOUT_FILENO, BANNER_PREFIX, sizeof(BANNER_PREFIX) - 1) < 0) {
        return 1;
    }
    if (write_all(STDOUT_FILENO, program_name, strlen(program_name)) < 0) {
        return 1;
    }
    if (write_all(STDOUT_FILENO, NEWLINE, sizeof(NEWLINE) - 1) < 0) {
        return 1;
    }

    for (;;) {
        ssize_t read_len = read(STDIN_FILENO, input, sizeof(input));

        if (read_len < 0) {
            return 1;
        }
        if (read_len == 0) {
            idle_once();
            continue;
        }
        if (write_all(STDOUT_FILENO, input, (size_t)read_len) < 0) {
            return 1;
        }
    }
}
