// Process lifecycle: a parent starts children with posix_spawn, fork+execve
// and plain fork, and reaps them by pid and with wait(). Everyone writes to
// the same stdout, so the order of lines is the schedule.
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static unsigned long work(unsigned long seed, int rounds) {
    unsigned long x = seed;
    for (int i = 0; i < rounds; i++) x = x * 6364136223846793005UL + 1442695040888963407UL;
    return x >> 40;
}

static int child(int index) {
    unsigned long sum = work((unsigned long)index, 400000);
    printf("child %d pid=%d ppid=%d sum=%lu\n", index, getpid(), getppid(), sum);
    return 10 + index;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "child") == 0) return child(atoi(argv[2]));

    char self[4096];
    uint32_t size = sizeof self;
    if (_NSGetExecutablePath(self, &size) != 0) return 2;
    printf("parent pid=%d ppid=%d\n", getpid(), getppid());
    fflush(stdout);

    pid_t pids[4];
    for (int i = 0; i < 2; i++) {
        char index[8];
        snprintf(index, sizeof index, "%d", i);
        char *args[] = {self, "child", index, NULL};
        int rc = posix_spawn(&pids[i], self, NULL, NULL, args, environ);
        if (rc != 0) {
            fprintf(stderr, "posix_spawn: %s\n", strerror(rc));
            return 3;
        }
    }
    pids[2] = fork();
    if (pids[2] == 0) {
        char *args[] = {self, "child", "2", NULL};
        execve(self, args, environ);
        _exit(99);
    }
    pids[3] = fork();
    if (pids[3] == 0) {
        int rc = child(3);
        fflush(stdout);
        _exit(rc);
    }
    printf("spawned %d %d %d %d\n", pids[0], pids[1], pids[2], pids[3]);
    fflush(stdout);

    unsigned long mine = work(99, 400000);
    int status = 0;
    pid_t got = waitpid(pids[1], &status, 0);
    printf("reaped %d exit=%d\n", got, WEXITSTATUS(status));
    for (int i = 0; i < 3; i++) {
        got = wait(&status);
        printf("reaped %d exit=%d\n", got, WEXITSTATUS(status));
    }
    got = wait(&status);
    printf("no more children: %d sum=%lu\n", got, mine);
    return 0;
}
