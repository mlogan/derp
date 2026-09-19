// A single-threaded server multiplexing several clients, with poll or
// with kevent, and an idle timeout per connection.
//
//   poll_server server poll|kevent <port> <clients>
//   poll_server client <host> <port> <id> <messages> [stall]
//
// The server upper-cases each line. A client that stalls for 200 ms is
// dropped by the server's 100 ms idle timeout; both are virtual time, so
// that happens on every seed. The server prints only totals: the order in
// which it served the clients is the schedule's business.
#include <arpa/inet.h>
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define MAX_CONNS 16
#define IDLE_MS 100

static void die(const char *what) {
    perror(what);
    exit(2);
}

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000LL + ts.tv_nsec / 1000000;
}

struct conn {
    int fd, id, messages;
    long long last_active;
    char line[128];
    size_t len;
};

static struct conn conns[MAX_CONNS];
static int nconns, finished, timed_out, total_messages;
static int use_kevent, kq, listener;

static void watch(int fd, int oneshot) {
    if (!use_kevent) return;
    struct kevent ev;
    EV_SET(&ev, fd, EVFILT_READ, EV_ADD | (oneshot ? EV_ONESHOT : EV_CLEAR), 0, 0, NULL);
    if (kevent(kq, &ev, 1, NULL, 0, NULL) != 0) die("kevent add");
}

static void drop(int i, int because_idle) {
    if (because_idle) {
        timed_out++;
        printf("client %d timed out after %d messages\n", conns[i].id, conns[i].messages);
    }
    close(conns[i].fd);
    conns[i] = conns[--nconns];
    finished++;
}

static void accept_all(void) {
    // The listener is edge-triggered under kevent: take everything queued
    for (;;) {
        int fd = accept(listener, NULL, NULL);
        if (fd < 0) {
            if (errno == EAGAIN) return;
            die("accept");
        }
        fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
        struct conn *c = &conns[nconns++];
        memset(c, 0, sizeof *c);
        c->fd = fd;
        c->id = -1;
        c->last_active = now_ms();
        watch(fd, 1);
    }
}

// Returns 0 when the connection is over.
static int serve(int i) {
    struct conn *c = &conns[i];
    for (;;) {
        char ch;
        ssize_t n = read(c->fd, &ch, 1);
        if (n == 0) return 0;
        if (n < 0) {
            if (errno == EAGAIN) break;
            return 0;
        }
        c->last_active = now_ms();
        if (c->len + 1 < sizeof c->line) c->line[c->len++] = ch;
        if (ch != '\n') continue;
        c->line[c->len] = 0;
        sscanf(c->line, "c%d", &c->id);
        for (size_t k = 0; k < c->len; k++) c->line[k] = (char)toupper((unsigned char)c->line[k]);
        if (write(c->fd, c->line, c->len) < 0) return 0;
        c->messages++;
        total_messages++;
        c->len = 0;
    }
    watch(c->fd, 1);  // one-shot registrations are re-armed after each event
    return 1;
}

static int server(const char *mode, const char *port, int clients) {
    use_kevent = strcmp(mode, "kevent") == 0;
    if (use_kevent && (kq = kqueue()) < 0) die("kqueue");
    listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in me = {0};
    me.sin_family = AF_INET;
    me.sin_port = htons((unsigned short)atoi(port));
    if (bind(listener, (struct sockaddr *)&me, sizeof me) != 0) die("bind");
    if (listen(listener, 16) != 0) die("listen");
    fcntl(listener, F_SETFL, fcntl(listener, F_GETFL) | O_NONBLOCK);
    watch(listener, 0);

    while (finished < clients) {
        long long now = now_ms(), wait_ms = 1000;
        for (int i = 0; i < nconns; i++) {
            long long left = conns[i].last_active + IDLE_MS - now;
            if (left < wait_ms) wait_ms = left < 0 ? 0 : left;
        }
        int ready_fds[MAX_CONNS + 1], nready = 0;
        if (use_kevent) {
            struct kevent evs[MAX_CONNS + 1];
            struct timespec ts = {wait_ms / 1000, (wait_ms % 1000) * 1000000};
            int n = kevent(kq, NULL, 0, evs, MAX_CONNS + 1, &ts);
            if (n < 0) die("kevent");
            for (int k = 0; k < n; k++) ready_fds[nready++] = (int)evs[k].ident;
        } else {
            struct pollfd pfds[MAX_CONNS + 1] = {{listener, POLLIN, 0}};
            for (int i = 0; i < nconns; i++) pfds[i + 1] = (struct pollfd){conns[i].fd, POLLIN, 0};
            int n = poll(pfds, (nfds_t)nconns + 1, (int)wait_ms);
            if (n < 0) die("poll");
            for (int i = 0; i <= nconns; i++)
                if (pfds[i].revents) ready_fds[nready++] = pfds[i].fd;
        }
        for (int k = 0; k < nready; k++) {
            if (ready_fds[k] == listener) {
                accept_all();
                continue;
            }
            for (int i = 0; i < nconns; i++)
                if (conns[i].fd == ready_fds[k]) {
                    if (!serve(i)) drop(i, 0);
                    break;
                }
        }
        now = now_ms();
        for (int i = nconns - 1; i >= 0; i--)
            if (now - conns[i].last_active >= IDLE_MS) drop(i, 1);
    }
    printf("%s server: %d clients, %d messages, %d timed out\n", mode, finished, total_messages,
           timed_out);
    return 0;
}

static int client(const char *host, const char *port, int id, int messages, int stall) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, port, &hints, &res) != 0) return 2;
    int fd = -1;
    for (int attempt = 0;; attempt++) {
        fd = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(fd, res->ai_addr, res->ai_addrlen) == 0) break;
        if (errno != ECONNREFUSED || attempt > 100000) die("connect");
        close(fd);
        usleep(1000);
    }
    freeaddrinfo(res);
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
    for (int m = 0; m < messages; m++) {
        if (stall && m == 1) usleep(200 * 1000);
        char line[128];
        int len = snprintf(line, sizeof line, "c%d message %d\n", id, m);
        ssize_t w = write(fd, line, (size_t)len);
        size_t got = 0;
        while (w > 0 && got < (size_t)len) {
            ssize_t n = read(fd, line + got, (size_t)len - got);
            if (n <= 0) break;
            got += (size_t)n;
        }
        if (w < 0 || got < (size_t)len) {
            printf("client %d dropped by the server before message %d\n", id, m);
            close(fd);
            return 0;
        }
        line[got] = 0;
        fputs(line, stdout);
    }
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 5 && strcmp(argv[1], "server") == 0) return server(argv[2], argv[3], atoi(argv[4]));
    if (argc >= 6 && strcmp(argv[1], "client") == 0)
        return client(argv[2], argv[3], atoi(argv[4]), atoi(argv[5]), argc > 6);
    return 2;
}
