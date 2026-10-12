// A holder renews a 5ms lease and writes only while it holds it; a
// contender takes the lease over once it has expired; a bystander computes.
// The holder checks its lease, does a little work, then writes: if it stalls
// in between for longer than the lease, the contender holds the lease when
// the write lands (the bug a GC pause causes against a lease without
// fencing). Prints how many writes landed without the lease.
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 3000
#define LEASE_NS 5000000ULL

enum { HOLDER = 1, CONTENDER = 2 };

static _Atomic int owner = HOLDER;
static _Atomic uint64_t expiry;
static atomic_bool done;
static int unfenced_writes;
static volatile uint64_t sink;

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static void work(int n) {
    for (int i = 0; i < n; i++) sink = sink * 31 + (uint64_t)i;
}

static void *holder(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        uint64_t t = now_ns();
        if (atomic_load(&owner) != HOLDER) {
            // Lost it: take it back once the contender's lease is over
            if (t < atomic_load(&expiry)) {
                work(200);
                continue;
            }
            atomic_store(&owner, HOLDER);
        }
        atomic_store(&expiry, t + LEASE_NS);
        work(2000);
        if (atomic_load(&owner) != HOLDER) unfenced_writes++;
    }
    atomic_store(&done, true);
    return NULL;
}

static void *contender(void *arg) {
    (void)arg;
    while (!atomic_load(&done)) {
        usleep(1000);
        uint64_t t = now_ns();
        if (atomic_load(&owner) == HOLDER && t > atomic_load(&expiry)) {
            atomic_store(&owner, CONTENDER);
            atomic_store(&expiry, t + LEASE_NS);
        }
    }
    return NULL;
}

static void *bystander(void *arg) {
    (void)arg;
    while (!atomic_load(&done)) work(1000);
    return NULL;
}

int main(void) {
    atomic_store(&expiry, now_ns() + LEASE_NS);
    pthread_t h, c, b;
    pthread_create(&h, NULL, holder, NULL);
    pthread_create(&c, NULL, contender, NULL);
    pthread_create(&b, NULL, bystander, NULL);
    pthread_join(h, NULL);
    pthread_join(c, NULL);
    pthread_join(b, NULL);
    printf("unfenced_writes=%d\n", unfenced_writes);
    return 0;
}
