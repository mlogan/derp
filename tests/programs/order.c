// An ordering bug: the reader assumes the initializer has run. One ordering
// constraint (the reader before the initializer's store) shows it: PCT
// depth 1. Prints whether the reader saw the value uninitialized.
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>

static _Atomic int config;
static int saw_uninitialized;
static volatile uint64_t sink;

static void work(int n) {
    for (int i = 0; i < n; i++) sink = sink * 31 + (uint64_t)i;
}

static void *initializer(void *arg) {
    (void)arg;
    work(20000);
    atomic_store(&config, 42);
    return NULL;
}

static void *reader(void *arg) {
    (void)arg;
    work(20000);
    saw_uninitialized = atomic_load(&config) != 42;
    return NULL;
}

int main(void) {
    pthread_t a, b;
    pthread_create(&a, NULL, initializer, NULL);
    pthread_create(&b, NULL, reader, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("saw_uninitialized=%d\n", saw_uninitialized);
    return 0;
}
