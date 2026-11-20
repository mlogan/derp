// A process that dies the way it is told to.
//
//   dies once      abort on its first life (it leaves a mark in its host
//                  directory), then wait forever on the next
//   dies forever   wait forever
//   dies exit0     exit 0 after a moment
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "forever";
    if (strcmp(mode, "exit0") == 0) {
        usleep(10000);
        return 0;
    }
    if (strcmp(mode, "once") == 0 && access("died", F_OK) != 0) {
        close(open("died", O_WRONLY | O_CREAT, 0644));
        usleep(10000);
        abort();
    }
    for (;;) sleep(3600);
}
