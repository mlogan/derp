// exec_race N: a worker takes SIGALRM at a high rate and, in the handler,
// writes to a pipe of its own many times; main re-executes this program N
// times. The handler runs on the parked worker at moments of real time,
// and each write wakes the scheduler's waiters, which takes the scheduler
// lock: sometimes on a thread that is inside it already.
#include <mach-o/dyld.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <execinfo.h>
#include <unistd.h>

extern char **environ;
static int pipe_fd[2];

static void on_alarm(int sig) {
    (void)sig;
    // Many writes: a long stretch of real time in and out of the scheduler
    // lock, into which an exec by the other thread can fall
    char b = 1;
    for (int i = 0; i < 400; i++) write(pipe_fd[1], &b, 1);
}

static void on_ill(int sig) {
    void *frames[32];
    int n = backtrace(frames, 32);
    char line[64];
    int len = snprintf(line, sizeof line, "SIG %d on %s thread:\n", sig, pthread_main_np() ? "main" : "worker");
    write(2, line, len);
    backtrace_symbols_fd(frames, n, 2);
    _exit(99);
}

static void *worker(void *arg) {
    (void)arg;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    pthread_sigmask(SIG_UNBLOCK, &set, NULL);
    struct itimerval every = {{0, 100}, {0, 100}};
    setitimer(ITIMER_REAL, &every, NULL);
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
    signal(SIGALRM, on_alarm);
    signal(SIGILL, on_ill);
    signal(SIGTRAP, on_ill);
    signal(SIGBUS, on_ill);
    signal(SIGSEGV, on_ill);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    usleep(2000);
    // No signal may be pending at the exec (macOS then kills the new image
    // before its first instruction): stop the timer and take what is left.
    // A handler already running on the worker is what this is about.
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);
    sigset_t pending;
    sigpending(&pending);
    if (sigismember(&pending, SIGALRM)) {
        int got;
        sigwait(&set, &got);
    }
    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char count[16];
    snprintf(count, sizeof count, "%d", left - 1);
    char *args[] = {self, count, NULL};
    execve(self, args, environ);
    perror("execve");
    return 2;
}
