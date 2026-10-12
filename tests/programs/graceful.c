// A server that appends records to a journal in its host directory and,
// asked to stop with SIGTERM, finishes the record under way, writes a clean
// shutdown mark and exits 0. Each life reports how the last one ended.
//
//   graceful [handle|ignore|slow|default]
//
// handle: exit as described; ignore: SIGTERM is ignored; slow: the handler
// runs but the shutdown takes 50ms of sleeping before the exit; default: no
// handler, so SIGTERM ends the process.
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t stopping;
static void on_term(int s) { (void)s; stopping = 1; }

/// The journal's last line, or "" when there is none
static void last_line(char *out, size_t size) {
    out[0] = 0;
    FILE *f = fopen("journal", "r");
    if (!f) return;
    char line[64];
    while (fgets(line, sizeof line, f)) {
        line[strcspn(line, "\n")] = 0;
        snprintf(out, size, "%s", line);
    }
    fclose(f);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "handle";
    setvbuf(stdout, NULL, _IONBF, 0);
    if (strcmp(mode, "ignore") == 0) signal(SIGTERM, SIG_IGN);
    else if (strcmp(mode, "default") != 0) signal(SIGTERM, on_term);
    char last[64];
    last_line(last, sizeof last);
    printf("start: last life %s\n",
           last[0] == 0                       ? "none"
           : strcmp(last, "clean") == 0       ? "ended clean"
                                              : "ended without a mark");
    int fd = open("journal", O_WRONLY | O_APPEND | O_CREAT, 0644);
    for (int i = 0;; i++) {
        char rec[32];
        int len = snprintf(rec, sizeof rec, "record %d\n", i);
        if (write(fd, rec, (size_t)len) != len) return 2;
        usleep(1000);
        if (stopping) {
            if (strcmp(mode, "slow") == 0) usleep(50000);
            if (write(fd, "clean\n", 6) != 6) return 2;
            close(fd);
            printf("stop: clean after %d records\n", i + 1);
            return 0;
        }
    }
}
