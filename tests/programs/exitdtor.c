// Threads whose pthread key destructor works and re-arms itself for more
// rounds, as jemalloc's thread cache cleanup does. The key is younger than
// the supervisor's, so its destructor runs after the supervisor's in every
// round; the main thread keeps busy meanwhile.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static pthread_key_t key;
static volatile unsigned long sink;

static void dtor(void *v) {
    long round = (long)v;
    for (unsigned long i = 0; i < 200000; i++) sink += i ^ (unsigned long)round;
    if (round < 4) pthread_setspecific(key, (void *)(round + 1));
}

static void *run(void *arg) {
    pthread_setspecific(key, (void *)1);
    return arg;
}

int main(void) {
    pthread_key_create(&key, dtor);
    pthread_t t[8];
    for (long i = 0; i < 8; i++) {
        pthread_create(&t[i], NULL, run, NULL);
        for (unsigned long j = 0; j < 400000; j++) sink += j * (unsigned long)i;
    }
    for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
    printf("sink=%lu\n", sink);
    return 0;
}
