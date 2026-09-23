// A client and a server that survive each other's crashes.
//
//   ping_pong pong <port>
//   ping_pong ping <host> <port> <count>    (0: until the run is stopped)
//
// The server counts its lives in a file in its host directory and says
// which life answered. The client keeps its progress in a file, so a
// restarted client resumes; when it loses the server it reconnects, resends
// the ping it got no answer to, and reports how long the server was out of
// reach on the (virtual) clock.
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static void die(const char *what) {
    perror(what);
    exit(2);
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000LL + ts.tv_nsec / 1000000;
}

// A number kept in a file: read it, add `step`, write it back.
static long bump(const char *path, long step) {
    long value = 0;
    FILE *f = fopen(path, "r");
    if (f) {
        if (fscanf(f, "%ld", &value) != 1) value = 0;
        fclose(f);
    }
    value += step;
    if (!(f = fopen(path, "w"))) die(path);
    fprintf(f, "%ld\n", value);
    fclose(f);
    return value;
}

static ssize_t read_line(int fd, char *line, size_t cap) {
    size_t n = 0;
    while (n + 1 < cap) {
        ssize_t r = read(fd, line + n, 1);
        if (r <= 0) return -1;  // a line cut short by a crash counts for nothing
        if (line[n++] == '\n') break;
    }
    line[n] = 0;
    return (ssize_t)n;
}

static int pong(const char *port) {
    long life = bump("lives", 1);
    printf("server life %ld is up\n", life);
    fflush(stdout);
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in me = {0};
    me.sin_family = AF_INET;
    me.sin_port = htons((unsigned short)atoi(port));
    if (bind(l, (struct sockaddr *)&me, sizeof me) != 0) die("bind");
    if (listen(l, 8) != 0) die("listen");
    for (;;) {
        int fd = accept(l, NULL, NULL);
        if (fd < 0) die("accept");
        int one = 1;
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
        char line[64], reply[96];
        while (read_line(fd, line, sizeof line) > 0) {
            int len = snprintf(reply, sizeof reply, "pong %d life %ld\n", atoi(line + 5), life);
            if (write(fd, reply, (size_t)len) != len) break;
        }
        close(fd);
    }
}

static int dial(const char *host, const char *port) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, port, &hints, &res) != 0) die("getaddrinfo");
    for (;;) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(fd, res->ai_addr, res->ai_addrlen) == 0) {
            int one = 1;
            setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
            freeaddrinfo(res);
            return fd;
        }
        if (errno != ECONNREFUSED) die("connect");
        close(fd);
        usleep(5000);  // the server is down; look again in 5 ms
    }
}

static int ping(const char *host, const char *port, long count) {
    long next = bump("progress", 0);
    long life = bump("client_lives", 1);
    if (life > 1) printf("client life %ld resumes at ping %ld\n", life, next);
    int fd = dial(host, port), reconnects = 0;
    while (count == 0 || next < count) {
        char line[96];
        int len = snprintf(line, sizeof line, "ping %ld\n", next);
        if (write(fd, line, (size_t)len) != len || read_line(fd, line, sizeof line) < 0) {
            long long lost = now_ms();
            close(fd);
            fd = dial(host, port);
            reconnects++;
            printf("server was out of reach for %lld ms\n", now_ms() - lost);
            fflush(stdout);
            continue;  // the same ping again
        }
        fputs(line, stdout);
        fflush(stdout);
        next = bump("progress", 1);
        usleep(5000);
    }
    printf("client life %ld done: %ld pongs, %d reconnects\n", life, count, reconnects);
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "pong") == 0) return pong(argv[2]);
    if (argc == 5 && strcmp(argv[1], "ping") == 0) return ping(argv[2], argv[3], atol(argv[4]));
    return 2;
}
