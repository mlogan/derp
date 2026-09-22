// Where threads' stacks and anonymous mappings land: prints each new
// thread's pthread_self (the top of its stack) and an mmap of its own.
#include <pthread.h>
#include <stdio.h>
#include <sys/mman.h>

static void *run(void *arg) {
    void *m = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    printf("thread %ld self=%p map=%p\n", (long)arg, (void *)pthread_self(), m);
    munmap(m, 1 << 20);
    return NULL;
}

int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, run, (void *)i);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    void *m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANON, -1, 0);
    printf("main map=%p\n", m);
    return 0;
}
