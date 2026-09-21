// One unsynchronised read-modify-write among decoys. Main aborts if an
// update was lost. Built with -g: the test wants source lines back.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define N 3000
static volatile long racy, guarded;
static volatile long table[64];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void *worker(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < N; i++) {
        pthread_mutex_lock(&lock);
        guarded = guarded + 1;  // DECOY: shared, but under the lock
        pthread_mutex_unlock(&lock);
        table[(id * 32 + i) % 64] += i;  // DECOY: shared array, own half
        racy = racy + 1;  // RACE: nothing protects this
    }
    return NULL;
}

int main(void) {
    pthread_t t[2];
    for (long i = 0; i < 2; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    printf("racy=%ld guarded=%ld of %d\n", racy, guarded, 2 * N);
    fflush(stdout);
    if (racy != 2 * N) abort();
    return guarded != 2 * N;
}
