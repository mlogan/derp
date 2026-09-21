// From a review: must behave under the supervisor as it does natively.
#include <stdio.h>
#include <errno.h>
#include <string.h>
#include <sys/socket.h>
int main(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    char b[8];
    ssize_t n = recv(sv[0], b, sizeof b, MSG_DONTWAIT);
    printf("recv MSG_DONTWAIT on empty pair: %zd (%s)\n", n, n < 0 ? strerror(errno) : "ok");
    return 0;
}
