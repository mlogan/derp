// Whether blocks of 100 MB and of 1000 MB can be had: the heap's size is
// the run's to set.
#include <stdio.h>
#include <stdlib.h>

static void *volatile escaped;

int main(void) {
    for (size_t mb = 100; mb <= 1000; mb *= 10) {
        escaped = malloc(mb << 20);
        printf("%zu MB: %s\n", mb, escaped ? "ok" : "null");
        free(escaped);
    }
    return 0;
}
