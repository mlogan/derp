// A parent signals a child that is asleep, and reports how the child ended.
// Modes: int (SIGINT, no handler), term (SIGTERM, no handler), handler
// (SIGTERM, a handler; the child exits 0), ignore (SIGTERM ignored, then
// SIGKILL).
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got;
static void on_term(int s) { (void)s; got = 1; }

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "term";
    int sig = strcmp(mode, "int") == 0 ? SIGINT : SIGTERM;
    pid_t child = fork();
    if (child == 0) {
        if (strcmp(mode, "handler") == 0) signal(SIGTERM, on_term);
        if (strcmp(mode, "ignore") == 0) signal(SIGTERM, SIG_IGN);
        for (;;) {
            sleep(100);
            if (got) {
                printf("child: handler ran\n");
                fflush(stdout);
                _exit(0);
            }
        }
    }
    usleep(10000);
    long sent = now_ms();
    kill(child, sig);
    if (strcmp(mode, "ignore") == 0) {
        usleep(10000);
        kill(child, SIGKILL);
    }
    int status;
    waitpid(child, &status, 0);
    long took = now_ms() - sent;
    if (WIFSIGNALED(status)) printf("child: killed by signal %d\n", WTERMSIG(status));
    else printf("child: exited %d\n", WEXITSTATUS(status));
    printf("promptly: %s\n", took < 1000 ? "yes" : "NO");
    return 0;
}
