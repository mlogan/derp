#include <pthread.h>
#include <stdio.h>

static volatile long counter;

static void *bump(void *arg) {
    for (int i = 0; i < 100000; i++) counter = counter + 1;
    return NULL;
}

int main(void) {
    pthread_t a, b;
    pthread_create(&a, NULL, bump, NULL);
    pthread_create(&b, NULL, bump, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("counter = %ld (expected 200000)\n", counter);
}
