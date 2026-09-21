// From a review: must behave under the supervisor as it does natively.
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
static volatile sig_atomic_t ran;
static void on_chld(int s) { (void)s; ran++; }
int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_chld;
    sigaction(SIGCHLD, &sa, NULL);
    sigset_t set, old; sigemptyset(&set); sigaddset(&set, SIGCHLD);
    sigprocmask(SIG_BLOCK, &set, &old);
    pid_t c = fork();
    if (c == 0) _exit(3);
    for (int i = 0; i < 50; i++) usleep(1000);   /* child dies meanwhile */
    sigprocmask(SIG_SETMASK, &old, NULL);         /* pending SIGCHLD is delivered here */
    for (int i = 0; i < 50 && !ran; i++) usleep(1000);
    printf("handler ran after unblocking: %d\n", (int)ran);
    int st; waitpid(c, &st, 0);
    return 0;
}
