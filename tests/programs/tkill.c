// A thread-directed signal (pthread_kill) to a sibling that sleeps: the
// handler runs on the sibling at a point of the schedule, and says so.
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static pthread_t worker;
static volatile int on_worker, elsewhere, rounds;

static void on_usr1(int sig) {
    (void)sig;
    if (pthread_equal(pthread_self(), worker)) on_worker++; else elsewhere++;
}

static void *run(void *arg) {
    (void)arg;
    for (int i = 0; i < 40 && on_worker < 3; i++) { usleep(1000); rounds++; }
    return NULL;
}

int main(void) {
    signal(SIGUSR1, on_usr1);
    pthread_create(&worker, NULL, run, NULL);
    for (int i = 0; i < 3; i++) { usleep(5000); pthread_kill(worker, SIGUSR1); }
    pthread_join(worker, NULL);
    printf("handled on the worker %d times, elsewhere %d, after %d rounds\n", on_worker, elsewhere, rounds);
    return 0;
}
