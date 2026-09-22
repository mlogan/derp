// What the outside network looks like from a guest: a name lookup and a
// connection beyond the virtual network, and a numeric address, which
// needs no lookup.
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    struct addrinfo *res = NULL;
    int rc = getaddrinfo("example.com", "80", NULL, &res);
    printf("lookup %s\n", rc == 0 ? "ok" : gai_strerror(rc));
    if (rc == 0) freeaddrinfo(res);
    rc = getaddrinfo("1.2.3.4", "80", NULL, &res);
    printf("numeric %s\n", rc == 0 ? "ok" : gai_strerror(rc));
    if (rc == 0) freeaddrinfo(res);

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(80);
    inet_pton(AF_INET, "1.1.1.1", &addr.sin_addr);
    rc = connect(fd, (struct sockaddr *)&addr, sizeof addr);
    printf("connect %s\n", rc == 0 ? "ok" : strerror(errno));
    close(fd);
    return 0;
}
