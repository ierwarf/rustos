/* Minimal baseline-only PID 1. No RustOS DVM service or appliance is modified.
 * The exact ipcbench ELF also used by RustOS runs against ordinary Linux ABIs.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    char suite[64] = {0};
    mkdir("/proc", 0755);
    mkdir("/tmp", 01777);
    if (mount("proc", "/proc", "proc", 0, NULL) != 0) {
        printf("linux-baseline: FAIL mount-proc errno=%d\n", errno);
        goto done;
    }
    FILE *file = fopen("/suite", "r");
    if (!file || !fgets(suite, sizeof(suite), file)) {
        puts("linux-baseline: FAIL suite-file");
        goto done;
    }
    fclose(file);
    suite[strcspn(suite, "\r\n")] = 0;
    pid_t child = fork();
    if (child == 0) {
        execl("/ipcbench", "/ipcbench", "--linux-baseline", suite, (char *)NULL);
        _exit(125);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("linux-baseline: FAIL child-status=%d\n", status);
    } else {
        puts("linux-baseline: child-complete v=1");
    }
done:
    fflush(NULL);
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
