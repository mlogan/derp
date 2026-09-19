// Timed waits on the virtual clock: sleeps wake in deadline order, timeouts
// take at least as long as asked, and poll/select see virtual sockets.
// Prints only what must hold on every seed.
#include <arpa/inet.h>
#include <dispatch/dispatch.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000LL + ts.tv_nsec / 1000000;
}

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int order[3], woke;

static void *sleeper(void *arg) {
    int ms = (int)(long)arg;
    usleep((useconds_t)ms * 1000);
    pthread_mutex_lock(&lock);
    order[woke++] = ms;
    pthread_mutex_unlock(&lock);
    return NULL;
}

static int sender_fd;
static void *late_sender(void *arg) {
    usleep((useconds_t)(long)arg * 1000);
    if (write(sender_fd, "x", 1) != 1) perror("write");
    return NULL;
}

static void check(const char *what, int ok) { printf("%s: %s\n", what, ok ? "yes" : "NO"); }

int main(void) {
    pthread_t t[3];
    long sleeps[3] = {30, 10, 20};
    for (int i = 0; i < 3; i++) pthread_create(&t[i], NULL, sleeper, (void *)sleeps[i]);
    for (int i = 0; i < 3; i++) pthread_join(t[i], NULL);
    printf("sleepers woke in order %d %d %d\n", order[0], order[1], order[2]);

    long long start = now_ms();
    struct timespec rq = {0, 5 * 1000 * 1000};
    nanosleep(&rq, NULL);
    check("nanosleep took at least 5 ms", now_ms() - start >= 5);

    pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
    struct timeval tv;
    gettimeofday(&tv, NULL);
    struct timespec until = {tv.tv_sec, tv.tv_usec * 1000 + 50 * 1000 * 1000};
    until.tv_sec += until.tv_nsec / 1000000000;
    until.tv_nsec %= 1000000000;
    start = now_ms();
    pthread_mutex_lock(&lock);
    int rc = pthread_cond_timedwait(&cond, &lock, &until);
    pthread_mutex_unlock(&lock);
    check("cond_timedwait timed out", rc == ETIMEDOUT);
    check("after at least 49 ms", now_ms() - start >= 49);

    // libdispatch computes these deadlines itself, through the virtual clock
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    start = now_ms();
    long late = dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC));
    check("dispatch semaphore timed out after 200 ms", late != 0 && now_ms() - start >= 200 && now_ms() - start < 260);
    start = now_ms();
    late = dispatch_semaphore_wait(sem, dispatch_walltime(NULL, 300 * NSEC_PER_MSEC));
    check("and after 300 ms of wall time", late != 0 && now_ms() - start >= 300 && now_ms() - start < 360);

    start = now_ms();
    check("poll with no descriptors returns 0", poll(NULL, 0, 25) == 0);
    check("after at least 25 ms", now_ms() - start >= 25);

    // A connection to ourselves
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(4000);
    inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
    if (bind(l, (struct sockaddr *)&a, sizeof a) != 0 || listen(l, 1) != 0) perror("listen");
    struct pollfd lp = {l, POLLIN, 0};
    check("listener not readable before connect", poll(&lp, 1, 0) == 0);
    sender_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(sender_fd, (struct sockaddr *)&a, sizeof a) != 0) perror("connect");
    check("listener readable after connect", poll(&lp, 1, 1000) == 1 && (lp.revents & POLLIN));
    int fd = accept(l, NULL, NULL);

    struct timeval rcv = {0, 40 * 1000};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &rcv, sizeof rcv);
    char c;
    start = now_ms();
    ssize_t n = recv(fd, &c, 1, 0);
    check("recv timed out with EAGAIN", n < 0 && errno == EAGAIN);
    check("after at least 40 ms", now_ms() - start >= 40);

    struct pollfd p = {fd, POLLIN | POLLOUT, 0};
    check("idle socket is writable, not readable", poll(&p, 1, 15) == 1 && p.revents == POLLOUT);
    p.events = POLLIN;
    start = now_ms();
    check("poll for input times out", poll(&p, 1, 15) == 0);
    check("after at least 15 ms", now_ms() - start >= 15);

    pthread_t s;
    pthread_create(&s, NULL, late_sender, (void *)5L);
    start = now_ms();
    check("poll wakes for late data", poll(&p, 1, 10000) == 1 && (p.revents & POLLIN));
    check("well before its timeout", now_ms() - start < 5000);
    pthread_join(s, NULL);
    check("the byte arrives", recv(fd, &c, 1, 0) == 1 && c == 'x');

    pthread_create(&s, NULL, late_sender, (void *)5L);
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(fd, &rs);
    struct timeval st = {10, 0};
    check("select wakes for late data", select(fd + 1, &rs, NULL, NULL, &st) == 1 && FD_ISSET(fd, &rs));
    pthread_join(s, NULL);

    close(sender_fd);
    p.events = POLLIN;
    check("close is readable", poll(&p, 1, 1000) == 1 && (p.revents & POLLIN));
    check("and reads the byte, then end of stream", recv(fd, &c, 1, 0) == 1 && recv(fd, &c, 1, 0) == 0);
    return 0;
}
