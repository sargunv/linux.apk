// SPDX-License-Identifier: AGPL-3.0-or-later
// LD_PRELOADed into a test program by tests/bionic/run.sh when it sets
// read-only properties, which the session's property service has already
// seeded. Test only; never installed.
//  - __system_property_*: a per-process store, seeded with
//    servicemanager.ready=true (libbinder's defaultServiceManager() waits
//    for it) and INNKEEPER_PROPS="name=value;name=value".
//  - __android_log_write_log_message: mirror logs to stderr, the runner's
//    output.
#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define EXPORT __attribute__((visibility("default")))

struct __android_log_message {
  size_t struct_size;
  int32_t buffer_id;
  int32_t priority;
  const char* tag;
  const char* file;
  uint32_t line;
  const char* message;
};
EXPORT void __android_log_write_log_message(struct __android_log_message* m) {
  static const char kPrio[] = "??VDIWEF";
  fprintf(
    stderr, "%c %s: %s\n",
    m->priority >= 0 && m->priority < 8 ? kPrio[m->priority] : '?',
    m->tag ? m->tag : "", m->message
  );
}

// ---- properties ----
#define PROP_VALUE_MAX 92
typedef struct prop_info {
  char name[128];
  char value[PROP_VALUE_MAX];
} prop_info;

static prop_info gProps[128];
static int gCount;
static pthread_mutex_t gLock = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t gOnce = PTHREAD_ONCE_INIT;

static prop_info* findLocked(const char* name) {
  for (int i = 0; i < gCount; i++)
    if (strcmp(gProps[i].name, name) == 0) return &gProps[i];
  return NULL;
}

static void setLocked(const char* name, const char* value) {
  prop_info* p = findLocked(name);
  if (!p) {
    if (gCount == (int)(sizeof gProps / sizeof gProps[0])) return;
    p = &gProps[gCount++];
    snprintf(p->name, sizeof p->name, "%s", name);
  }
  snprintf(p->value, sizeof p->value, "%s", value);
}

static void init(void) {
  setLocked("servicemanager.ready", "true");
  const char* env = getenv("INNKEEPER_PROPS");
  if (!env) return;
  char* copy = strdup(env);
  char* save = NULL;
  for (char* kv = strtok_r(copy, ";", &save); kv;
       kv = strtok_r(NULL, ";", &save)) {
    char* eq = strchr(kv, '=');
    if (!eq) continue;
    *eq = 0;
    setLocked(kv, eq + 1);
  }
  free(copy);
}

EXPORT const prop_info* __system_property_find(const char* name) {
  pthread_once(&gOnce, init);
  pthread_mutex_lock(&gLock);
  prop_info* p = findLocked(name);
  pthread_mutex_unlock(&gLock);
  return p;
}

EXPORT void __system_property_read_callback(
  const prop_info* pi, void (*cb)(void*, const char*, const char*, uint32_t),
  void* cookie
) {
  cb(cookie, pi->name, pi->value, 1);
}

EXPORT int __system_property_read(
  const prop_info* pi, char* name, char* value
) {
  if (name) snprintf(name, 32, "%s", pi->name);
  snprintf(value, PROP_VALUE_MAX, "%s", pi->value);
  return (int)strlen(pi->value);
}

EXPORT int __system_property_get(const char* name, char* value) {
  const prop_info* p = __system_property_find(name);
  value[0] = 0;
  if (!p) return 0;
  snprintf(value, PROP_VALUE_MAX, "%s", p->value);
  return (int)strlen(value);
}

EXPORT int __system_property_set(const char* name, const char* value) {
  pthread_once(&gOnce, init);
  pthread_mutex_lock(&gLock);
  setLocked(name, value);
  pthread_mutex_unlock(&gLock);
  return 0;
}

EXPORT uint32_t __system_property_serial(const prop_info* pi) {
  (void)pi;
  return 1;
}

EXPORT bool __system_property_wait(
  const prop_info* pi, uint32_t old_serial, uint32_t* new_serial,
  const struct timespec* timeout
) {
  (void)pi;
  // Values never change from outside the process: sleep instead of spinning.
  struct timespec ts = {0, 100 * 1000 * 1000};
  if (timeout && timeout->tv_sec == 0 && timeout->tv_nsec < ts.tv_nsec)
    ts = *timeout;
  nanosleep(&ts, NULL);
  if (new_serial) *new_serial = old_serial;
  return false;
}
