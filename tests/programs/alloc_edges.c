// Corners of the allocator that only show under the supervisor.
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static pthread_key_t key;
static void *volatile escaped;

static void *with_a_key(void *arg) {
    (void)arg;
    // Freed by the key's destructor as the thread exits
    pthread_setspecific(key, malloc(100000));
    return NULL;
}

static void *on_a_small_stack(void *arg) {
    (void)arg;
    // 28 KB of stack, 24 KB of it in use when malloc is called
    volatile char *pad = __builtin_alloca(24000);
    for (int i = 0; i < 24000; i++) pad[i] = 1;
    char *p = malloc(40);
    memset(p, pad[7], 40);
    free(p);
    return p;
}

int main(void) {
    // The pointer escapes, or the compiler removes the call and decides for
    // itself that it succeeded
    volatile size_t absurd = SIZE_MAX - 100;
    escaped = malloc(absurd);
    printf("absurd size: %s\n", escaped ? "a pointer" : "null");

    pthread_key_create(&key, free);
    for (int i = 0; i < 50; i++) {
        pthread_t t;
        pthread_create(&t, NULL, with_a_key, NULL);
        pthread_join(t, NULL);
    }
    puts("key destructors ran");

    pthread_attr_t small;
    pthread_attr_init(&small);
    pthread_attr_setstacksize(&small, PTHREAD_STACK_MIN);
    pthread_t t;
    void *got;
    pthread_create(&t, &small, on_a_small_stack, NULL);
    pthread_join(t, &got);
    printf("malloc on a small stack: %s\n", got ? "ok" : "null");

    // Many scattered small blocks, then big ones
    for (int i = 0; i < 2000; i++) memset(malloc(40000), 1, 64);
    for (size_t mb = 32; mb <= 2048; mb *= 8) {
        char *big = malloc(mb << 20);
        printf("%zu MB after 80 MB of small blocks: %s\n", mb, big ? "ok" : "null");
        if (big) big[(mb << 20) - 1] = 1;
        free(big);
    }
    return 0;
}
