// Exercise actual openat notification dispatch, including the binder fast path.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

extern int innkeeper_filter_install(void);
extern void innkeeper_broker_start(int listener, int pins);

int main(void) {
  int sp[2];
  if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sp)) return 1;
  pid_t child = fork();
  if (child < 0) return 1;
  if (child == 0) {
    alarm(5);
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) _exit(1);
    int listener = innkeeper_filter_install();
    if (listener < 0) _exit(1);
    char control[CMSG_SPACE(sizeof(int))] = {0};
    char byte = 0;
    struct iovec io = {&byte, 1};
    struct msghdr msg = {
      .msg_iov = &io,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control)
    };
    struct cmsghdr* cm = CMSG_FIRSTHDR(&msg);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &listener, sizeof(listener));
    if (sendmsg(sp[1], &msg, 0) != 1) _exit(1);
    close(listener);
    const char* nodes[] = {
      "/dev/dri/renderD128", "/dev/dri/renderD129", "/dev/binder"
    };
    const char* selected = getenv("INNKEEPER_RENDER_NODE");
    for (unsigned i = 0; i < sizeof(nodes) / sizeof(nodes[0]); i++) {
      int fd = open(nodes[i], O_RDWR | O_CLOEXEC);
      int allowed = i == 2 || strcmp(nodes[i], selected) == 0;
      if ((allowed && fd < 0) || (!allowed && (fd >= 0 || errno != EACCES))) {
        fprintf(
          stderr, "FAIL %s selected=%s fd=%d errno=%d\n", nodes[i], selected,
          fd, errno
        );
        _exit(1);
      }
      if (fd >= 0) close(fd);
    }
    _exit(0);
  }
  char control[CMSG_SPACE(sizeof(int))] = {0};
  char byte;
  struct iovec io = {&byte, 1};
  struct msghdr msg = {
    .msg_iov = &io,
    .msg_iovlen = 1,
    .msg_control = control,
    .msg_controllen = sizeof(control)
  };
  alarm(8);
  if (recvmsg(sp[0], &msg, 0) != 1 || !CMSG_FIRSTHDR(&msg)) return 1;
  int listener;
  memcpy(&listener, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof(listener));
  innkeeper_broker_start(listener, -1);
  int status;
  if (waitpid(child, &status, 0) != child) return 1;
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}
