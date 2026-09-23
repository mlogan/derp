// Reads the file its argument names and says whether it could.
#include <errno.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    FILE *f = fopen(argv[1], "r");
    if (f == NULL) {
        printf("%s\n", errno == EACCES ? "refused" : strerror(errno));
        return 0;
    }
    char line[256] = {0};
    fgets(line, sizeof line, f);
    fclose(f);
    printf("read %s", line);
    return 0;
}
