// What a guest's environment looks like, and where its stack is.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern char **environ;

static int by_name(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static const char *value(const char *name) {
    const char *v = getenv(name);
    return v ? v : "(unset)";
}

int main(void) {
    int local = 0;
    int n = 0;
    while (environ[n]) n++;
    char **names = malloc(sizeof(char *) * (size_t)n);
    for (int i = 0; i < n; i++) names[i] = strndup(environ[i], strcspn(environ[i], "="));
    qsort(names, (size_t)n, sizeof *names, by_name);
    for (int i = 0; i < n; i++)
        if (strncmp(names[i], "REWRITE_", 8) != 0 && strncmp(names[i], "DYLD_", 5) != 0)
            printf("%s ", names[i]);
    printf("\n");
    printf("PATH=%s LANG=%s TZ=%s USER=%s\n", value("PATH"), value("LANG"), value("TZ"), value("USER"));
    printf("AMBIENT=%s PASSED=%s FROM_FILE=%s MINE=%s\n", value("AMBIENT"), value("PASSED"),
           value("FROM_FILE"), value("MINE"));
    printf("stack=%p\n", (void *)&local);
    for (int i = 0; i < 5; i++) usleep(1000);
    return 0;
}
