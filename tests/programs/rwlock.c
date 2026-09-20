// Two writers keep a pair of counters equal under a pthread rwlock; four
// readers must never see them apart. A trylock prober never blocks.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define ROUNDS 20000

static pthread_rwlock_t lock = PTHREAD_RWLOCK_INITIALIZER;
static pthread_mutex_t probe = PTHREAD_MUTEX_INITIALIZER;
static volatile long a, b;
static long torn, probes_won;

static void *writer(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_rwlock_wrlock(&lock);
        a++;
        for (volatile int spin = 0; spin < 3; spin++) {}
        b++;
        pthread_rwlock_unlock(&lock);
    }
    return NULL;
}

static void *reader(void *arg) {
    long *reads = arg;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_rwlock_rdlock(&lock);
        if (a != b) __sync_fetch_and_add(&torn, 1);
        (*reads)++;
        pthread_rwlock_unlock(&lock);
        if (pthread_mutex_trylock(&probe) == 0) {
            probes_won++;
            pthread_mutex_unlock(&probe);
        }
    }
    return NULL;
}

int main(void) {
    pthread_t w[2], r[4];
    long reads[4] = {0};
    for (int i = 0; i < 2; i++) pthread_create(&w[i], NULL, writer, NULL);
    for (int i = 0; i < 4; i++) pthread_create(&r[i], NULL, reader, &reads[i]);
    for (int i = 0; i < 2; i++) pthread_join(w[i], NULL);
    long total = 0;
    for (int i = 0; i < 4; i++) {
        pthread_join(r[i], NULL);
        total += reads[i];
    }
    printf("a=%ld b=%ld reads=%ld torn=%ld probes=%s\n", a, b, total, torn,
           probes_won > 0 ? "some" : "none");
    return torn != 0 || a != 2 * ROUNDS;
}
