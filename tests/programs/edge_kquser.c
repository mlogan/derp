// From a review: must behave under the supervisor as it does natively.
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <sys/event.h>
#include <unistd.h>
static int kq;
static void *waker(void *a) {
    (void)a;
    usleep(5000);
    struct kevent t;
    EV_SET(&t, 1, EVFILT_USER, 0, NOTE_TRIGGER, 0, NULL);
    kevent(kq, &t, 1, NULL, 0, NULL);
    return NULL;
}
int main(void) {
    kq = kqueue();
    struct kevent ch[2], ev;
    signal(SIGUSR2, SIG_IGN);
    EV_SET(&ch[0], 1, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, NULL);
    EV_SET(&ch[1], SIGUSR2, EVFILT_SIGNAL, EV_ADD, 0, 0, NULL);
    kevent(kq, ch, 2, NULL, 0, NULL);
    pthread_t t; pthread_create(&t, NULL, waker, NULL);
    int n = kevent(kq, NULL, 0, &ev, 1, NULL);
    printf("woken by the user event: %d (filter %d)\n", n, ev.filter);
    pthread_join(t, NULL);
    return 0;
}
