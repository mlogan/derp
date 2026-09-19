// N processes each add 1 to a counter kept in a file, K times, by read,
// add, write. With --flock the file lock is held across the update; without
// it updates can be lost whenever another process runs in between.
//
//   counter_file <file> <processes> <k> [--flock]
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int worker(const char *path, int k, int locked) {
    int fd = open(path, O_RDWR);
    if (fd < 0) {
        perror("open");
        return 2;
    }
    for (int i = 0; i < k; i++) {
        char text[32] = {0};
        if (locked && flock(fd, LOCK_EX) != 0) perror("flock");
        if (pread(fd, text, sizeof text - 1, 0) < 0) perror("pread");
        long value = atol(text) + 1;
        int len = snprintf(text, sizeof text, "%-20ld", value);
        if (pwrite(fd, text, (size_t)len, 0) != len) perror("pwrite");
        if (locked) flock(fd, LOCK_UN);
    }
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 5 && strcmp(argv[1], "worker") == 0)
        return worker(argv[2], atoi(argv[3]), strcmp(argv[4], "--flock") == 0);
    if (argc < 4) return 2;
    const char *path = argv[1];
    int processes = atoi(argv[2]);
    const char *mode = argc > 4 ? argv[4] : "--racy";

    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "0                   ", 20) != 20) {
        perror(path);
        return 2;
    }
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    pid_t pids[32];
    if (processes > 32) processes = 32;
    for (int i = 0; i < processes; i++) {
        char *args[] = {self, "worker", (char *)path, argv[3], (char *)mode, NULL};
        if (posix_spawn(&pids[i], self, NULL, NULL, args, environ) != 0) return 3;
    }
    for (int i = 0; i < processes; i++) waitpid(pids[i], NULL, 0);
    char text[32] = {0};
    if (pread(fd, text, sizeof text - 1, 0) < 0) perror("pread");
    printf("total=%ld expected=%d\n", atol(text), processes * atoi(argv[3]));
    return 0;
}
