#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void log_line(const char *message) {
    printf("%s\r\n", message);
    fflush(stdout);
}

int main(void) {
    log_line("netprobe: start");

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("netprobe: socket failed errno=%d\r\n", errno);
        fflush(stdout);
        return 1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(80);
    if (inet_pton(AF_INET, "142.250.72.14", &addr.sin_addr) != 1) {
        log_line("netprobe: inet_pton failed");
        close(fd);
        return 1;
    }

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        printf("netprobe: connect failed errno=%d\r\n", errno);
        fflush(stdout);
        close(fd);
        return 1;
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
