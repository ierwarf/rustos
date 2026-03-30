#include <stdio.h>
#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void print_prompt(void) {
    char cwd[PATH_MAX];
    if (getcwd(cwd, sizeof(cwd)) == NULL) {
        strcpy(cwd, "?");
    }
    printf("%s $ ", cwd);
    fflush(stdout);
}

static void command_ls(const char *path) {
    const char *target = (path == NULL || *path == '\0') ? "." : path;
    DIR *dir = opendir(target);
    if (dir == NULL) {
        printf("ls: %s: %s\r\n", target, strerror(errno));
        return;
    }

    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
            continue;
        }
        printf("%s\r\n", entry->d_name);
    }

    closedir(dir);
}

static void command_cd(const char *path) {
    const char *target = (path == NULL || *path == '\0') ? "/" : path;
    if (chdir(target) != 0) {
        printf("cd: %s: %s\r\n", target, strerror(errno));
    }
}

static void command_pwd(void) {
    char cwd[PATH_MAX];
    if (getcwd(cwd, sizeof(cwd)) == NULL) {
        printf("pwd: %s\r\n", strerror(errno));
        return;
    }
    printf("%s\r\n", cwd);
}

int main(void) {
    char line[256];

    printf("rustos shell ready\r\n");
    printf("builtins: cd, ls, pwd, exit\r\n");
    fflush(stdout);

    for (;;) {
        char *command;
        char *arg;

        print_prompt();
        if (fgets(line, sizeof(line), stdin) == NULL) {
            break;
        }

        line[strcspn(line, "\r\n")] = '\0';
        command = strtok(line, " \t");
        if (command == NULL) {
            continue;
        }

        arg = strtok(NULL, "");

        if (strcmp(command, "cd") == 0) {
            command_cd(arg);
        } else if (strcmp(command, "ls") == 0) {
            command_ls(arg);
        } else if (strcmp(command, "pwd") == 0) {
            command_pwd();
        } else if (strcmp(command, "exit") == 0) {
            break;
        } else {
            printf("%s: command not found\r\n", command);
        }
        fflush(stdout);
    }

    printf("stdin closed\r\n");
    fflush(stdout);
    return 0;
}
