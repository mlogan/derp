// parent_of PROGRAM [ARGS…]: start PROGRAM from this binary's directory
// and end as it ended. The interesting part of the run is in the child.
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char *slash = strrchr(self, '/');
    if (slash) *slash = 0;
    char path[4096];
    snprintf(path, sizeof path, "%s/%s", self, argv[1]);
    pid_t pid;
    if (posix_spawn(&pid, path, NULL, NULL, argv + 1, environ) != 0) {
        perror(path);
        return 2;
    }
    int status;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status)) {
        printf("child died of signal %d\n", WTERMSIG(status));
        return 1;
    }
    printf("child exited %d\n", WEXITSTATUS(status));
    return WEXITSTATUS(status);
}
