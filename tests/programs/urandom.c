// Reads the random device the way Redis seeds its hash tables, and prints
// what it got: the seed's bytes under the supervisor.
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    unsigned char buf[8];
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd < 0 || read(fd, buf, sizeof buf) != (ssize_t)sizeof buf) return 1;
    close(fd);
    for (int i = 0; i < 8; i++) printf("%02x", buf[i]);
    printf("\n");
    return 0;
}
