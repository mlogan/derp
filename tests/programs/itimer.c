// An interval timer's SIGALRMs, counted by a handler, while the main
// thread sleeps in short steps: three by the time it looks.
#include <signal.h>
#include <stdio.h>
#include <sys/time.h>
#include <unistd.h>

static volatile int alarms;

static void on_alarm(int sig) { (void)sig; alarms++; }

int main(void) {
    signal(SIGALRM, on_alarm);
    struct itimerval it = {{0, 50000}, {0, 50000}};
    setitimer(ITIMER_REAL, &it, NULL);
    struct timeval t0, t1;
    gettimeofday(&t0, NULL);
    while (alarms < 3) usleep(10000);
    gettimeofday(&t1, NULL);
    struct itimerval cur;
    getitimer(ITIMER_REAL, &cur);
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_usec - t0.tv_usec) / 1000;
    printf("alarms=%d after %ld ms, %s left to the next\n", alarms, ms / 10 * 10,
           cur.it_value.tv_usec < 50000 ? "under 50 ms" : "50 ms");
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);
    unsigned left = alarm(0);
    printf("cancelled, alarm(0) says %u\n", left);
    return 0;
}
