// Echo over stream sockets. The server accepts N connections one after
// the other and answers every line with a per-connection counter; a line
// starting with "blob" announces a block of bytes, which it sums instead
// of echoing (more than a socket buffer, so the sender must block).
//
//   tcp_echo server <port>|--unix <path> <connections>
//   tcp_echo client <host>|--unix <path> <port> <connections>
//
// <host> is a dotted address or a virtual host's name.
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define BLOB (200 * 1024)

static void die(const char *what) {
    perror(what);
    exit(2);
}

static int resolve(const char *host, struct in_addr *out) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, NULL, &hints, &res) != 0) return -1;
    *out = ((struct sockaddr_in *)res->ai_addr)->sin_addr;
    freeaddrinfo(res);
    return 0;
}

static socklen_t make_addr(const char *host, const char *port, const char *path,
                           struct sockaddr_storage *ss) {
    memset(ss, 0, sizeof *ss);
    if (path) {
        struct sockaddr_un *un = (struct sockaddr_un *)ss;
        un->sun_family = AF_UNIX;
        strncpy(un->sun_path, path, sizeof un->sun_path - 1);
        return sizeof *un;
    }
    struct sockaddr_in *in = (struct sockaddr_in *)ss;
    in->sin_family = AF_INET;
    in->sin_port = htons((unsigned short)atoi(port));
    if (host && resolve(host, &in->sin_addr) != 0) {
        fprintf(stderr, "cannot resolve %s\n", host);
        exit(2);
    }
    return sizeof *in;
}

static void read_exact(int fd, char *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(fd, buf + got, n - got);
        if (r <= 0) die("read");
        got += (size_t)r;
    }
}

static ssize_t read_line(int fd, char *line, size_t cap) {
    size_t n = 0;
    while (n + 1 < cap) {
        ssize_t r = read(fd, line + n, 1);
        if (r < 0) die("read");
        if (r == 0) break;
        if (line[n++] == '\n') break;
    }
    line[n] = 0;
    return (ssize_t)n;
}

static int server(const char *port, const char *path, int connections) {
    struct sockaddr_storage ss;
    socklen_t len = make_addr(NULL, port, path, &ss);
    int l = socket(path ? AF_UNIX : AF_INET, SOCK_STREAM, 0);
    if (l < 0) die("socket");
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (struct sockaddr *)&ss, len) != 0) die("bind");
    if (listen(l, 16) != 0) die("listen");
    static char blob[BLOB];
    for (int c = 0; c < connections; c++) {
        int fd = accept(l, NULL, NULL);
        if (fd < 0) die("accept");
        char line[256], reply[320];
        int counter = 0;
        while (read_line(fd, line, sizeof line) > 0) {
            if (strncmp(line, "blob", 4) == 0) {
                read_exact(fd, blob, BLOB);
                unsigned long sum = 0;
                for (int i = 0; i < BLOB; i++) sum = sum * 31 + (unsigned char)blob[i];
                snprintf(reply, sizeof reply, "conn %d blob sum=%lu\n", c, sum);
            } else {
                snprintf(reply, sizeof reply, "conn %d #%d: %s", c, counter++, line);
            }
            if (write(fd, reply, strlen(reply)) < 0) die("write");
        }
        close(fd);
    }
    close(l);
    printf("server done after %d connections\n", connections);
    return 0;
}

static int client(const char *host, const char *port, const char *path, int connections) {
    struct sockaddr_storage ss;
    socklen_t len = make_addr(host, port, path, &ss);
    int fds[64];
    if (connections > 64) connections = 64;
    for (int c = 0; c < connections; c++) {
        // The server may not be listening yet
        for (int attempt = 0;; attempt++) {
            fds[c] = socket(path ? AF_UNIX : AF_INET, SOCK_STREAM, 0);
            if (fds[c] < 0) die("socket");
            if (connect(fds[c], (struct sockaddr *)&ss, len) == 0) break;
            if (errno != ECONNREFUSED || attempt > 100000) die("connect");
            close(fds[c]);
            usleep(1000);
        }
    }
    static char blob[BLOB];
    for (int i = 0; i < BLOB; i++) blob[i] = (char)(i * 7 + 1);
    char line[320];
    for (int c = 0; c < connections; c++) {
        for (int m = 0; m < 3; m++) {
            snprintf(line, sizeof line, "hello %d from connection %d\n", m, c);
            if (write(fds[c], line, strlen(line)) < 0) die("write");
            read_line(fds[c], line, sizeof line);
            fputs(line, stdout);
        }
        if (write(fds[c], "blob\n", 5) < 0 || write(fds[c], blob, BLOB) != BLOB) die("write blob");
        read_line(fds[c], line, sizeof line);
        fputs(line, stdout);
        close(fds[c]);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 4) return 2;
    int is_unix = strcmp(argv[2], "--unix") == 0;
    if (strcmp(argv[1], "server") == 0) {
        if (is_unix) return server(NULL, argv[3], atoi(argv[4]));
        return server(argv[2], NULL, atoi(argv[3]));
    }
    if (is_unix) return client(NULL, NULL, argv[3], atoi(argv[4]));
    return client(argv[2], argv[3], NULL, atoi(argv[4]));
}
