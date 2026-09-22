// An mmap with an address hint into the stack of a thread that has exited.
// The kernel frees such a stack itself when the thread terminates, so the
// hint names a hole it would grant; the run places the mapping instead.
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>

static void *stack;

static void *run(void *arg) {
    volatile char here = 0;
    stack = (void *)&here;
    return arg;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, run, NULL);
    pthread_join(t, NULL);
    uintptr_t hint = ((uintptr_t)stack - 0x40000) & ~(uintptr_t)0x3FFF;
    void *m = mmap((void *)hint, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    printf("hint=%#lx got=%p %s\n", (unsigned long)hint, m, m == (void *)hint ? "granted" : "placed");
    return 0;
}
