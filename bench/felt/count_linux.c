// Linux stand-in for count.c (LD_PRELOAD), to smoke-test felt.py only.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <unistd.h>
static int log_fd = -1;
static long long now_ms(void){struct timeval tv;gettimeofday(&tv,NULL);return (long long)tv.tv_sec*1000+tv.tv_usec/1000;}
static void emit(const char*k,const char*a,const char*b){if(log_fd<0)return;int s=errno;char l[1200];int n=snprintf(l,sizeof l,"%lld\t%s\t%s\t%s\n",now_ms(),k,a?a:"",b?b:"");if(n>(int)sizeof l-1){n=sizeof l-1;l[n-1]='\n';}if(n>0)write(log_fd,l,n);errno=s;}
#define REAL(name) static __typeof__(name) *real_##name; if(!real_##name) real_##name = dlsym(RTLD_NEXT, #name)
__attribute__((constructor)) static void start(void){const char*d=getenv("FELT_LOG");if(!d)return;char p[1024];snprintf(p,sizeof p,"%s/%d.log",d,getpid());
 log_fd=syscall(2,p,O_WRONLY|O_CREAT|O_APPEND|O_CLOEXEC,0644);
 char a[900]="";FILE*f=fopen("/proc/self/cmdline","r");if(f){size_t n=fread(a,1,800,f);fclose(f);for(size_t i=0;i<n;i++)if(!a[i])a[i]=' ';a[n]=0;}
 char pp[32];snprintf(pp,sizeof pp,"%d",getppid());emit("start",pp,a);}
static void argv_line(char*const argv[],char*o,size_t s){o[0]=0;for(int i=0;argv&&argv[i]&&i<6;i++){strncat(o,argv[i],s-strlen(o)-2);strcat(o," ");}}
int posix_spawn(pid_t*pid,const char*path,const posix_spawn_file_actions_t*fa,const posix_spawnattr_t*at,char*const argv[],char*const envp[]){REAL(posix_spawn);char a[600];argv_line(argv,a,sizeof a);emit("spawn",path,a);return real_posix_spawn(pid,path,fa,at,argv,envp);}
int posix_spawnp(pid_t*pid,const char*path,const posix_spawn_file_actions_t*fa,const posix_spawnattr_t*at,char*const argv[],char*const envp[]){REAL(posix_spawnp);char a[600];argv_line(argv,a,sizeof a);emit("spawn",path,a);return real_posix_spawnp(pid,path,fa,at,argv,envp);}
int execve(const char*path,char*const argv[],char*const envp[]){REAL(execve);char a[600];argv_line(argv,a,sizeof a);emit("exec",path,a);return real_execve(path,argv,envp);}
pid_t fork(void){REAL(fork);emit("fork","","");return real_fork();}
int open(const char*p,int fl,...){REAL(open);mode_t m=0;if(fl&O_CREAT){va_list ap;va_start(ap,fl);m=va_arg(ap,int);va_end(ap);}emit((fl&(O_WRONLY|O_RDWR))?"openw":"open",p,"");return real_open(p,fl,m);}
int open64(const char*p,int fl,...){REAL(open64);mode_t m=0;if(fl&O_CREAT){va_list ap;va_start(ap,fl);m=va_arg(ap,int);va_end(ap);}emit((fl&(O_WRONLY|O_RDWR))?"openw":"open",p,"");return real_open64(p,fl,m);}
int openat(int d,const char*p,int fl,...){REAL(openat);mode_t m=0;if(fl&O_CREAT){va_list ap;va_start(ap,fl);m=va_arg(ap,int);va_end(ap);}emit((fl&(O_WRONLY|O_RDWR))?"openw":"open",p,"at");return real_openat(d,p,fl,m);}
int openat64(int d,const char*p,int fl,...){REAL(openat64);mode_t m=0;if(fl&O_CREAT){va_list ap;va_start(ap,fl);m=va_arg(ap,int);va_end(ap);}emit((fl&(O_WRONLY|O_RDWR))?"openw":"open",p,"at");return real_openat64(d,p,fl,m);}
int statx(int d,const char*p,int fl,unsigned int mask,struct statx*b){REAL(statx);emit("stat",p,"x");return real_statx(d,p,fl,mask,b);}
int stat(const char*p,struct stat*b){REAL(stat);emit("stat",p,"");return real_stat(p,b);}
int lstat(const char*p,struct stat*b){REAL(lstat);emit("stat",p,"l");return real_lstat(p,b);}
int stat64(const char*p,struct stat64*b){REAL(stat64);emit("stat",p,"");return real_stat64(p,b);}
int access(const char*p,int m){REAL(access);emit("stat",p,"access");return real_access(p,m);}
int rename(const char*a,const char*b){REAL(rename);emit("rename",b,"");return real_rename(a,b);}
int fsync(int fd){REAL(fsync);emit("fsync","","");return real_fsync(fd);}
int fdatasync(int fd){REAL(fdatasync);emit("fsync","","d");return real_fdatasync(fd);}
int connect(int fd,const struct sockaddr*a,socklen_t l){REAL(connect);if(a&&a->sa_family==AF_UNIX)emit("connect",((const struct sockaddr_un*)a)->sun_path,"");else emit("connect","inet","");return real_connect(fd,a,l);}
