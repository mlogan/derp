// The same server on two virtual hosts and the same port, and a client on
// a third host that reaches both by name and by address.
//
//   two_hosts server <port>
//   two_hosts client <port> <host>...
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <mach-o/dyld.h>
#include <net/if.h>
#include <netdb.h>
#include <netinet/in.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void die(const char *what) {
    perror(what);
    exit(2);
}

static const char *my_name(void) {
    static char name[128];
    if (gethostname(name, sizeof name) != 0) die("gethostname");
    return name;
}

static int child(void) {
    printf("child of %d runs on %s\n", getppid(), my_name());
    return 0;
}

static int server(const char *port) {
    // A spawned child lives on its parent's host
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char *args[] = {self, "child", NULL};
    pid_t pid;
    fflush(stdout);
    if (posix_spawn(&pid, self, NULL, NULL, args, environ) != 0) die("posix_spawn");
    waitpid(pid, NULL, 0);

    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in me = {0};
    me.sin_family = AF_INET;
    me.sin_port = htons((unsigned short)atoi(port));
    if (bind(l, (struct sockaddr *)&me, sizeof me) != 0) die("bind");
    if (listen(l, 4) != 0) die("listen");
    for (int i = 0; i < 2; i++) {
        struct sockaddr_in peer, local;
        socklen_t pl = sizeof peer, ll = sizeof local;
        int fd = accept(l, (struct sockaddr *)&peer, &pl);
        if (fd < 0) die("accept");
        getsockname(fd, (struct sockaddr *)&local, &ll);
        char a[32], b[32], reply[160];
        inet_ntop(AF_INET, &peer.sin_addr, a, sizeof a);
        inet_ntop(AF_INET, &local.sin_addr, b, sizeof b);
        snprintf(reply, sizeof reply, "%s at %s:%d greets %s\n", my_name(), b,
                 ntohs(local.sin_port), a);
        if (write(fd, reply, strlen(reply)) < 0) die("write");
        close(fd);
    }
    close(l);
    return 0;
}

static void talk(const struct sockaddr *addr, socklen_t len, const char *how) {
    for (int attempt = 0;; attempt++) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(fd, addr, len) == 0) {
            char reply[160] = {0};
            size_t got = 0;
            ssize_t n;
            while ((n = read(fd, reply + got, sizeof reply - 1 - got)) > 0) got += (size_t)n;
            printf("%s: %s", how, reply);
            close(fd);
            return;
        }
        if (errno != ECONNREFUSED || attempt > 100000) die("connect");
        close(fd);
        usleep(1000);
    }
}

static int client(const char *port, int nhosts, char **hosts) {
    printf("client on %s\n", my_name());
    struct ifaddrs *ifs = NULL;
    if (getifaddrs(&ifs) != 0) die("getifaddrs");
    for (struct ifaddrs *i = ifs; i; i = i->ifa_next) {
        char a[32];
        inet_ntop(AF_INET, &((struct sockaddr_in *)i->ifa_addr)->sin_addr, a, sizeof a);
        printf("interface %s %s%s\n", i->ifa_name, a, i->ifa_flags & IFF_LOOPBACK ? " loopback" : "");
    }
    freeifaddrs(ifs);

    for (int h = 0; h < nhosts; h++) {
        struct addrinfo hints = {0}, *res = NULL;
        hints.ai_family = AF_INET;
        hints.ai_socktype = SOCK_STREAM;
        int rc = getaddrinfo(hosts[h], port, &hints, &res);
        if (rc != 0) {
            fprintf(stderr, "getaddrinfo %s: %s\n", hosts[h], gai_strerror(rc));
            return 2;
        }
        char how[96], a[32];
        struct sockaddr_in in = *(struct sockaddr_in *)res->ai_addr;
        inet_ntop(AF_INET, &in.sin_addr, a, sizeof a);
        snprintf(how, sizeof how, "by name %s", hosts[h]);
        talk(res->ai_addr, res->ai_addrlen, how);
        freeaddrinfo(res);
        snprintf(how, sizeof how, "by address %s", a);
        talk((struct sockaddr *)&in, sizeof in, how);

        // Another host's address is not ours to bind
        int b = socket(AF_INET, SOCK_STREAM, 0);
        in.sin_port = htons(9999);
        if (bind(b, (struct sockaddr *)&in, sizeof in) != 0)
            printf("bind to %s: %s\n", a, errno == EADDRNOTAVAIL ? "not available" : strerror(errno));
        close(b);
    }

    // Nothing listens on this host, and loopback never leaves it
    struct sockaddr_in lo = {0};
    lo.sin_family = AF_INET;
    lo.sin_port = htons((unsigned short)atoi(port));
    inet_pton(AF_INET, "127.0.0.1", &lo.sin_addr);
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(fd, (struct sockaddr *)&lo, sizeof lo) != 0)
        printf("loopback: %s\n", errno == ECONNREFUSED ? "refused" : strerror(errno));
    else
        printf("loopback: connected?!\n");
    close(fd);

    // An address in the subnet that no host owns
    inet_pton(AF_INET, "10.0.0.200", &lo.sin_addr);
    fd = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(fd, (struct sockaddr *)&lo, sizeof lo) != 0)
        printf("10.0.0.200: %s\n", errno == EHOSTUNREACH ? "unreachable" : strerror(errno));
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "child") == 0) return child();
    if (argc == 3 && strcmp(argv[1], "server") == 0) return server(argv[2]);
    if (argc >= 4 && strcmp(argv[1], "client") == 0) return client(argv[2], argc - 3, argv + 3);
    return 2;
}
