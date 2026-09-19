// Numbered datagrams between two processes.
//
//   udp_ping server <port>
//   udp_ping client <host> <port> <count>
//
// The server answers every "ping N" with "pong N" and stops at "done". The
// client resends a ping until its pong arrives (a ping sent before the
// server is bound is lost, as UDP allows) and ignores stale pongs.
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static void die(const char *what) {
    perror(what);
    exit(2);
}

static int server(const char *port) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) die("socket");
    struct sockaddr_in me = {0};
    me.sin_family = AF_INET;
    me.sin_port = htons((unsigned short)atoi(port));
    if (bind(fd, (struct sockaddr *)&me, sizeof me) != 0) die("bind");
    int type = 0;
    socklen_t tl = sizeof type;
    getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &tl);
    printf("server socket type %s\n", type == SOCK_DGRAM ? "dgram" : "other");
    for (;;) {
        char buf[64], reply[64], from_text[32];
        struct sockaddr_in from;
        socklen_t fl = sizeof from;
        ssize_t n = recvfrom(fd, buf, sizeof buf - 1, 0, (struct sockaddr *)&from, &fl);
        if (n < 0) die("recvfrom");
        buf[n] = 0;
        inet_ntop(AF_INET, &from.sin_addr, from_text, sizeof from_text);
        printf("got \"%s\" from %s\n", buf, from_text);
        if (strcmp(buf, "done") == 0) break;
        snprintf(reply, sizeof reply, "pong %s", buf + 5);
        if (sendto(fd, reply, strlen(reply), 0, (struct sockaddr *)&from, fl) < 0) die("sendto");
    }
    close(fd);
    return 0;
}

static int client(const char *host, const char *port, int count) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    int rc = getaddrinfo(host, port, &hints, &res);
    if (rc != 0) {
        fprintf(stderr, "getaddrinfo %s: %s\n", host, gai_strerror(rc));
        return 2;
    }
    int fd = socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (fd < 0) die("socket");
    // Connected: only the server's replies are accepted
    if (connect(fd, res->ai_addr, res->ai_addrlen) != 0) die("connect");
    freeaddrinfo(res);
    for (int i = 0; i < count; i++) {
        char ping[32], buf[64];
        snprintf(ping, sizeof ping, "ping %d", i);
        for (int waited = 0;; waited++) {
            if (waited % 50 == 0 && send(fd, ping, strlen(ping), 0) < 0) die("send");
            int pending = 0;
            ioctl(fd, FIONREAD, &pending);
            ssize_t n = recv(fd, buf, sizeof buf - 1, MSG_DONTWAIT);
            if (n < 0 && errno != EAGAIN) die("recv");
            if (n < 0) {
                usleep(1000);
                continue;
            }
            buf[n] = 0;
            if ((ssize_t)pending != n) printf("FIONREAD said %d, got %zd\n", pending, n);
            if (atoi(buf + 5) == i) break;
        }
        printf("%s\n", buf);
    }
    if (send(fd, "done", 4, 0) < 0) die("send");
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) return server(argv[2]);
    if (argc == 5 && strcmp(argv[1], "client") == 0) return client(argv[2], argv[3], atoi(argv[4]));
    return 2;
}
