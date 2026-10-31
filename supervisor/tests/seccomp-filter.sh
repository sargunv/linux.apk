#!/usr/bin/env bash
# The session's seccomp filter (broker/src/filter.rs) traps exactly what the
# broker serves, and nothing hot besides: bpf, openat, binder ioctls, the
# credential, filesystem and capability calls; open(2), access-token and ashmem
# ioctls for an OpenHarmony guest only.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$here/../../out/test/supervisor/seccomp-filter
mkdir -p "$out"
cargo build -q -p innkeeper-broker --example supervisor-probes
cc -O1 -Wall -Werror -o "$out/t" "$here/seccomp-filter.c" "$here/../../out/cargo/debug/examples/libsupervisor_probes.a" -lpthread -ldl -lm
fail=0
# Name, the probes expected to be notified, then environment.
check() {
  local mode=$1 want=$2 got
  got=$(env "${@:3}" "$out/t" | awk '$NF == 1 { $NF = ""; sub(/ $/, ""); print }' | paste -sd, -)
  if [[ $got == "$want" ]]; then echo "ok   $mode: $got"; else
    echo "FAIL $mode: notified [$got], want [$want]"
    fail=1
  fi
}
rest="getuid,getgroups,setresuid,setgroups,fchownat,getsockopt SO_PEERCRED,lstat,access W_OK,fstat,readlinkat,getcwd,setsockopt SO_MARK,capget,capset,mkdirat,fchmodat,prctl PR_CAPBSET_DROP"
check "Android" "bpf,openat,ioctl binder,$rest"
check "OpenHarmony" "bpf,openat,ioctl binder,open,ioctl access token,ioctl ashmem,$rest" INNKEEPER_GUEST=ohos
exit $fail
