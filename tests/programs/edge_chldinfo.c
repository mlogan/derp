// From a review: must behave under the supervisor as it does natively.
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
static volatile sig_atomic_t ran, pid_ok, code_ok, status;
static pid_t child;
static void on_chld(int s, siginfo_t *i, void *c) {
    (void)s; (void)c;
    ran++; pid_ok = i->si_pid == child; code_ok = i->si_code == CLD_EXITED; status = i->si_status;
}
static volatile sig_atomic_t ran2;
static void once(int s) { (void)s; ran2++; }
int main(void) {
    struct sigaction sa = {0};
    sa.sa_sigaction = on_chld; sa.sa_flags = SA_SIGINFO;
    sigaction(SIGCHLD, &sa, NULL);
    child = fork();
    if (child == 0) _exit(3);
    for (int i = 0; i < 100 && !ran; i++) usleep(1000);
    printf("siginfo: ran %d, si_pid is the child: %d, si_code CLD_EXITED: %d, si_status %d\n", (int)ran, (int)pid_ok, (int)code_ok, (int)status);
    waitpid(child, NULL, 0);
    struct sigaction sb = {0};
    sb.sa_handler = once; sb.sa_flags = SA_RESETHAND;
    sigaction(SIGCHLD, &sb, NULL);
    pid_t c2 = fork();
    if (c2 == 0) _exit(4);
    for (int i = 0; i < 100 && !ran2; i++) usleep(1000);
    printf("SA_RESETHAND: ran %d\n", (int)ran2);
    waitpid(c2, NULL, 0);
    return 0;
}
