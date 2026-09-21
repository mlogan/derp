// From a review: must behave under the supervisor as it does natively.
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>
static volatile sig_atomic_t got;
static void on_usr1(int s) { (void)s; got = 1; }
static void *worker(void *a) {
    (void)a;
    sigset_t set; sigemptyset(&set); sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_UNBLOCK, &set, NULL);
    for (int i = 0; i < 200 && !got; i++) usleep(1000);
    return NULL;
}
int main(void) {
    signal(SIGUSR1, on_usr1);
    sigset_t set; sigemptyset(&set); sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
    pthread_t t; pthread_create(&t, NULL, worker, NULL);
    usleep(5000);
    kill(getpid(), SIGUSR1);
    pthread_join(t, NULL);
    printf("handler ran on the thread that accepts the signal: %s\n", got ? "yes" : "NO");
    return 0;
}
