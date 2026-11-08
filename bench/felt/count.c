// felt/count.c: a DYLD_INSERT_LIBRARIES interposer that logs every process
// launch, file open, stat and socket connect of the process it is loaded in,
// one line each, to $FELT_LOG/<pid>.log. macOS only. Build:
//   clang -O2 -dynamiclib -o count.dylib count.c
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <crt_externs.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <unistd.h>

#define INTERPOSE(replacement, original) \
  __attribute__((used)) static struct { const void *r; const void *o; } \
  interpose_##original __attribute__((section("__DATA,__interpose"))) = \
  { (const void *)(unsigned long)&replacement, (const void *)(unsigned long)&original };

static int log_fd = -1;

static long long now_ms(void) {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return (long long)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}

static void emit(const char *kind, const char *a, const char *b) {
  if (log_fd < 0) return;
  int saved = errno;
  char line[1200];
  int n = snprintf(line, sizeof line, "%lld\t%s\t%s\t%s\n", now_ms(), kind,
                   a ? a : "", b ? b : "");
  if (n > (int)sizeof line - 1) { n = sizeof line - 1; line[n - 1] = '\n'; }
  if (n > 0) write(log_fd, line, n);
  errno = saved;
}

__attribute__((constructor)) static void start(void) {
  const char *dir = getenv("FELT_LOG");
  if (!dir) return;
  char path[1024];
  snprintf(path, sizeof path, "%s/%d.log", dir, getpid());
  log_fd = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
  char **argv = *_NSGetArgv();
  int argc = *_NSGetArgc();
  char args[900] = "";
  for (int i = 0; i < argc && strlen(args) < 800; i++) {
    strncat(args, argv[i], 800 - strlen(args));
    strcat(args, " ");
  }
  char ppid[32];
  snprintf(ppid, sizeof ppid, "%d", getppid());
  emit("start", ppid, args);
}

// CPU seconds at a normal exit, for processes gone before the phase's last
// look (a killed process runs no destructor and goes uncounted).
__attribute__((destructor)) static void finish(void) {
  struct rusage ru;
  if (getrusage(RUSAGE_SELF, &ru) != 0) return;
  char cpu[32];
  snprintf(cpu, sizeof cpu, "%.3f",
           ru.ru_utime.tv_sec + ru.ru_stime.tv_sec +
               (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) / 1e6);
  emit("exit", cpu, "");
}

static void argv_line(char *const argv[], char *out, size_t size) {
  out[0] = 0;
  for (int i = 0; argv && argv[i] && i < 6; i++) {
    size_t used = strlen(out);
    if (used + 2 >= size) break;  // room for one more byte and the space
    strncat(out, argv[i], size - used - 2);
    strcat(out, " ");
  }
}

static int my_posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                          const posix_spawnattr_t *at, char *const argv[], char *const envp[]) {
  char a[600]; argv_line(argv, a, sizeof a);
  int rc = posix_spawn(pid, path, fa, at, argv, envp);
  emit(rc == 0 ? "spawn" : "spawnfail", path, a);
  return rc;
}
INTERPOSE(my_posix_spawn, posix_spawn)

static int my_posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *fa,
                           const posix_spawnattr_t *at, char *const argv[], char *const envp[]) {
  char a[600]; argv_line(argv, a, sizeof a);
  int rc = posix_spawnp(pid, file, fa, at, argv, envp);
  emit(rc == 0 ? "spawn" : "spawnfail", file, a);
  return rc;
}
INTERPOSE(my_posix_spawnp, posix_spawnp)

static int my_execve(const char *path, char *const argv[], char *const envp[]) {
  char a[600]; argv_line(argv, a, sizeof a);
  emit("exec", path, a);
  return execve(path, argv, envp);
}
INTERPOSE(my_execve, execve)

static pid_t my_fork(void) {
  emit("fork", "", "");
  return fork();
}
INTERPOSE(my_fork, fork)

static int my_open(const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = (mode_t)va_arg(ap, int); va_end(ap); }
  emit((flags & (O_WRONLY | O_RDWR)) ? "openw" : "open", path, "");
  return open(path, flags, mode);
}
INTERPOSE(my_open, open)

static int my_openat(int fd, const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = (mode_t)va_arg(ap, int); va_end(ap); }
  emit((flags & (O_WRONLY | O_RDWR)) ? "openw" : "open", path, "at");
  return openat(fd, path, flags, mode);
}
INTERPOSE(my_openat, openat)

static int my_stat(const char *path, struct stat *buf) { emit("stat", path, ""); return stat(path, buf); }
INTERPOSE(my_stat, stat)
static int my_lstat(const char *path, struct stat *buf) { emit("stat", path, "l"); return lstat(path, buf); }
INTERPOSE(my_lstat, lstat)
static int my_fstatat(int fd, const char *path, struct stat *buf, int flag) { emit("stat", path, "at"); return fstatat(fd, path, buf, flag); }
INTERPOSE(my_fstatat, fstatat)
static int my_access(const char *path, int mode) { emit("stat", path, "access"); return access(path, mode); }
INTERPOSE(my_access, access)

static int my_rename(const char *from, const char *to) { emit("rename", to, ""); return rename(from, to); }
INTERPOSE(my_rename, rename)
static int my_fsync(int fd) { emit("fsync", "", ""); return fsync(fd); }
INTERPOSE(my_fsync, fsync)

static int my_connect(int fd, const struct sockaddr *addr, socklen_t len) {
  if (addr && addr->sa_family == AF_UNIX) emit("connect", ((const struct sockaddr_un *)addr)->sun_path, "");
  else emit("connect", "inet", "");
  return connect(fd, addr, len);
}
INTERPOSE(my_connect, connect)
