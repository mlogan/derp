// Frees 4 MB blocks over and over and says whether the memory they held
// stayed resident, and what a freed block reads as afterwards: the heap
// gives freed runs back, as zeros, unless the run says otherwise.
#include <mach/mach.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define BLOCK (4u << 20)

static unsigned char *volatile escaped;

static unsigned long long resident(void) {
    mach_task_basic_info_data_t info;
    mach_msg_type_number_t count = MACH_TASK_BASIC_INFO_COUNT;
    task_info(mach_task_self(), MACH_TASK_BASIC_INFO, (task_info_t)&info, &count);
    return info.resident_size;
}

int main(void) {
    unsigned long long before = resident();
    for (int i = 0; i < 100; i++) {
        escaped = malloc(BLOCK);
        memset(escaped, 0xa5, BLOCK);
        free(escaped);
    }
    // 400 MB touched in all
    printf("resident: %s\n", resident() - before < (64u << 20) ? "bounded" : "grew");
    printf("freed block: %s\n", escaped[BLOCK / 2] == 0 ? "zeros" : "old contents");
    return 0;
}
