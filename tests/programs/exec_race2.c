// exec_race2 N: main sends the worker a signal and calls execve at once.
// The handler runs on the parked worker in real time, writing to a pipe
// many times, in and out of the scheduler lock, while the exec is under
// way; the exec kills the worker wherever it is.
#include <mach-o/dyld.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern char **environ;
static int pipe_fd[2];

static void on_usr1(int sig) {
    (void)sig;
    char b = 1;
    for (int i = 0; i < 2000; i++) write(pipe_fd[1], &b, 1);
}

static void *worker(void *arg) {
    (void)arg;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_UNBLOCK, &set, NULL);
    char buf[256];
    for (;;) read(pipe_fd[0], buf, sizeof buf);
    return NULL;
}

int main(int argc, char **argv) {
    int left = argc > 1 ? atoi(argv[1]) : 0;
    if (left <= 0) {
        puts("done");
        return 0;
    }
    pipe(pipe_fd);
    signal(SIGUSR1, on_usr1);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    usleep(1000);
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char count[16];
    snprintf(count, sizeof count, "%d", left - 1);
    char *args[] = {self, count, NULL};
    pthread_kill(t, SIGUSR1);
    execve(self, args, environ);
    perror("execve");
    return 2;
}
