#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void log_line(const char *message) {
    printf("%s\r\n", message);
    fflush(stdout);
}

int main(void) {
    const char *qemu_mode = getenv("RUSTOS_NETPROBE_QEMU");
    int qemu_gateway = qemu_mode != NULL && strcmp(qemu_mode, "1") == 0;
    const char *target = qemu_gateway ? "10.0.2.2" : "142.250.72.14";
    unsigned short port = qemu_gateway ? 9 : 80;
    log_line(qemu_gateway ? "netprobe: start target=qemu-gateway" : "netprobe: start");

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("netprobe: socket failed errno=%d\r\n", errno);
        fflush(stdout);
        return 1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, target, &addr.sin_addr) != 1) {
        log_line("netprobe: inet_pton failed");
        close(fd);
        return 1;
    }

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        if (qemu_gateway && errno == ECONNREFUSED) {
            log_line("netprobe: qemu gateway reachable");
            close(fd);
            return 0;
        }
        printf("netprobe: connect failed errno=%d\r\n", errno);
        fflush(stdout);
        close(fd);
        return 1;
    }
    if (qemu_gateway) {
        log_line("netprobe: qemu gateway reachable");
        close(fd);
        return 0;
    }
    log_line("netprobe: connect ok");

    const char request[] =
        "GET / HTTP/1.0\r\n"
        "Host: google.com\r\n"
        "Connection: close\r\n"
        "\r\n";
    ssize_t sent = send(fd, request, sizeof(request) - 1, 0);
    if (sent < 0) {
        printf("netprobe: send failed errno=%d\r\n", errno);
        fflush(stdout);
        close(fd);
        return 1;
    }
    printf("netprobe: sent %ld bytes\r\n", (long)sent);
    fflush(stdout);

    char buffer[512];
    ssize_t received = recv(fd, buffer, sizeof(buffer) - 1, 0);
    if (received < 0) {
        printf("netprobe: recv failed errno=%d\r\n", errno);
        fflush(stdout);
        close(fd);
        return 1;
    }
    buffer[received] = '\0';
    printf("netprobe: received %ld bytes\r\n", (long)received);
    printf("netprobe: response begin\r\n%.*s\r\nnetprobe: response end\r\n", (int)received, buffer);
    fflush(stdout);

    close(fd);
    return 0;
}
