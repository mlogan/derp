// Two processes taking turns through a System V semaphore set, the way
// postgres's lightweight locks sleep: the order of their prints is the
// schedule's, and the set is created by key as postgres does.
#include <stdio.h>
#include <stdlib.h>
#include <sys/ipc.h>
#include <sys/sem.h>
#include <sys/wait.h>
#include <unistd.h>

static void op(int id, int num, int delta) {
    struct sembuf b = {(unsigned short)num, (short)delta, 0};
    while (semop(id, &b, 1) < 0) perror("semop");
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int id = semget(5432000, 2, IPC_CREAT | IPC_EXCL | 0600);
    if (id < 0) { perror("semget"); return 1; }
    semctl(id, 0, SETVAL, 1);
    semctl(id, 1, SETVAL, 0);
    pid_t child = fork();
    for (int i = 0; i < 4; i++) {
        if (child == 0) {
            op(id, 1, -1);
            printf("child %d\n", i);
            op(id, 0, 1);
        } else {
            op(id, 0, -1);
            printf("parent %d\n", i);
            op(id, 1, 1);
        }
    }
    if (child == 0) return 0;
    waitpid(child, NULL, 0);
    printf("keyed lookup %s\n", semget(5432000, 2, 0) < 0 ? "finds nothing" : "finds it");
    semctl(id, 0, IPC_RMID);
    return 0;
}
