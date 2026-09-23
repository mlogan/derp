// The Mach clock services, as RocksDB's NowNanos reads them on macOS.
#include <mach/clock.h>
#include <mach/mach.h>
#include <stdio.h>

static void show(const char *name, clock_id_t id) {
    clock_serv_t clock;
    mach_timespec_t ts;
    host_get_clock_service(mach_host_self(), id, &clock);
    clock_get_time(clock, &ts);
    mach_port_deallocate(mach_task_self(), clock);
    printf("%s %u.%09d\n", name, ts.tv_sec, ts.tv_nsec);
}

int main(void) {
    show("calendar", CALENDAR_CLOCK);
    show("system", SYSTEM_CLOCK);
    return 0;
}
