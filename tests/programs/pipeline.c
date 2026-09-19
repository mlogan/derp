// producer | filter | consumer, three processes started by a parent with
// posix_spawn and connected by pipes. Enough data goes through to fill the
// pipe buffers many times, so every stage blocks on both reads and writes.
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int producer(long n) {
    for (long i = 1; i <= n; i++) printf("%ld\n", i * 7 + 3);
    return 0;
}

static int filter(void) {
    char line[64];
    while (fgets(line, sizeof line, stdin)) {
        long v = atol(line);
        if (v % 3 == 0) printf("%ld\n", v / 3);
    }
    return 0;
}

static int consumer(void) {
    char line[64];
    unsigned long sum = 0, count = 0;
    while (fgets(line, sizeof line, stdin)) {
        sum = sum * 31 + (unsigned long)atol(line);
        count++;
    }
    printf("count=%lu checksum=%lu\n", count, sum);
    return 0;
}

static pid_t stage(const char *self, const char *name, const char *arg, int in, int out,
                   int *to_close, int n_close) {
    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    if (in >= 0) posix_spawn_file_actions_adddup2(&fa, in, 0);
    if (out >= 0) posix_spawn_file_actions_adddup2(&fa, out, 1);
    for (int i = 0; i < n_close; i++) posix_spawn_file_actions_addclose(&fa, to_close[i]);
    char *args[] = {(char *)self, (char *)name, (char *)arg, NULL};
    pid_t pid = 0;
    int rc = posix_spawn(&pid, self, &fa, NULL, args, environ);
    posix_spawn_file_actions_destroy(&fa);
    if (rc != 0) {
        fprintf(stderr, "posix_spawn %s: %s\n", name, strerror(rc));
        exit(3);
    }
    return pid;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "producer") == 0) return producer(atol(argv[2]));
    if (argc >= 2 && strcmp(argv[1], "filter") == 0) return filter();
    if (argc >= 2 && strcmp(argv[1], "consumer") == 0) return consumer();

    const char *count = argc > 1 ? argv[1] : "200000";
    char self[4096];
    uint32_t size = sizeof self;
    if (_NSGetExecutablePath(self, &size) != 0) return 2;

    int a[2], b[2];
    if (pipe(a) != 0 || pipe(b) != 0) return 2;
    int all[4] = {a[0], a[1], b[0], b[1]};
    pid_t pids[3];
    pids[0] = stage(self, "producer", count, -1, a[1], all, 4);
    pids[1] = stage(self, "filter", "-", a[0], b[1], all, 4);
    pids[2] = stage(self, "consumer", "-", b[0], -1, all, 4);
    for (int i = 0; i < 4; i++) close(all[i]);

    int failed = 0;
    for (int i = 0; i < 3; i++) {
        int status = 0;
        waitpid(pids[i], &status, 0);
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) failed = 1;
    }
    return failed;
}
