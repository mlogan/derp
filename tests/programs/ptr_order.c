// How pairs of heap blocks compare, and whether a freed block comes
// straight back. A program may depend on any of it without meaning to.
//   ptr_order          print the comparisons
//   ptr_order crash    abort if the first block is below the second
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *order(void *a, void *b) {
    return (uintptr_t)a < (uintptr_t)b ? "a<b" : "a>b";
}

int main(int argc, char **argv) {
    char *a = malloc(40), *b = malloc(40);
    if (argc > 1 && strcmp(argv[1], "crash") == 0) {
        if (a < b) {
            fprintf(stderr, "a < b: the case nobody tested\n");
            abort();
        }
        puts("survived");
        return 0;
    }
    printf("same class: %s\n", order(a, b));

    char *small = malloc(24), *big = malloc(900);
    printf("small then big: %s\n", order(small, big));

    char *page = malloc(5000), *pages = malloc(300000);
    printf("4K class then a run: %s\n", order(page, pages));

    char *huge1 = malloc(3 << 20), *huge2 = malloc(3 << 20);
    printf("two 3 MB blocks: %s\n", order(huge1, huge2));

    // The blocks are usable and distinct
    memset(a, 1, 40); memset(b, 2, 40); memset(small, 3, 24); memset(big, 4, 900);
    memset(page, 5, 5000); memset(pages, 6, 300000);
    memset(huge1, 7, 3 << 20); memset(huge2, 8, 3 << 20);
    if (a[39] != 1 || b[0] != 2 || big[899] != 4 || pages[299999] != 6 || huge1[(3 << 20) - 1] != 7) {
        puts("blocks overlap");
        return 1;
    }

    int straight_back = 0;
    for (int i = 0; i < 32; i++) {
        char *p = malloc(64);
        // Taken before the free: a compiler may assume a freed pointer
        // equals nothing
        volatile uintptr_t was = (uintptr_t)p;
        free(p);
        char *q = malloc(64);
        straight_back += was == (uintptr_t)q;
        free(q);
    }
    printf("freed block came straight back: %s\n",
           straight_back == 32 ? "always" : straight_back == 0 ? "never" : "sometimes");
    free(a); free(b); free(small); free(big); free(page); free(pages); free(huge1); free(huge2);
    return 0;
}
