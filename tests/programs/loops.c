// Overhead workload: sieve, matmul and recursion. Prints checksums so the
// rewritten binary can be checked against the original.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned sieve(int n) {
    char *comp = calloc(n + 1, 1);
    unsigned count = 0;
    for (int i = 2; i <= n; i++) {
        if (comp[i]) continue;
        count++;
        for (long j = (long)i * i; j <= n; j += i) comp[j] = 1;
    }
    free(comp);
    return count;
}

static double matmul(int n) {
    double *a = malloc(sizeof(double) * n * n);
    double *b = malloc(sizeof(double) * n * n);
    double *c = calloc(n * n, sizeof(double));
    for (int i = 0; i < n * n; i++) {
        a[i] = (i % 7) * 0.5;
        b[i] = (i % 11) * 0.25;
    }
    for (int i = 0; i < n; i++)
        for (int k = 0; k < n; k++) {
            double aik = a[i * n + k];
            for (int j = 0; j < n; j++) c[i * n + j] += aik * b[k * n + j];
        }
    double sum = 0;
    for (int i = 0; i < n * n; i++) sum += c[i];
    free(a);
    free(b);
    free(c);
    return sum;
}

static unsigned fib(unsigned n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

int main(int argc, char **argv) {
    int scale = argc > 1 ? atoi(argv[1]) : 1;
    unsigned primes = sieve(20000000 * scale);
    double mm = matmul(300 * scale);
    unsigned f = fib(30 + scale);
    printf("primes=%u matmul=%.1f fib=%u\n", primes, mm, f);
    return 0;
}
