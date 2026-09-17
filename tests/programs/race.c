// Two threads each add N to a shared counter without synchronization.
// "global" shares a global; "stack" shares a local of main by address.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define N 200000

static volatile long global_counter;

static void *bump(void *arg) {
    volatile long *p = arg;
    for (int i = 0; i < N; i++) *p = *p + 1;
    return NULL;
}

int main(int argc, char **argv) {
    volatile long local_counter = 0;
    volatile long *target = &global_counter;
    if (argc > 1 && strcmp(argv[1], "stack") == 0) target = &local_counter;
    pthread_t a, b;
    pthread_create(&a, NULL, bump, (void *)target);
    pthread_create(&b, NULL, bump, (void *)target);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("total=%ld expected=%d\n", *target, 2 * N);
    return 0;
}
