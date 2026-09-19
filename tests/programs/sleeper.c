// sleeper N US: sleep N times for US microseconds with some work between,
// then say when (on the process's clock) it finished. Touches no sockets.
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 100;
    long us = argc > 2 ? atol(argv[2]) : 1000;
    volatile unsigned long x = 0;
    for (long i = 0; i < n; i++) {
        usleep((useconds_t)us);
        for (unsigned long j = 0; j < 20000; j++) x += j * (unsigned long)i;
    }
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    printf("done at %lld ms\n", ts.tv_sec * 1000LL + ts.tv_nsec / 1000000);
    return 0;
}
