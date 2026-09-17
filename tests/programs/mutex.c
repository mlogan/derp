// Same increments as race.c, under a pthread mutex: always 2N.
#include <pthread.h>
#include <stdio.h>

#define N 200000

static long counter;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void *bump(void *arg) {
    (void)arg;
    for (int i = 0; i < N; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

int main(void) {
    pthread_t a, b;
    pthread_create(&a, NULL, bump, NULL);
    pthread_create(&b, NULL, bump, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("total=%ld expected=%d\n", counter, 2 * N);
    return 0;
}
