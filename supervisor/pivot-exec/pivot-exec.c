// pivot-exec NEWROOT PROG [ARGS...]
//
// Starts Android's init (PROG) under the session's seccomp filter. A child
// execs `$INNKEEPER_BROKER --init`, which installs the filter, hands its
// listener back and execs PROG; this process then execs the broker to serve
// that listener as init's parent. Built static with the NDK: it runs before
// Android's linker exists.
//
// NEWROOT must be "/": the sandbox forbids mounts and namespaces, and the
// launcher already built Android's root out of symlinks in the sandbox's "/".
// Init is a plain child and this process its subreaper.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

extern char** environ;

static const char* broker;

static void die(const char* what) {
  fprintf(stderr, "pivot-exec: %s: %s\n", what, strerror(errno));
  _exit(1);
}

static void exec_broker(int pins, int listener, pid_t init) {
  // Init's pid, which the broker shows as 1.
  char pid[16];
  snprintf(pid, sizeof(pid), "%d", init);
  setenv("INNKEEPER_INIT_PID", pid, 1);
  // Inherited fds must survive exec; clear CLOEXEC.
  if (pins >= 0) fcntl(pins, F_SETFD, 0);
  fcntl(listener, F_SETFD, 0);
  char pinbuf[16], fdbuf[16], initbuf[16];
  snprintf(pinbuf, sizeof(pinbuf), "%d", pins);
  snprintf(fdbuf, sizeof(fdbuf), "%d", listener);
  snprintf(initbuf, sizeof(initbuf), "%d", init);
  // Init inherited the shim preload; the supervisor must not load it.
  unsetenv("LD_PRELOAD");
  char* argv[] = {(char*)broker, fdbuf, pinbuf, initbuf, NULL};
  execve(broker, argv, environ);
  die("exec broker");
}

static char** child_argv;
static int sync_sock[2];  // [0] child end, [1] parent end

static void child_main(void) {
  close(sync_sock[1]);

  // The shim's cue that this process is init (it isn't pid 1 here): init
  // keeps the shim, its services don't inherit it.
  setenv("INNKEEPER_SHIM_INIT", "1", 1);

  fcntl(sync_sock[0], F_SETFD, 0);
  char syncbuf[16];
  snprintf(syncbuf, sizeof(syncbuf), "%d", sync_sock[0]);
  int n = 0;
  while (child_argv[n]) n++;
  char** args = calloc(n + 4, sizeof(char*));
  if (!args) die("calloc broker argv");
  args[0] = (char*)broker;
  args[1] = "--init";
  args[2] = syncbuf;
  memcpy(args + 3, child_argv, (n + 1) * sizeof(char*));
  // Init's shim consumes LD_PRELOAD (and its init marker) in its constructor.
  // Do not load it into this intermediate Rust process: restore the preload
  // only in the environment Rust hands to the final Android exec.
  const char* preload = getenv("LD_PRELOAD");
  if (preload) {
    if (setenv("INNKEEPER_INIT_PRELOAD", preload, 1) != 0) die("save preload");
    unsetenv("LD_PRELOAD");
  }
  execve(broker, args, environ);
  die("execve");
}

int main(int argc, char** argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: pivot-exec NEWROOT PROG [ARGS...]\n");
    return 2;
  }
  child_argv = argv + 2;
  broker = getenv("INNKEEPER_BROKER");
  if (broker == NULL || broker[0] == '\0') {
    fprintf(stderr, "pivot-exec: INNKEEPER_BROKER (the broker) is not set\n");
    return 2;
  }
  if (strcmp(argv[1], "/") != 0) {
    fprintf(stderr, "pivot-exec: NEWROOT must be /\n");
    return 2;
  }
  if (chdir("/") != 0) die("chdir /");
  // Orphaned Android processes come back to us, not the sandbox's pid 1.
  if (prctl(PR_SET_CHILD_SUBREAPER, 1) != 0) die("subreaper");

  // The session's bpffs tmpfs (the launcher's), for the bpf emulator's pins:
  // opened now, before Android can mount anything over it.
  int pins = open("/sys/fs/bpf", O_PATH | O_DIRECTORY | O_CLOEXEC);
  if (pins < 0)
    fprintf(stderr, "pivot-exec: /sys/fs/bpf: %s\n", strerror(errno));

  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sync_sock) != 0)
    die("socketpair");
  pid_t pid = fork();
  if (pid < 0) die("fork");
  if (pid == 0) child_main();
  close(sync_sock[0]);

  // Receive the child's single seccomp listener.
  char c, cbuf[CMSG_SPACE(sizeof(int))];
  struct iovec iov = {.iov_base = &c, .iov_len = 1};
  struct msghdr msg = {
    .msg_iov = &iov,
    .msg_iovlen = 1,
    .msg_control = cbuf,
    .msg_controllen = sizeof(cbuf)
  };
  if (recvmsg(sync_sock[1], &msg, MSG_CMSG_CLOEXEC) != 1)
    die("receive seccomp listener");
  struct cmsghdr* cm = CMSG_FIRSTHDR(&msg);
  if (!cm || cm->cmsg_type != SCM_RIGHTS) die("no seccomp listener");
  int listener;
  memcpy(&listener, CMSG_DATA(cm), sizeof(int));
  close(sync_sock[1]);

  // Keep this pid: the broker must be Android's ancestor for memory access
  // under Yama ptrace_scope=1. It also takes over reaping.
  exec_broker(pins, listener, pid);
}
