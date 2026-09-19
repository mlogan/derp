// What a guest may name on its host: files in its own directory, system
// locations, and nothing of another host's.
//
//   hostfs <other host's name>
#include <errno.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static const char *outcome(int ok) { return ok ? "ok" : errno == EACCES ? "refused" : strerror(errno); }

static void try_open(const char *label, const char *path, int flags) {
    int fd = open(path, flags, 0644);
    printf("%s: %s\n", label, outcome(fd >= 0));
    if (fd >= 0) close(fd);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    char host[64], cwd[4096], path[4200];
    gethostname(host, sizeof host);
    if (!getcwd(cwd, sizeof cwd)) return 2;
    const char *leaf = strrchr(cwd, '/') + 1;
    printf("%s starts in a directory named %s\n", host, leaf);
    printf("HOME and PWD agree with it: %s\n",
           strcmp(getenv("HOME"), cwd) == 0 && strcmp(getenv("PWD"), cwd) == 0 ? "yes" : "no");
    snprintf(path, sizeof path, "%s/tmp", cwd);
    printf("TMPDIR is inside it: %s\n", strcmp(getenv("TMPDIR"), path) == 0 ? "yes" : "no");

    if (argc > 2) {  // the spawned child
        snprintf(path, sizeof path, "%s/../%s/data.txt", cwd, argv[1]);
        try_open("child reads the other host's file", path, O_RDONLY);
        try_open("child reads its own host's file", "data.txt", O_RDONLY);
        return 0;
    }

    FILE *f = fopen("data.txt", "w");
    printf("write data.txt: %s\n", outcome(f != NULL));
    if (f) {
        fprintf(f, "written on %s\n", host);
        fclose(f);
    }
    char line[128] = "(not copied)";
    if ((f = fopen("seed.txt", "r"))) {
        if (!fgets(line, sizeof line, f)) line[0] = 0;
        fclose(f);
        line[strcspn(line, "\n")] = 0;
    }
    printf("seed.txt from the run file: %s\n", line);
    printf("mkdir sub and a file by absolute path: %s\n", outcome(mkdir("sub", 0755) == 0));
    snprintf(path, sizeof path, "%s/sub/inner.txt", cwd);
    try_open("absolute path inside", path, O_WRONLY | O_CREAT);
    try_open("/etc/hosts", "/etc/hosts", O_RDONLY);
    try_open("/dev/null for writing", "/dev/null", O_WRONLY);

    struct stat st;
    printf("stat /: %s\n", outcome(stat("/", &st) == 0));
    snprintf(path, sizeof path, "%s/..", cwd);
    printf("stat the directory above: %s\n", outcome(stat(path, &st) == 0));
    try_open("but not open it", path, O_RDONLY);

    snprintf(path, sizeof path, "../%s/data.txt", argv[1]);
    try_open("other host by relative path", path, O_RDONLY);
    snprintf(path, sizeof path, "%s/../%s/data.txt", cwd, argv[1]);
    try_open("other host by absolute path", path, O_RDONLY);
    printf("stat the other host's file: %s\n", outcome(stat(path, &st) == 0));
    printf("rename into the other host: %s\n", outcome(rename("data.txt", path) == 0));
    snprintf(path, sizeof path, "%s/../%s", cwd, argv[1]);
    printf("chdir to the other host: %s\n", outcome(chdir(path) == 0));
    try_open("a file in the real /tmp", "/tmp/rewrite-hostfs-escape", O_WRONLY | O_CREAT);

    char self[4096];
    uint32_t size = sizeof self;
    _NSGetExecutablePath(self, &size);
    char *args[] = {self, argv[1], "child", NULL};
    pid_t pid;
    fflush(stdout);
    if (posix_spawn(&pid, self, NULL, NULL, args, environ) != 0) return 3;
    waitpid(pid, NULL, 0);
    return 0;
}
