// An atomicity violation: the user checks a pointer, works, and uses it;
// the closer clears it. The bug needs two ordering constraints (the check
// before the clear, the clear before the use): PCT depth 2. Prints whether
// the use found the pointer gone.
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>

static int resource = 7;
static int *_Atomic ptr = &resource;
static int used_after_close;
static volatile uint64_t sink;

static void work(int n) {
    for (int i = 0; i < n; i++) sink = sink * 31 + (uint64_t)i;
}

static void *user(void *arg) {
    (void)arg;
    work(10000);
    if (atomic_load(&ptr) != NULL) {
        work(10000);
        used_after_close = atomic_load(&ptr) == NULL;
    }
    return NULL;
}

static void *closer(void *arg) {
    (void)arg;
    work(10000);
    atomic_store(&ptr, NULL);
    return NULL;
}

int main(void) {
    pthread_t a, b;
    pthread_create(&a, NULL, user, NULL);
    pthread_create(&b, NULL, closer, NULL);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    printf("used_after_close=%d\n", used_after_close);
    return 0;
}
