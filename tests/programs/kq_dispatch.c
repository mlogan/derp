// kq_dispatch server PORT | client HOST PORT
// EV_DISPATCH on a listening socket: one event, then silence while the
// connection is still waiting, until the registration is enabled again.
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/socket.h>
#include <unistd.h>

static void die(const char *what) {
    perror(what);
    exit(1);
}

static int events(int kq, struct kevent *change, const struct timespec *timeout) {
    struct kevent out;
    int n = kevent(kq, change, change ? 1 : 0, &out, 1, timeout);
    if (n < 0) die("kevent");
    return n;
}

static int server(const char *port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons((uint16_t)atoi(port))};
    if (bind(fd, (struct sockaddr *)&a, sizeof a) != 0) die("bind");
    if (listen(fd, 4) != 0) die("listen");
    int kq = kqueue();
    struct kevent add, enable;
    const struct timespec now = {0, 0};
    EV_SET(&add, fd, EVFILT_READ, EV_ADD | EV_DISPATCH, 0, 0, NULL);
    EV_SET(&enable, fd, EVFILT_READ, EV_ENABLE | EV_DISPATCH, 0, 0, NULL);
    printf("first wait: %d\n", events(kq, &add, NULL));
    printf("while disabled: %d\n", events(kq, NULL, &now));
    printf("enabled again: %d\n", events(kq, &enable, &now));
    int c = accept(fd, NULL, NULL);
    if (c < 0) die("accept");
    printf("after accept: %d\n", events(kq, &enable, &now));
    close(c);
    return 0;
}

static int client(const char *host, const char *port) {
    struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_STREAM}, *res = NULL;
    if (getaddrinfo(host, port, &hints, &res) != 0) die("getaddrinfo");
    for (;;) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(fd, res->ai_addr, res->ai_addrlen) == 0) {
            char c;
            // Until the server has accepted and hung up
            while (read(fd, &c, 1) > 0) {}
            return 0;
        }
        if (errno != ECONNREFUSED) die("connect");
        close(fd);
        usleep(5000);
    }
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) return server(argv[2]);
    if (argc == 4 && strcmp(argv[1], "client") == 0) return client(argv[2], argv[3]);
    return 2;
}
