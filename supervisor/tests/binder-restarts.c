// Real seccomp binder clients: concurrent open/reuse, one-way ordering, replies
// and signal-interrupted reads. No Android image, devices or privileges needed.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/android/binder.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

extern int innkeeper_filter_install(void);
extern void innkeeper_broker_start(int listener, int pins);

enum { CLIENTS = 8, CALLS = 32, MAP_SIZE = 1024 * 1024 };
struct message {
  uint32_t client, sequence, oneway;
};
static volatile sig_atomic_t interrupted;
static pid_t children[CLIENTS + 1];
static atomic_int stop_signals;

static void fail(const char* what) {
  perror(what);
  _exit(1);
}
static void signal_handler(int signal) { (void)signal; }
static void append(
  unsigned char* buf, size_t* len, uint32_t cmd, const void* data
) {
  memcpy(buf + *len, &cmd, sizeof(cmd));
  *len += sizeof(cmd);
  if (_IOC_SIZE(cmd)) {
    memcpy(buf + *len, data, _IOC_SIZE(cmd));
    *len += _IOC_SIZE(cmd);
  }
}
static void exchange(int fd, struct binder_write_read* bwr) {
  while (ioctl(fd, BINDER_WRITE_READ, bwr) < 0) {
    if (errno != EINTR) fail("BINDER_WRITE_READ");
    interrupted++;
    // Keep the driver's write_consumed across restart: never replay commands.
  }
}
static int binder_open(void** mapping) {
  int fd;
  do {
    fd = open("/dev/binder", O_RDWR | O_CLOEXEC);
  } while (fd < 0 && errno == EINTR);
  if (fd < 0) fail("binder open");
  *mapping = mmap(NULL, MAP_SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
  if (*mapping == MAP_FAILED) fail("binder mmap");
  return fd;
}
static void free_buffer(
  unsigned char* commands, size_t* count, binder_uintptr_t ptr
) {
  append(commands, count, BC_FREE_BUFFER, &ptr);
}
static void server(int ready, int done) {
  void* mapping;
  int fd = binder_open(&mapping);
  int zero = 0;
  if (ioctl(fd, BINDER_SET_CONTEXT_MGR, &zero)) fail("context manager");
  if (write(ready, "r", 1) != 1) fail("ready");
  unsigned char commands[4096], output[1024];
  struct message replies[16];
  size_t count = 0;
  unsigned seen[CLIENTS] = {0};
  append(commands, &count, BC_ENTER_LOOPER, NULL);
  unsigned received = 0;
  while (received < CLIENTS * CALLS * 2) {
    struct binder_write_read bwr = {
      .write_size = count,
      .write_buffer = (uintptr_t)commands,
      .read_size = sizeof(output),
      .read_buffer = (uintptr_t)output,
    };
    exchange(fd, &bwr);
    count = 0;
    size_t reply_count = 0;
    for (size_t off = 0; off < bwr.read_consumed;) {
      uint32_t cmd;
      memcpy(&cmd, output + off, 4);
      off += 4;
      if (cmd == BR_TRANSACTION) {
        struct binder_transaction_data td;
        memcpy(&td, output + off, sizeof(td));
        if (td.data_size != sizeof(struct message) || td.offsets_size)
          fail("transaction size");
        struct message m;
        memcpy(&m, (void*)(uintptr_t)td.data.ptr.buffer, sizeof(m));
        if (m.client >= CLIENTS || m.sequence >= CALLS)
          fail("transaction data");
        if (m.oneway) {
          if (!(td.flags & TF_ONE_WAY) || m.sequence != seen[m.client]++)
            fail("oneway order");
        } else {
          if (td.flags & TF_ONE_WAY || reply_count >= 16) fail("sync flags");
          replies[reply_count] = m;
          struct binder_transaction_data reply = {
            .data_size = sizeof(m),
            .data.ptr.buffer = (uintptr_t)&replies[reply_count++],
          };
          append(commands, &count, BC_REPLY, &reply);
        }
        free_buffer(commands, &count, td.data.ptr.buffer);
        received++;
      } else if (
        cmd != BR_NOOP && cmd != BR_TRANSACTION_COMPLETE &&
        cmd != BR_SPAWN_LOOPER
      ) {
        fprintf(stderr, "unexpected server command %#x\n", cmd);
        _exit(1);
      }
      off += _IOC_SIZE(cmd);
    }
  }
  struct binder_write_read flush = {
    .write_size = count, .write_buffer = (uintptr_t)commands
  };
  exchange(fd, &flush);
  for (unsigned i = 0; i < CLIENTS; i++)
    if (seen[i] != CALLS) fail("oneway count");
  sig_atomic_t interrupts = interrupted;
  if (write(done, &interrupts, sizeof(interrupts)) != sizeof(interrupts))
    fail("server done");
  _exit(0);
}
static void client(unsigned id, int done) {
  for (unsigned cycle = 0; cycle < CALLS / 8; cycle++) {
    void* mapping;
    int fd = binder_open(&mapping);
    for (unsigned n = cycle * 8; n < cycle * 8 + 8; n++) {
      struct message messages[2] = {{id, n, 1}, {id, n, 0}};
      unsigned char commands[256], output[1024];
      size_t count = 0;
      for (unsigned i = 0; i < 2; i++) {
        struct binder_transaction_data td = {
          .flags = i ? 0 : TF_ONE_WAY,
          .data_size = sizeof(struct message),
          .data.ptr.buffer = (uintptr_t)&messages[i],
        };
        append(commands, &count, BC_TRANSACTION, &td);
      }
      int replied = 0;
      while (!replied) {
        struct binder_write_read bwr = {
          .write_size = count,
          .write_buffer = (uintptr_t)commands,
          .read_size = sizeof(output),
          .read_buffer = (uintptr_t)output,
        };
        exchange(fd, &bwr);
        count = 0;
        for (size_t off = 0; off < bwr.read_consumed;) {
          uint32_t cmd;
          memcpy(&cmd, output + off, 4);
          off += 4;
          if (cmd == BR_REPLY) {
            struct binder_transaction_data td;
            memcpy(&td, output + off, sizeof(td));
            if (
              td.data_size != sizeof(struct message) ||
              memcmp(
                (void*)(uintptr_t)td.data.ptr.buffer, &messages[1],
                sizeof(struct message)
              )
            )
              fail("reply data");
            if (replied++) fail("duplicate reply");
            free_buffer(commands, &count, td.data.ptr.buffer);
          } else if (cmd != BR_NOOP && cmd != BR_TRANSACTION_COMPLETE) {
            fprintf(stderr, "unexpected client command %#x\n", cmd);
            _exit(1);
          }
          off += _IOC_SIZE(cmd);
        }
      }
      struct binder_write_read flush = {
        .write_size = count, .write_buffer = (uintptr_t)commands
      };
      exchange(fd, &flush);
    }
    munmap(mapping, MAP_SIZE);
    close(fd);  // Next cycle reuses this fd while the monitor checks closures.
  }
  sig_atomic_t interrupts = interrupted;
  if (write(done, &interrupts, sizeof(interrupts)) != sizeof(interrupts))
    fail("client done");
  _exit(0);
}
static void* signal_clients(void* unused) {
  (void)unused;
  for (unsigned burst = 0; burst < 100 && !atomic_load(&stop_signals);
       burst++) {
    for (unsigned i = 0; i <= CLIENTS; i++) kill(children[i], SIGUSR1);
    usleep(500);
  }
  return NULL;
}
static void clients(void) {
  int ready[2], done[2];
  if (pipe(ready) || pipe(done)) fail("pipes");
  struct sigaction sa = {.sa_handler = signal_handler};
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL)) fail("sigaction");
  children[0] = fork();
  if (!children[0]) server(ready[1], done[1]);
  if (children[0] < 0) fail("server fork");
  char byte;
  if (read(ready[0], &byte, 1) != 1) fail("server ready");
  for (unsigned i = 0; i < CLIENTS; i++) {
    children[i + 1] = fork();
    if (!children[i + 1]) client(i, done[1]);
    if (children[i + 1] < 0) fail("client fork");
  }
  pthread_t signaler;
  if (pthread_create(&signaler, NULL, signal_clients, NULL)) fail("signaler");
  unsigned interrupted_total = 0;
  for (unsigned i = 0; i <= CLIENTS; i++) {
    sig_atomic_t count;
    if (read(done[0], &count, sizeof(count)) != sizeof(count))
      fail("completion");
    interrupted_total += count;
  }
  atomic_store(&stop_signals, 1);
  pthread_join(signaler, NULL);
  // Do not reap while the signaler still uses these pids: no PID reuse race.
  for (unsigned i = 0; i <= CLIENTS; i++) {
    int status;
    if (
      waitpid(children[i], &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status)
    )
      fail("child status");
  }
  if (!interrupted_total) fail("no interrupted reads exercised");
  printf(
    "ok   concurrent binder: %u requests, ordered oneways, fd reuse, %u "
    "interrupted ioctls\n",
    CLIENTS * CALLS * 2, interrupted_total
  );
}
int main(void) {
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets)) fail("socketpair");
  pid_t child = fork();
  if (!child) {
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) fail("no_new_privs");
    int fd = innkeeper_filter_install();
    if (fd < 0) fail("filter");
    char control[CMSG_SPACE(sizeof(int))] = {0}, byte = 0;
    struct iovec iov = {&byte, 1};
    struct msghdr msg = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control)
    };
    struct cmsghdr* cm = CMSG_FIRSTHDR(&msg);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &fd, sizeof(fd));
    if (sendmsg(sockets[1], &msg, 0) != 1) fail("send listener");
    close(fd);
    clients();
    return 0;
  }
  if (child < 0) fail("fork");
  char control[CMSG_SPACE(sizeof(int))], byte;
  struct iovec iov = {&byte, 1};
  struct msghdr msg = {
    .msg_iov = &iov,
    .msg_iovlen = 1,
    .msg_control = control,
    .msg_controllen = sizeof(control)
  };
  if (recvmsg(sockets[0], &msg, 0) != 1 || !CMSG_FIRSTHDR(&msg))
    fail("receive listener");
  int fd;
  memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&msg)), sizeof(fd));
  innkeeper_broker_start(fd, -1);
  int status;
  if (waitpid(child, &status, 0) < 0 || !WIFEXITED(status)) return 1;
  return WEXITSTATUS(status);
}
