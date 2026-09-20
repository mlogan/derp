// A bug whose damage is done long before it is noticed. Two workers warm
// up, then each does a few unsynchronised read-modify-writes of a shared
// counter (the window, whose virtual times are printed), then both work on
// for much longer. Only at the very end does main check the counter and
// abort if an update was lost.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 3
// Calls between the read and the write: the wider, the likelier the race
#define WINDOW 40
static volatile long counter;
static volatile unsigned long sink;

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

// Calls are switch points even with only branches and calls hooked
__attribute__((noinline)) static void think(int n) {
    for (int i = 0; i < n; i++) sink += (unsigned long)i * 31;
}

static void busy_ms(int ms) {
    for (int i = 0; i < ms; i++) {
        usleep(1000);
        for (int k = 0; k < 20; k++) think(50);
    }
}

static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t both_here = PTHREAD_COND_INITIALIZER;
static int arrived;

static void *worker(void *arg) {
    long id = (long)arg;
    busy_ms(40);
    // Into the window together, or there is nothing to race with
    pthread_mutex_lock(&gate);
    arrived++;
    pthread_cond_broadcast(&both_here);
    while (arrived < 2) pthread_cond_wait(&both_here, &gate);
    pthread_mutex_unlock(&gate);
    long long from = now_ns();
    for (int r = 0; r < ROUNDS; r++) {
        long seen = counter;
        for (int k = 0; k < WINDOW; k++) think(10);
        counter = seen + 1;
    }
    printf("worker %ld window: %lld %lld\n", id, from, now_ns());
    busy_ms(300);
    return NULL;
}

int main(void) {
    pthread_t t[2];
    for (long i = 0; i < 2; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    printf("counter %ld of %d at %lld\n", counter, 2 * ROUNDS, now_ns());
    fflush(stdout);
    if (counter != 2 * ROUNDS) abort();
    return 0;
}
