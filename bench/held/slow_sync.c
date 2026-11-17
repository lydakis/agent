// A disk that takes its time to sync, for `bench.held_reads`: every fsync
// and fdatasync of the process it is preloaded into waits SLOW_SYNC_MS
// first, as a saturated disk holds a commit. Linux only.
//
//   cc -O2 -shared -fPIC -o slow_sync.so slow_sync.c -ldl
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <time.h>

static void wait_out(void) {
    static long ms = -1;
    if (ms < 0) {
        const char *set = getenv("SLOW_SYNC_MS");
        ms = set ? atol(set) : 0;
    }
    if (ms > 0) {
        struct timespec span = {ms / 1000, (ms % 1000) * 1000000L};
        while (nanosleep(&span, &span) != 0) {
        }
    }
}

int fsync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    wait_out();
    return real(fd);
}

int fdatasync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fdatasync");
    wait_out();
    return real(fd);
}
