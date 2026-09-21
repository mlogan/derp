// From a review: must behave under the supervisor as it does natively.
#include <stdio.h>
#include <fcntl.h>
#include <sys/event.h>
#include <unistd.h>
int main(void) {
    int kq = kqueue();
    int fd = open("/dev/null", O_RDONLY);
    struct kevent ch, ev;
    EV_SET(&ch, fd, EVFILT_TIMER, EV_ADD | EV_ONESHOT, 0, 50, NULL);
    kevent(kq, &ch, 1, NULL, 0, NULL);
    close(fd);
    int rc = kevent(kq, NULL, 0, &ev, 1, NULL);
    printf("timer with ident %s: kevent %d\n", "== a closed fd number", rc);
    return 0;
}
