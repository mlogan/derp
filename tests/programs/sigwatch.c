// A child that watches its parent with kqueue, the way postgres's
// children watch the postmaster: EVFILT_SIGNAL for the signals the parent
// sends it and EVFILT_PROC for the parent's death.
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/event.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int handled;

static void on_usr1(int sig) { (void)sig; handled++; }

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        signal(SIGUSR1, on_usr1);
        int kq = kqueue();
        struct kevent regs[2];
        EV_SET(&regs[0], SIGUSR1, EVFILT_SIGNAL, EV_ADD, 0, 0, NULL);
        EV_SET(&regs[1], parent, EVFILT_PROC, EV_ADD, NOTE_EXIT | NOTE_EXITSTATUS, 0, NULL);
        if (kevent(kq, regs, 2, NULL, 0, NULL) < 0) { perror("kevent"); return 1; }
        printf("child: parent %s\n", kill(parent, 0) == 0 ? "alive" : "gone");
        for (;;) {
            struct kevent ev;
            int n = kevent(kq, NULL, 0, &ev, 1, NULL);
            if (n <= 0) { perror("wait"); return 1; }
            if (ev.filter == EVFILT_SIGNAL)
                printf("child: signal %d x%d, handled %d\n", (int)ev.ident, (int)ev.data, handled);
            if (ev.filter == EVFILT_PROC) {
                int status = (int)ev.data;
                printf("child: parent exited with %d, kill(0) says %s\n", WEXITSTATUS(status),
                       kill(parent, 0) == 0 ? "alive" : "gone");
                return 0;
            }
        }
    }
    for (int i = 0; i < 3; i++) {
        usleep(10000);
        kill(child, SIGUSR1);
        if (i == 1) kill(child, SIGUSR1);
    }
    usleep(10000);
    printf("parent: done\n");
    exit(7);
}
