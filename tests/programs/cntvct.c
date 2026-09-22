// Reads the CPU's counter the way Redis's monotonic clock does, and prints
// what it saw: under the supervisor the virtual clock, in ticks.
#include <stdint.h>
#include <stdio.h>

static inline uint64_t counter(void) {
    uint64_t v;
    __asm__ volatile("mrs %0, cntvct_el0" : "=r"(v));
    return v;
}

static inline uint64_t frequency(void) {
    uint64_t v;
    __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(v));
    return v;
}

int main(void) {
    uint64_t f = frequency();
    uint64_t a = counter();
    volatile uint64_t sink = 0;
    for (uint64_t i = 0; i < 1000000; i++) sink += i;
    uint64_t b = counter();
    // The register the compiler picked is whatever it is; a few more
    // reads through different paths
    uint64_t c = counter() + counter();
    printf("freq=%llu a=%llu b-a=%llu c-2b=%llu\n", (unsigned long long)f, (unsigned long long)a,
           (unsigned long long)(b - a), (unsigned long long)(c - 2 * b));
    return 0;
}
