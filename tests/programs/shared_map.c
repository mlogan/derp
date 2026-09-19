// Two processes increment a counter in a MAP_SHARED file mapping with no
// synchronization: the cross-process form of race.c.
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define N 200000

extern char **environ;

static volatile long *map_counter(const char *path) {
    int fd = open(path, O_RDWR);
    if (fd < 0) {
        perror(path);
        exit(2);
    }
    void *p = mmap(NULL, sizeof(long), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    return p;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "worker") == 0) {
        volatile long *p = map_counter(argv[2]);
        for (int i = 0; i < N; i++) *p = *p + 1;
        return 0;
    }
    const char *path = argc > 1 ? argv[1] : "shared_map.bin";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    long zero = 0;
    if (fd < 0 || write(fd, &zero, sizeof zero) != sizeof zero) return 2;
    close(fd);

    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    pid_t pids[2];
    for (int i = 0; i < 2; i++) {
        char *args[] = {self, "worker", (char *)path, NULL};
        if (posix_spawn(&pids[i], self, NULL, NULL, args, environ) != 0) return 3;
    }
    for (int i = 0; i < 2; i++) waitpid(pids[i], NULL, 0);
    printf("total=%ld expected=%d\n", *map_counter(path), 2 * N);
    return 0;
}
