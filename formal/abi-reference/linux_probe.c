#define _GNU_SOURCE

#include <fcntl.h>
#include <linux/stat.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define PAIR(name, value) printf(name "=%llu\n", (unsigned long long)(value))

int main(void) {
    PAIR("af_unix", AF_UNIX);
    PAIR("epoll_cloexec", EPOLL_CLOEXEC);
    PAIR("epoll_ctl_add", EPOLL_CTL_ADD);
    PAIR("epoll_ctl_del", EPOLL_CTL_DEL);
    PAIR("epoll_ctl_mod", EPOLL_CTL_MOD);
    PAIR("epollerr", EPOLLERR);
    PAIR("epollet", EPOLLET);
    PAIR("epollhup", EPOLLHUP);
    PAIR("epollin", EPOLLIN);
    PAIR("epollout", EPOLLOUT);
    PAIR("f_dupfd_cloexec", F_DUPFD_CLOEXEC);
    PAIR("map_anonymous", MAP_ANONYMOUS);
    PAIR("map_fixed", MAP_FIXED);
    PAIR("map_private", MAP_PRIVATE);
    PAIR("msg_cmsg_cloexec", MSG_CMSG_CLOEXEC);
    PAIR("msg_dontwait", MSG_DONTWAIT);
    PAIR("o_cloexec", O_CLOEXEC);
    PAIR("o_nonblock", O_NONBLOCK);
    PAIR("offset_epoll_event_data", offsetof(struct epoll_event, data));
    PAIR("offset_msghdr_control", offsetof(struct msghdr, msg_control));
    PAIR("pollerr", POLLERR);
    PAIR("pollhup", POLLHUP);
    PAIR("pollin", POLLIN);
    PAIR("pollout", POLLOUT);
    PAIR("prot_exec", PROT_EXEC);
    PAIR("prot_read", PROT_READ);
    PAIR("prot_write", PROT_WRITE);
    PAIR("scm_rights", SCM_RIGHTS);
    PAIR("size_cmsghdr", sizeof(struct cmsghdr));
    PAIR("size_epoll_event", sizeof(struct epoll_event));
    PAIR("size_iovec", sizeof(struct iovec));
    PAIR("size_msghdr", sizeof(struct msghdr));
    PAIR("size_pollfd", sizeof(struct pollfd));
    PAIR("size_sockaddr_un", sizeof(struct sockaddr_un));
    PAIR("size_stat", sizeof(struct stat));
    PAIR("size_statx", sizeof(struct statx));
    PAIR("size_timespec", sizeof(struct timespec));
    PAIR("sock_cloexec", SOCK_CLOEXEC);
    PAIR("sock_nonblock", SOCK_NONBLOCK);
    PAIR("sol_socket", SOL_SOCKET);
    PAIR("sys_epoll_create1", SYS_epoll_create1);
    PAIR("sys_epoll_ctl", SYS_epoll_ctl);
    PAIR("sys_epoll_wait", SYS_epoll_wait);
    PAIR("sys_mmap", SYS_mmap);
    PAIR("sys_recvmsg", SYS_recvmsg);
    PAIR("sys_sendmsg", SYS_sendmsg);
    PAIR("sys_socketpair", SYS_socketpair);
    return 0;
}
