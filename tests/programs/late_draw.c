// A failure decided by one random draw in the middle of the run, noticed
// at the end. Nothing about the schedule matters.
//   late_draw heap      three pairs of blocks all compare a < b (1 in 8)
//   late_draw entropy   arc4random_uniform(8) gives 0
// The moment of the draw is printed on the guest's clock.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static void busy_ms(int ms) {
    for (int i = 0; i < ms; i++) usleep(1000);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    busy_ms(60);
    long long from = now_ns();
    int doomed;
    if (strcmp(argv[1], "heap") == 0) {
        doomed = 1;
        for (int i = 0; i < 3; i++) {
            char *a = malloc(48), *b = malloc(48);
            doomed &= (uintptr_t)a < (uintptr_t)b;
        }
    } else {
        doomed = arc4random_uniform(8) == 0;
    }
    printf("draw: %lld %lld\n", from, now_ns());
    busy_ms(240);
    printf("doomed %d at %lld\n", doomed, now_ns());
    fflush(stdout);
    if (doomed) abort();
    return 0;
}
