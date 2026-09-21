// From a review: must behave under the supervisor as it does natively.
#include <stdio.h>
#include <sys/event.h>
#include <unistd.h>
int main(void) {
    int kq = kqueue();
    struct kevent ch;
    EV_SET(&ch, 1, EVFILT_USER, EV_ADD | EV_RECEIPT, 0, 0, NULL);
    int rc = kevent(kq, &ch, 1, NULL, 0, NULL);
    printf("kevent with EV_RECEIPT and no event list: %d\n", rc);
    return 0;
}
