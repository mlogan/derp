// A guest that puts work on a dispatch queue. That is not supported: the
// supervisor ends the run with an error instead of letting the block run
// outside the scheduler.
//
//   gcd_user async | sync
#include <dispatch/dispatch.h>
#include <stdio.h>
#include <string.h>

static void work(void *context) { printf("block ran: %s\n", (const char *)context); }

int main(int argc, char **argv) {
    dispatch_queue_t q = dispatch_queue_create("guest.queue", DISPATCH_QUEUE_SERIAL);
    if (argc > 1 && strcmp(argv[1], "sync") == 0) {
        // Runs on the calling thread: fine
        dispatch_sync_f(q, "sync", work);
        puts("done");
        return 0;
    }
    puts("about to use dispatch_async_f");
    fflush(stdout);
    dispatch_async_f(q, "async", work);
    dispatch_sync_f(q, "drain", work);
    puts("not reached");
    return 0;
}
