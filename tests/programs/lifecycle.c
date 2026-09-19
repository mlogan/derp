// Process lifecycle corners.
//
//   lifecycle stuck     read from a pipe nobody will ever write to
//   lifecycle killer    start a child that never ends, kill it, reap it
#include <mach-o/dyld.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    if (strcmp(argv[1], "stuck") == 0) {
        int p[2];
        char c;
        if (pipe(p) != 0) return 2;
        puts("waiting for a byte that never comes");
        fflush(stdout);
        return read(p[0], &c, 1) == 1 ? 0 : 1;
    }
    if (strcmp(argv[1], "forever") == 0) {
        for (;;) sleep(3600);
    }
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char *args[] = {self, "forever", NULL};
    pid_t pid;
    if (posix_spawn(&pid, self, NULL, NULL, args, environ) != 0) return 3;
    printf("killing %d: %d\n", pid, kill(pid, SIGTERM));
    int status = 0;
    pid_t got = waitpid(pid, &status, 0);
    printf("reaped %d, killed by a signal: %s\n", got, WIFSIGNALED(status) ? "yes" : "no");
    printf("again: %d\n", kill(pid, 0));
    return 0;
}
