// A parent that counts SIGCHLD: one per child, delivered by the time
// waitpid has returned and the parent has run on, whichever of sigaction
// and signal installed the handler.
#include <mach-o/dyld.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static volatile sig_atomic_t seen;

static void on_child(int sig) {
    if (sig == SIGCHLD) seen++;
}

static int child_exits_with(const char *self, const char *code) {
    char *args[] = {(char *)self, "child", (char *)code, NULL};
    pid_t pid;
    int status;
    if (posix_spawn(&pid, self, NULL, NULL, args, environ) != 0) return -1;
    if (waitpid(pid, &status, 0) != pid) return -1;
    // The handler runs when this thread next takes up the baton
    for (int i = 0; i < 100 && seen == 0; i++) usleep(1000);
    return WEXITSTATUS(status);
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "child") == 0) return argv[2][0] - '0';
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);

    struct sigaction sa = {0}, old = {0};
    sa.sa_handler = on_child;
    sigaction(SIGCHLD, &sa, NULL);
    int code = child_exits_with(self, "3");
    printf("sigaction: exit %d, handler ran %d\n", code, (int)seen);

    seen = 0;
    sigaction(SIGCHLD, NULL, &old);
    printf("handler reads back: %s\n", old.sa_handler == on_child ? "yes" : "no");
    signal(SIGCHLD, on_child);
    code = child_exits_with(self, "4");
    printf("signal: exit %d, handler ran %d\n", code, (int)seen);

    seen = 0;
    signal(SIGCHLD, SIG_DFL);
    code = child_exits_with(self, "5");
    printf("default: exit %d, handler ran %d\n", code, (int)seen);
    return 0;
}
