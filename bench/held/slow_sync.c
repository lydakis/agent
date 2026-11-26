// A disk that takes its time, for `bench.held_reads` and
// `bench.held_window`: every fsync and fdatasync of the process it is
// preloaded into waits SLOW_SYNC_MS first, as a saturated disk holds a
// commit. With SLOW_SYNC_FILE, the wait is the number of milliseconds that
// file holds, read at each call, so a run can slow the disk down (and speed
// it up again) while the process runs; a missing file is no wait.
//
// With SLOW_WRITES=1, every change to the file system waits too, as it
// does while the disk's journal is held by another's sync: opening a file
// to write or create it, a rename, an unlink, a mkdir. Linux only.
//
//   cc -O2 -shared -fPIC -o slow_sync.so slow_sync.c -ldl
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static long held_ms(void) {
    static long ms = -1;
    static const char *file = NULL;
    if (ms < 0) {
        const char *set = getenv("SLOW_SYNC_MS");
        ms = set ? atol(set) : 0;
        file = getenv("SLOW_SYNC_FILE");
    }
    if (!file) return ms;
    static int (*real_open)(const char *, int, ...);
    if (!real_open) real_open = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, "open");
    char text[32] = {0};
    int fd = real_open(file, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 0;
    ssize_t n = read(fd, text, sizeof text - 1);
    close(fd);
    return n > 0 ? atol(text) : 0;
}

static void wait_out(void) {
    long ms = held_ms();
    if (ms > 0) {
        struct timespec span = {ms / 1000, (ms % 1000) * 1000000L};
        while (nanosleep(&span, &span) != 0) {
        }
    }
}

static int writes(void) {
    static int on = -1;
    if (on < 0) {
        const char *set = getenv("SLOW_WRITES");
        on = set && atoi(set) == 1;
    }
    return on;
}

static void wait_write(int flags) {
    if (writes() && (flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC))) wait_out();
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

#define OPEN(name, ...)                                                        \
    static int (*real)(__VA_ARGS__);                                           \
    if (!real) real = (int (*)(__VA_ARGS__))dlsym(RTLD_NEXT, name);           \
    mode_t mode = 0;                                                           \
    if (flags & (O_CREAT | O_TMPFILE)) {                                       \
        va_list given;                                                         \
        va_start(given, flags);                                                \
        mode = va_arg(given, mode_t);                                          \
        va_end(given);                                                         \
    }                                                                          \
    wait_write(flags);

int open(const char *path, int flags, ...) {
    OPEN("open", const char *, int, ...)
    return real(path, flags, mode);
}

int open64(const char *path, int flags, ...) {
    OPEN("open64", const char *, int, ...)
    return real(path, flags, mode);
}

int openat(int dir, const char *path, int flags, ...) {
    OPEN("openat", int, const char *, int, ...)
    return real(dir, path, flags, mode);
}

int openat64(int dir, const char *path, int flags, ...) {
    OPEN("openat64", int, const char *, int, ...)
    return real(dir, path, flags, mode);
}

int rename(const char *from, const char *to) {
    static int (*real)(const char *, const char *);
    if (!real) real = (int (*)(const char *, const char *))dlsym(RTLD_NEXT, "rename");
    if (writes()) wait_out();
    return real(from, to);
}

int unlink(const char *path) {
    static int (*real)(const char *);
    if (!real) real = (int (*)(const char *))dlsym(RTLD_NEXT, "unlink");
    if (writes()) wait_out();
    return real(path);
}

int mkdir(const char *path, mode_t mode) {
    static int (*real)(const char *, mode_t);
    if (!real) real = (int (*)(const char *, mode_t))dlsym(RTLD_NEXT, "mkdir");
    if (writes()) wait_out();
    return real(path, mode);
}
