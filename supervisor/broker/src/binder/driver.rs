//! Userspace reimplementation of the Android binder driver's semantics.
//!
//! The driver makes no syscalls itself: process memory, receive buffers, fds
//! and answering the caller go through the [`Userspace`] trait, so the whole
//! driver is exercised by `cargo test` against an in-memory fake (see
//! `tests.rs`). The seccomp binding
//! that wires it to real Android processes lives in `src/notify.rs`.
//!
//! The reference is the kernel's `drivers/android/binder.c`; function names in
//! comments refer to it. The structure deliberately follows it:
//!
//!  * work lists: per-process `todo`, per-thread `todo` with the
//!    `process_todo` flag (deferred work such as the TRANSACTION_COMPLETE of a
//!    synchronous call is queued without it), per-node `async_todo`, and the
//!    per-process `delivered_death` list;
//!  * thread selection: process work goes on the process `todo` and wakes
//!    the longest-waiting thread blocked for it
//!    (`binder_select_thread_ilocked`), else the poll threads; only looper
//!    threads with an empty stack and queue take process work
//!    (`binder_available_for_proc_work`);
//!  * transactions and buffers are separate objects: a buffer outlives its
//!    transaction until `BC_FREE_BUFFER`, and owns the references taken while
//!    translating the objects in it (released by
//!    `binder_transaction_buffer_release`);
//!  * node refcounts (`internal_strong`, `local_strong/weak`, `tmp_refs`, the
//!    set of incoming refs) and the INCREFS/ACQUIRE/RELEASE/DECREFS handshake
//!    with `BC_*_DONE` are the kernel's;
//!  * death notifications move through the same states (DEAD_BINDER,
//!    DEAD_BINDER_AND_CLEAR, CLEAR_DEATH_NOTIFICATION, delivered);
//!  * the freezer: `BINDER_FREEZE` marks every binder instance of a pid
//!    frozen, after waiting up to its timeout for `outstanding_txns` to
//!    drain; a synchronous transaction to a frozen process fails with
//!    BR_FROZEN_REPLY, a oneway is queued and its sender gets
//!    BR_TRANSACTION_PENDING_FROZEN (`binder_proc_transaction`), and freeze
//!    notifications (`binder_ref_freeze`) are sent, coalesced and resent as
//!    in `binder_add_freeze_work`. Reads of a frozen process are served as
//!    usual: its threads stop in the cgroup freezer, which ends their waits.
//!
//! The driver serves whole ioctls ([`BinderDriver::ioctl`]) and owns every
//! thread's place in them. Each `BINDER_WRITE_READ` read goes through one
//! lifecycle:
//!
//!  * **prepared**: the read takes work off its lists and renders the BR_*
//!    stream, but changes nothing else yet (a `Delivery`);
//!  * **blocked**: with nothing to read, the thread waits (`Wait::Blocked`),
//!    holding its unanswered [`Call`]. Work for it, or process work it is
//!    the longest-waiting thread for, wakes it (`Wait::Woken`), and its read
//!    runs again before the ioctl that woke it returns;
//!  * **committed**: once the stream and the bwr are copied out and the
//!    call is answered, the delivery takes effect (transactions pushed on
//!    the stack, buffers handed over, node and death states advanced);
//!  * **rolled back**: if a copy fails (`EFAULT`) or the call is gone (a
//!    signal interrupted it, and the kernel restarts the ioctl), the work goes
//!    back to the front of the lists it came from (`abandon`): thread work
//!    for the thread's restarted read, process work for the next looper;
//!  * **abandoned**: a wait ends without a read when the thread issues
//!    another ioctl (a thread runs one syscall at a time), when it is woken
//!    but its call is gone (a signal stopped it, or it will restart; nothing
//!    is written to it then, unless the signal lands after that check), or
//!    when process release answers it.
//!
//! A `BINDER_FREEZE` that waits for a target to drain is the thread's wait
//! too (`Wait::Freezing`): it ends when the target's last outstanding
//! transaction is freed or at its deadline ([`BinderDriver::expire`]), and
//! a wait abandoned like a read's thaws the target, as the kernel does when
//! a signal interrupts the ioctl.
//!
//! A write is consumed as it is processed, and its progress is written back
//! whatever the read does (before a thread blocks, too), so a restarted
//! ioctl never replays it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::Ordering;

use crate::binder::uapi::*;
use crate::{DEBUG, log};

macro_rules! tlog {
    ($($a:tt)*) => {
        if DEBUG.load(Ordering::Relaxed) {
            log(format_args!("broker: {}", format_args!($($a)*)));
        }
    };
}

pub type ProcId = u32;
pub type NodeId = u64;
pub type TxnId = u64;
type DeathId = u64;
type FreezeId = u64;

/// One binder ioctl being served: the seccomp notification that answers it,
/// and its argument (for `BINDER_WRITE_READ`, the address of the bwr).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub id: u64,
    pub arg: u64,
}

/// Why an fd could not be installed into a reading thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// The reader's call is gone (a signal interrupted it).
    Interrupted,
    /// The reader can't take the fd (for example, its fd table is full).
    Failed,
}

/// The driver's access to the processes it serves (the kernel's
/// `copy_{from,to}_user`, the binder mapping, `fd_install` and returning
/// from the ioctl). The real implementation (`src/notify.rs`) uses each
/// process's pinned `/proc/<pid>/mem`, a per-process memfd, `pidfd_getfd`
/// and seccomp user notifications; tests use [`crate::fake`].
pub trait Userspace {
    /// Read `len` bytes from `proc`'s address space at `addr`.
    fn read(&mut self, proc: ProcId, addr: u64, len: usize) -> Option<Vec<u8>>;
    /// Write `data` into `proc`'s address space at `addr`.
    fn write(&mut self, proc: ProcId, addr: u64, data: &[u8]) -> bool;
    /// Size of `proc`'s mapped receive region (0 if not mapped yet).
    fn recv_size(&mut self, proc: ProcId) -> usize;
    /// Reserve `size` bytes in `proc`'s receive region and return the target
    /// user address of the start, or `None` on ENOSPC.
    fn alloc_recv(&mut self, proc: ProcId, size: usize, is_async: bool) -> Option<u64>;
    /// Write translated transaction bytes into a previously-reserved region.
    fn write_recv(&mut self, proc: ProcId, addr: u64, data: &[u8]);
    /// Release a receive region previously returned by `alloc_recv`.
    fn free_recv(&mut self, proc: ProcId, addr: u64);
    /// Capture `fd` out of `from` into the broker, returning an opaque token,
    /// so the fd survives even if the sender closes it before the target
    /// reads the transaction.
    fn capture_fd(&mut self, from: ProcId, from_fd: u32) -> Option<u64>;
    /// Install a captured fd into `to`, whose thread is reading in `call`,
    /// returning the new fd number. The token stays valid.
    fn install_fd(&mut self, to: ProcId, call: Call, token: u64) -> Result<u32, InstallError>;
    /// Drop a captured fd.
    fn drop_fd(&mut self, token: u64);
    /// End `call` with `errno` (0 for success). False if the call is gone.
    fn answer(&mut self, call: Call, errno: i32) -> bool;
    /// Whether `call` still waits for its answer: false once a signal ended
    /// it (the thread is stopped, or will restart the ioctl).
    fn pending(&mut self, call: Call) -> bool;
    /// One of `proc`'s threads polls its binder fd from now on (`call` is
    /// its write-only `BC_ENTER_LOOPER`): make the fd pollable, once per
    /// process. False if that failed (the call is gone).
    fn make_pollable(&mut self, proc: ProcId, call: Call) -> bool;
    /// Whether a poll of `proc`'s binder fd reports it readable.
    fn set_poll_ready(&mut self, proc: ProcId, ready: bool);
    /// Milliseconds on a monotonic clock (`BINDER_FREEZE` timeouts).
    fn now_ms(&mut self) -> u64;
    /// Call [`BinderDriver::expire`] once [`Userspace::now_ms`] reaches
    /// `at_ms` (or later; an early call is harmless).
    fn arm_timer(&mut self, at_ms: u64);
    /// OpenHarmony: the access tokens thread `tid` of `proc` sends with
    /// (its task's token and first token).
    fn access_token(&mut self, proc: ProcId, tid: u32) -> (u64, u64) {
        let _ = (proc, tid);
        (0, 0)
    }
}

// Looper state flags (kernel BINDER_LOOPER_STATE_*).
const LOOPER_REGISTERED: u32 = 0x01;
const LOOPER_ENTERED: u32 = 0x02;
const LOOPER_EXITED: u32 = 0x04;
const LOOPER_INVALID: u32 = 0x08;
const LOOPER_POLL: u32 = 0x20;

/// Largest write buffer accepted in one `BINDER_WRITE_READ`.
const MAX_WRITE: usize = 16 * 1024 * 1024;

/// Size of the largest single item the read loop emits
/// (`binder_transaction_data_secctx` plus its command word), which the kernel
/// requires free before dequeuing any work.
const MIN_READ_SPACE: usize = 4 + size_of::<BinderTransactionDataSecctx>();

const NO_EXTENDED_ERROR: BinderExtendedError = BinderExtendedError {
    id: 0,
    command: BR_OK,
    param: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Transaction(TxnId),
    TransactionComplete,
    OnewaySpamSuspect,
    /// A oneway's TRANSACTION_COMPLETE when its target is frozen.
    TransactionPendingFrozen,
    ReturnError,
    ReplyError,
    /// A reply whose fds couldn't be installed: BR_FAILED_REPLY in its place.
    FailedReply,
    Node(NodeId),
    Death(DeathId),
    Freeze(FreezeId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeathKind {
    DeadBinder,
    DeadBinderAndClear,
    Clear,
}

struct Death {
    cookie: u64,
    kind: DeathKind,
    /// The work item is on some list (a todo list or `delivered_death`).
    queued: bool,
}

/// A freeze notification on a ref (`binder_ref_freeze`): its work item
/// reports `is_frozen` (BR_FROZEN_BINDER) or, once cleared, that it is gone
/// (BR_CLEAR_FREEZE_NOTIFICATION_DONE).
struct FreezeListener {
    cookie: u64,
    is_frozen: bool,
    /// Detached from its ref by BC_CLEAR_FREEZE_NOTIFICATION.
    cleared: bool,
    /// Delivered, waiting for BC_FREEZE_NOTIFICATION_DONE.
    sent: bool,
    /// Queue it again once the delivery is acknowledged.
    resend: bool,
    /// The work item is on the process's `todo` or `delivered_freeze`.
    queued: bool,
}

/// A `BINDER_FREEZE` in progress: the thread waits for `targets[0]` to
/// drain (`binder_ioctl_freeze`), then freezes the rest in turn.
#[derive(Clone, Debug)]
struct Freezing {
    call: Call,
    enable: bool,
    timeout_ms: u32,
    /// The binder instances of the pid still to freeze, in order.
    targets: Vec<ProcId>,
    /// `targets[0]` is marked frozen and waits until this time.
    deadline: Option<u64>,
}

#[derive(Default)]
struct Thread {
    looper: u32,
    todo: VecDeque<Work>,
    process_todo: bool,
    transaction_stack: Option<TxnId>,
    return_error: u32,
    reply_error: u32,
    wait: Wait,
    ee: BinderExtendedError,
    /// OpenHarmony: the tokens and sender pid of the last transaction or
    /// reply this thread read (`thread->tokens`, `thread->sender_pid_nr`).
    sender: BinderSenderInfo,
}

/// A thread's wait in a blocking ioctl (see the module docs).
#[derive(Clone, Debug, Default)]
enum Wait {
    /// Not waiting in the driver.
    #[default]
    None,
    /// Blocked with nothing to read. `proc_work`: it takes process work
    /// (`binder_wait_for_work` on the process's `waiting_threads`); `since`
    /// orders the threads that do, longest-waiting first.
    Blocked {
        read: Read,
        proc_work: bool,
        since: u64,
    },
    /// Work arrived: the read runs again before the current ioctl returns.
    Woken(Read),
    /// In `BINDER_FREEZE`, waiting for its target to drain.
    Freezing(Freezing),
}

/// A `BINDER_WRITE_READ` in its read phase: its call, and its bwr as of the
/// end of the write phase.
#[derive(Clone, Copy, Debug)]
struct Read {
    call: Call,
    bwr: BinderWriteRead,
}

/// The result of preparing a read.
enum Prepared {
    /// Nothing to read, and the caller doesn't block.
    Again,
    /// Nothing to read: the thread blocks.
    Blocked { proc_work: bool },
    /// A BR_* stream to copy out.
    Ready(Delivery),
    /// The call is gone (an fd could not be installed for that reason).
    Interrupted(Delivery),
}

/// What a read took off its lists: the rendered BR_* stream, and the work
/// to commit once the reader has it, or to put back if it never does.
struct Delivery {
    bytes: Vec<u8>,
    taken: Vec<Taken>,
    /// The thread's `process_todo` before the read.
    process_todo: bool,
    /// The stream starts with BR_SPAWN_LOOPER, already counted in
    /// `requested_threads` (until a rollback).
    spawn_looper: bool,
}

/// How a read takes a transaction.
enum TxnRead {
    /// Its BR_* item.
    Rendered(Vec<u8>),
    /// Gone, or failed for good: nothing to read.
    Dropped,
    /// A reply that failed for good: the reader gets BR_FAILED_REPLY.
    FailedReply,
    /// Its fds can't be installed: it fails as the first item of the next
    /// read. Unlike the kernel, which installs all of a transaction's fds or
    /// none, fds already installed stay in the reader, and leak if the
    /// transaction then fails for good (an fd installed with ADDFD can't be
    /// taken back).
    NotFirst,
    /// The reader's call is gone.
    Interrupted,
}

/// What a node's work item reports to its owner, and whether the node is
/// freed after.
struct NodeChanges {
    incref: bool,
    acquire: bool,
    release: bool,
    decref: bool,
    free: bool,
}

impl NodeChanges {
    fn reports(&self) -> bool {
        self.incref || self.acquire || self.release || self.decref
    }
}

#[derive(Clone, Copy)]
struct Taken {
    work: Work,
    /// From the process's list rather than the thread's.
    from_proc: bool,
}

struct Node {
    /// Owner, `None` once the owner died (`node->proc == NULL`).
    proc: Option<ProcId>,
    ptr: u64,
    cookie: u64,
    internal_strong: i32,
    local_strong: i32,
    local_weak: i32,
    tmp_refs: i32,
    has_strong_ref: bool,
    has_weak_ref: bool,
    pending_strong: bool,
    pending_weak: bool,
    accept_fds: bool,
    txn_security_ctx: bool,
    has_async_transaction: bool,
    async_todo: VecDeque<TxnId>,
    /// Processes holding a ref (a process holds at most one per node).
    refs: BTreeSet<ProcId>,
    /// The node's work item is queued on some list.
    work_queued: bool,
}

struct Ref {
    node: NodeId,
    strong: i32,
    weak: i32,
    death: Option<DeathId>,
    freeze: Option<FreezeId>,
}

#[derive(Default)]
struct Process {
    /// Reported as sender_pid.
    pid: i32,
    euid: u32,
    context: u8,
    max_threads: u32,
    requested_threads: u32,
    requested_threads_started: u32,
    oneway_spam_detection: bool,
    threads: BTreeMap<u32, Thread>,
    /// The binder fd was opened `O_NONBLOCK`: reads never block.
    nonblock: bool,
    todo: VecDeque<Work>,
    nodes: HashMap<u64, NodeId>,
    refs_by_desc: BTreeMap<u32, Ref>,
    node_to_desc: HashMap<NodeId, u32>,
    delivered_death: Vec<DeathId>,
    delivered_freeze: Vec<FreezeId>,
    free_async_space: i64,
    /// `BINDER_FREEZE`: synchronous transactions to it fail.
    is_frozen: bool,
    /// A synchronous or oneway transaction arrived while frozen.
    sync_recv: bool,
    async_recv: bool,
    /// Transactions queued for it or being served by it, which a freeze
    /// waits for (`outstanding_txns`).
    outstanding_txns: u32,
}

struct Context {
    mgr_node: Option<NodeId>,
    mgr_uid: Option<u32>,
}

struct Txn {
    /// Synchronous caller (`t->from`); `None` for oneway, replies, and once
    /// the caller thread died.
    from: Option<(ProcId, u32)>,
    from_parent: Option<TxnId>,
    to_proc: Option<ProcId>,
    to_thread: Option<u32>,
    to_parent: Option<TxnId>,
    buffer: Option<(ProcId, u64)>,
    code: u32,
    flags: u32,
    sender_euid: u32,
    /// OpenHarmony: the sender's (token, first token).
    tokens: (u64, u64),
    /// The process whose `outstanding_txns` counts it: its target, from
    /// being queued until it is freed or its reader thread exits.
    outstanding: Option<ProcId>,
}

#[derive(Clone, Copy)]
enum Obj {
    /// A node local to the buffer owner (BINDER_TYPE_(WEAK_)BINDER).
    Node { node: NodeId, strong: bool },
    /// A handle in the buffer owner's table (BINDER_TYPE_(WEAK_)HANDLE).
    Handle { desc: u32, strong: bool },
}

struct Buffer {
    data_size: usize,
    offsets_size: usize,
    total: usize,
    is_async: bool,
    target_node: Option<NodeId>,
    transaction: Option<TxnId>,
    allow_user_free: bool,
    objects: Vec<Obj>,
    /// Captured fds still to install: (offset of the fd in the buffer, token).
    fd_fixups: Vec<(usize, u64)>,
    /// Address of the security context in the buffer (0: none).
    secctx: u64,
}

pub struct BinderDriver {
    contexts: [Context; 3],
    procs: HashMap<ProcId, Process>,
    nodes: HashMap<NodeId, Node>,
    txns: HashMap<TxnId, Txn>,
    buffers: HashMap<(ProcId, u64), Buffer>,
    deaths: HashMap<DeathId, Death>,
    freezes: HashMap<FreezeId, FreezeListener>,
    next_proc: ProcId,
    next_id: u64,
    /// Threads woken during the current ioctl, in wake order.
    woken: VecDeque<(ProcId, u32)>,
    /// Processes whose poll readiness may have changed.
    poll_dirty: BTreeSet<ProcId>,
    /// Frozen processes whose last outstanding transaction went away: a
    /// freeze may be waiting for that.
    drained: BTreeSet<ProcId>,
    /// Serve OpenHarmony's ioctls and record its tokens.
    ohos: bool,
}

impl Default for BinderDriver {
    fn default() -> Self {
        Self::new()
    }
}

fn ctx_new() -> Context {
    Context {
        mgr_node: None,
        mgr_uid: None,
    }
}

impl BinderDriver {
    pub fn new() -> Self {
        Self::with_guest(crate::Guest::Android)
    }

    /// The driver as `guest`'s kernel has it: Android's, or Android's plus
    /// OpenHarmony's extensions.
    pub fn with_guest(guest: crate::Guest) -> Self {
        BinderDriver {
            contexts: [ctx_new(), ctx_new(), ctx_new()],
            procs: HashMap::new(),
            nodes: HashMap::new(),
            txns: HashMap::new(),
            buffers: HashMap::new(),
            deaths: HashMap::new(),
            freezes: HashMap::new(),
            next_proc: 1,
            next_id: 1,
            woken: VecDeque::new(),
            poll_dirty: BTreeSet::new(),
            drained: BTreeSet::new(),
            ohos: guest == crate::Guest::Ohos,
        }
    }

    fn id(&mut self) -> u64 {
        let i = self.next_id;
        self.next_id += 1;
        i
    }

    /// Open a binder device for process `pid` with `euid`; `context` is 0=binder, 1=hwbinder,
    /// 2=vndbinder.
    pub fn open(&mut self, pid: i32, euid: u32, context: u8, nonblock: bool) -> ProcId {
        let id = self.next_proc;
        self.next_proc += 1;
        self.procs.insert(
            id,
            Process {
                pid,
                euid,
                context: context.min(2),
                nonblock,
                free_async_space: -1, // set from the mapping on first use
                ..Default::default()
            },
        );
        id
    }

    pub fn proc_exists(&self, proc: ProcId) -> bool {
        self.procs.contains_key(&proc)
    }

    /// `BINDER_SET_CONTEXT_MGR(_EXT)`. `flat` is the optional flat_binder_object
    /// (EXT form); pass `None` for the plain form.
    fn set_context_mgr(&mut self, proc: ProcId, flat: Option<FlatBinderObject>) -> i32 {
        let p = &self.procs[&proc];
        let (ctx, euid) = (p.context as usize, p.euid);
        if self.contexts[ctx].mgr_node.is_some() {
            return libc::EBUSY;
        }
        match self.contexts[ctx].mgr_uid {
            Some(want) if want != euid => return libc::EPERM,
            Some(_) => {}
            None => self.contexts[ctx].mgr_uid = Some(euid),
        }
        let (ptr, cookie, flags) = match flat {
            Some(f) => (f.binder_or_handle, f.cookie, f.flags),
            None => (0, 0, 0),
        };
        let nid = self.get_or_new_node(proc, ptr, cookie, flags);
        let n = self.nodes.get_mut(&nid).unwrap();
        n.local_weak += 1;
        n.local_strong += 1;
        n.has_strong_ref = true;
        n.has_weak_ref = true;
        self.contexts[ctx].mgr_node = Some(nid);
        0
    }

    fn get_or_new_node(&mut self, proc: ProcId, ptr: u64, cookie: u64, flags: u32) -> NodeId {
        if let Some(&nid) = self.procs[&proc].nodes.get(&ptr) {
            return nid;
        }
        let id = self.id();
        self.nodes.insert(
            id,
            Node {
                proc: Some(proc),
                ptr,
                cookie,
                internal_strong: 0,
                local_strong: 0,
                local_weak: 0,
                tmp_refs: 0,
                has_strong_ref: false,
                has_weak_ref: false,
                pending_strong: false,
                pending_weak: false,
                accept_fds: flags & FLAT_BINDER_FLAG_ACCEPTS_FDS != 0,
                txn_security_ctx: flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX != 0,
                has_async_transaction: false,
                async_todo: VecDeque::new(),
                refs: BTreeSet::new(),
                work_queued: false,
            },
        );
        self.procs.get_mut(&proc).unwrap().nodes.insert(ptr, id);
        id
    }

    // ---- threads, queues and wakeups ---------------------------------------

    fn thread_mut(&mut self, proc: ProcId, tid: u32) -> &mut Thread {
        self.procs
            .get_mut(&proc)
            .unwrap()
            .threads
            .entry(tid)
            .or_insert_with(|| Thread {
                return_error: BR_OK,
                reply_error: BR_OK,
                ee: NO_EXTENDED_ERROR,
                ..Default::default()
            })
    }

    fn available_for_proc_work(t: &Thread) -> bool {
        t.transaction_stack.is_none()
            && t.todo.is_empty()
            && t.looper & (LOOPER_ENTERED | LOOPER_REGISTERED) != 0
    }

    fn has_work(p: &Process, t: &Thread, do_proc_work: bool) -> bool {
        t.process_todo || (do_proc_work && !p.todo.is_empty())
    }

    /// Wake a blocked thread: its read runs again before the current ioctl
    /// returns (see `settle`).
    fn wake_thread(&mut self, proc: ProcId, tid: u32) {
        if let Some(t) = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.threads.get_mut(&tid))
            && let Wait::Blocked { read, .. } = t.wait
        {
            t.wait = Wait::Woken(read);
            self.woken.push_back((proc, tid));
        }
        self.poll_dirty.insert(proc);
    }

    /// `binder_select_thread_ilocked`: the thread that has waited longest
    /// for process work.
    fn select_thread(&self, proc: ProcId) -> Option<u32> {
        self.procs
            .get(&proc)?
            .threads
            .iter()
            .filter_map(|(&tid, t)| match t.wait {
                Wait::Blocked {
                    proc_work: true,
                    since,
                    ..
                } => Some((since, tid)),
                _ => None,
            })
            .min()
            .map(|(_, tid)| tid)
    }

    /// `binder_wakeup_proc_ilocked`: wake one waiting thread, else the poll
    /// threads.
    fn wakeup_proc(&mut self, proc: ProcId) {
        if let Some(tid) = self.select_thread(proc) {
            self.wake_thread(proc, tid);
        }
        self.poll_dirty.insert(proc);
    }

    /// `binder_enqueue_thread_work_ilocked`.
    fn enqueue_thread_work(&mut self, proc: ProcId, tid: u32, w: Work) {
        let t = self.thread_mut(proc, tid);
        t.todo.push_back(w);
        t.process_todo = true;
        self.wake_thread(proc, tid);
    }

    /// `binder_enqueue_deferred_thread_work_ilocked`: queued, but the thread
    /// does not return to userspace for it alone.
    fn enqueue_deferred_thread_work(&mut self, proc: ProcId, tid: u32, w: Work) {
        self.thread_mut(proc, tid).todo.push_back(w);
    }

    fn enqueue_proc_work(&mut self, proc: ProcId, w: Work) {
        if let Some(p) = self.procs.get_mut(&proc) {
            p.todo.push_back(w);
        }
        self.wakeup_proc(proc);
    }

    /// Remove a work item from every list of `proc` (`binder_dequeue_work`).
    fn dequeue_work(&mut self, proc: ProcId, w: Work) {
        if let Some(p) = self.procs.get_mut(&proc) {
            p.todo.retain(|x| *x != w);
            for t in p.threads.values_mut() {
                t.todo.retain(|x| *x != w);
                if t.todo.is_empty() {
                    t.process_todo = false;
                }
            }
            match w {
                Work::Death(d) => p.delivered_death.retain(|x| *x != d),
                Work::Freeze(f) => p.delivered_freeze.retain(|x| *x != f),
                _ => {}
            }
        }
    }

    /// Would a poll thread of `proc` see work now (`binder_poll`)?
    fn proc_pollable(&self, proc: ProcId) -> bool {
        let Some(p) = self.procs.get(&proc) else {
            return false;
        };
        p.threads.values().any(|t| {
            t.looper & LOOPER_POLL != 0 && Self::has_work(p, t, Self::available_for_proc_work(t))
        })
    }

    // ---- node refcounts ----------------------------------------------------

    fn node_wants(n: &Node) -> (bool, bool) {
        let strong = n.internal_strong > 0 || n.local_strong > 0;
        let weak = !n.refs.is_empty() || n.local_weak > 0 || n.tmp_refs > 0 || strong;
        (strong, weak)
    }

    /// `binder_inc_node_nilocked`. `target` is the thread whose deferred todo
    /// receives the node work (the sender translating its own binder).
    fn inc_node(
        &mut self,
        nid: NodeId,
        strong: bool,
        internal: bool,
        target: Option<(ProcId, u32)>,
    ) {
        let Some(n) = self.nodes.get_mut(&nid) else {
            return;
        };
        let mut enqueue = false;
        let mut requeue = false;
        if strong {
            if internal {
                n.internal_strong += 1;
            } else {
                n.local_strong += 1;
            }
            if !n.has_strong_ref && target.is_some() {
                requeue = true;
            }
        } else {
            if !internal {
                n.local_weak += 1;
            }
            if !n.has_weak_ref && target.is_some() && !n.work_queued {
                enqueue = true;
            }
        }
        let owner = n.proc;
        if let (Some((tp, tt)), Some(owner)) = (target, owner)
            && (enqueue || requeue)
        {
            if requeue && self.nodes[&nid].work_queued {
                self.dequeue_work(owner, Work::Node(nid));
            }
            self.nodes.get_mut(&nid).unwrap().work_queued = true;
            self.enqueue_deferred_thread_work(tp, tt, Work::Node(nid));
        }
    }

    /// `binder_dec_node_nilocked` (+ freeing). Queues the owner's node work
    /// when the last strong or weak reference goes away.
    fn dec_node(&mut self, nid: NodeId, strong: bool, internal: bool) {
        let Some(n) = self.nodes.get_mut(&nid) else {
            return;
        };
        if strong {
            if internal {
                n.internal_strong -= 1;
            } else {
                n.local_strong -= 1;
            }
            if n.local_strong > 0 || n.internal_strong > 0 {
                return;
            }
        } else {
            if !internal {
                n.local_weak -= 1;
            }
            if n.local_weak > 0 || n.tmp_refs > 0 || !n.refs.is_empty() {
                return;
            }
        }
        self.node_maybe_release(nid);
    }

    fn node_maybe_release(&mut self, nid: NodeId) {
        let Some(n) = self.nodes.get(&nid) else {
            return;
        };
        if let Some(owner) = n.proc
            && (n.has_strong_ref || n.has_weak_ref)
        {
            if !n.work_queued {
                self.nodes.get_mut(&nid).unwrap().work_queued = true;
                self.enqueue_proc_work(owner, Work::Node(nid));
            }
            return;
        }
        let (strong, weak) = Self::node_wants(n);
        if !strong && !weak && n.refs.is_empty() {
            self.free_node(nid);
        }
    }

    fn free_node(&mut self, nid: NodeId) {
        if let Some(n) = self.nodes.remove(&nid) {
            if let Some(owner) = n.proc {
                if n.work_queued {
                    self.dequeue_work(owner, Work::Node(nid));
                }
                if let Some(p) = self.procs.get_mut(&owner)
                    && p.nodes.get(&n.ptr) == Some(&nid)
                {
                    p.nodes.remove(&n.ptr);
                }
            }
            for c in self.contexts.iter_mut() {
                if c.mgr_node == Some(nid) {
                    c.mgr_node = None;
                }
            }
        }
    }

    // ---- refs (handles) -----------------------------------------------------

    fn ref_node(&self, proc: ProcId, desc: u32, need_strong: bool) -> Option<NodeId> {
        let r = self.procs.get(&proc)?.refs_by_desc.get(&desc)?;
        if need_strong && r.strong == 0 {
            return None;
        }
        Some(r.node)
    }

    /// `binder_inc_ref_for_node`: get or create `proc`'s ref to `node`, take a
    /// strong or weak count on it, and return its descriptor.
    fn inc_ref_for_node(
        &mut self,
        proc: ProcId,
        nid: NodeId,
        strong: bool,
        target: Option<(ProcId, u32)>,
    ) -> Result<u32, u32> {
        let p = self.procs.get(&proc).ok_or(BR_DEAD_REPLY)?;
        let desc = match p.node_to_desc.get(&nid) {
            Some(&d) => d,
            None => {
                let is_mgr = self.contexts[p.context as usize].mgr_node == Some(nid);
                let mut desc = if is_mgr { 0 } else { 1 };
                for &d in p.refs_by_desc.range(desc..).map(|(d, _)| d) {
                    if d != desc {
                        break;
                    }
                    desc += 1;
                }
                let p = self.procs.get_mut(&proc).unwrap();
                p.refs_by_desc.insert(
                    desc,
                    Ref {
                        node: nid,
                        strong: 0,
                        weak: 0,
                        death: None,
                        freeze: None,
                    },
                );
                p.node_to_desc.insert(nid, desc);
                self.nodes.get_mut(&nid).unwrap().refs.insert(proc);
                desc
            }
        };
        self.inc_ref(proc, desc, strong, target)?;
        Ok(desc)
    }

    /// `binder_inc_ref_olocked`.
    fn inc_ref(
        &mut self,
        proc: ProcId,
        desc: u32,
        strong: bool,
        target: Option<(ProcId, u32)>,
    ) -> Result<(), u32> {
        let r = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.refs_by_desc.get_mut(&desc))
            .ok_or(BR_FAILED_REPLY)?;
        let nid = r.node;
        if strong {
            if r.strong == 0 {
                // binder_inc_node(node, 1, 1, target): a node with no strong
                // refs can only gain one through translation of its owner's
                // object (target != None) or if it is a live context manager.
                let n = &self.nodes[&nid];
                let is_mgr = self.contexts.iter().any(|c| c.mgr_node == Some(nid));
                if target.is_none()
                    && n.internal_strong == 0
                    && !(n.proc.is_some() && is_mgr && n.has_strong_ref)
                {
                    return Err(BR_FAILED_REPLY);
                }
                self.inc_node(nid, true, true, target);
            }
            let r = self
                .procs
                .get_mut(&proc)
                .unwrap()
                .refs_by_desc
                .get_mut(&desc)
                .unwrap();
            r.strong += 1;
        } else {
            if r.weak == 0 {
                self.inc_node(nid, false, true, target);
            }
            let r = self
                .procs
                .get_mut(&proc)
                .unwrap()
                .refs_by_desc
                .get_mut(&desc)
                .unwrap();
            r.weak += 1;
        }
        Ok(())
    }

    /// `binder_dec_ref_olocked` + `binder_cleanup_ref_olocked` when it hits 0.
    fn dec_ref(&mut self, proc: ProcId, desc: u32, strong: bool) -> Result<(), i32> {
        let r = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.refs_by_desc.get_mut(&desc))
            .ok_or(-libc::EINVAL)?;
        let nid = r.node;
        if strong {
            if r.strong == 0 {
                return Err(-libc::EINVAL);
            }
            r.strong -= 1;
            if r.strong == 0 {
                self.dec_node(nid, true, true);
            }
        } else {
            if r.weak == 0 {
                return Err(-libc::EINVAL);
            }
            r.weak -= 1;
        }
        let r = &self.procs[&proc].refs_by_desc[&desc];
        if r.strong == 0 && r.weak == 0 {
            self.delete_ref(proc, desc);
        }
        Ok(())
    }

    fn delete_ref(&mut self, proc: ProcId, desc: u32) {
        let Some(p) = self.procs.get_mut(&proc) else {
            return;
        };
        let Some(r) = p.refs_by_desc.remove(&desc) else {
            return;
        };
        p.node_to_desc.remove(&r.node);
        if r.strong > 0 {
            self.dec_node(r.node, true, true);
        }
        if let Some(n) = self.nodes.get_mut(&r.node) {
            n.refs.remove(&proc);
        }
        self.dec_node(r.node, false, true);
        if let Some(d) = r.death {
            self.dequeue_work(proc, Work::Death(d));
            self.deaths.remove(&d);
        }
        if let Some(f) = r.freeze {
            self.dequeue_work(proc, Work::Freeze(f));
            self.freezes.remove(&f);
        }
    }

    // ---- ioctls and the read lifecycle -----------------------------------------

    /// Serve binder ioctl `cmd` from thread `tid` of `proc` (`binder_ioctl`)
    /// and answer `call`: now, or for a blocking read once there is work.
    /// Threads it wakes are served before it returns.
    pub fn ioctl(&mut self, proc: ProcId, tid: u32, cmd: u32, call: Call, us: &mut dyn Userspace) {
        if !self.proc_exists(proc) {
            us.answer(call, libc::EINVAL);
            return;
        }
        // A thread runs one syscall at a time, so any wait it was in is over
        // (a signal interrupted it, and this may be its restart).
        let t = self.thread_mut(proc, tid);
        match std::mem::take(&mut t.wait) {
            Wait::Blocked { .. } => tlog!("proc={proc} tid={tid} wait abandoned"),
            Wait::Freezing(f) => self.freeze_interrupted(&f),
            _ => {}
        }
        if cmd == BINDER_WRITE_READ {
            self.write_read(proc, tid, call, us);
        } else if cmd == BINDER_FREEZE {
            self.freeze_ioctl(proc, tid, call, us);
        } else {
            let errno = self.other_ioctl(proc, tid, cmd, call.arg, us);
            us.answer(call, errno);
        }
        self.settle(us);
    }

    /// The ioctls other than `BINDER_WRITE_READ`. Returns an errno.
    fn other_ioctl(
        &mut self,
        proc: ProcId,
        tid: u32,
        cmd: u32,
        arg: u64,
        us: &mut dyn Userspace,
    ) -> i32 {
        fn get<T: Copy>(us: &mut dyn Userspace, proc: ProcId, arg: u64) -> Option<T> {
            us.read(proc, arg, size_of::<T>())
                .and_then(|b| read_pod(&b, 0))
        }
        fn put<T: Copy>(us: &mut dyn Userspace, proc: ProcId, arg: u64, v: &T) -> i32 {
            if us.write(proc, arg, pod_bytes(v)) {
                0
            } else {
                libc::EFAULT
            }
        }
        let p = self.procs.get_mut(&proc).unwrap();
        match cmd {
            BINDER_VERSION => {
                let v = BinderVersion {
                    protocol_version: BINDER_CURRENT_PROTOCOL_VERSION,
                };
                put(us, proc, arg, &v)
            }
            BINDER_SET_MAX_THREADS => match get::<u32>(us, proc, arg) {
                Some(n) => {
                    p.max_threads = n;
                    0
                }
                None => libc::EFAULT,
            },
            BINDER_ENABLE_ONEWAY_SPAM_DETECTION => match get::<u32>(us, proc, arg) {
                Some(on) => {
                    p.oneway_spam_detection = on != 0;
                    0
                }
                None => libc::EFAULT,
            },
            BINDER_SET_CONTEXT_MGR => self.set_context_mgr(proc, None),
            BINDER_SET_CONTEXT_MGR_EXT => match get::<FlatBinderObject>(us, proc, arg) {
                Some(f) => self.set_context_mgr(proc, Some(f)),
                None => libc::EFAULT,
            },
            BINDER_THREAD_EXIT => {
                self.thread_exit(proc, tid);
                0
            }
            BINDER_GET_EXTENDED_ERROR => {
                let t = self.thread_mut(proc, tid);
                let ee = std::mem::replace(&mut t.ee, NO_EXTENDED_ERROR);
                put(us, proc, arg, &ee)
            }
            BINDER_GET_NODE_DEBUG_INFO => match get::<BinderNodeDebugInfo>(us, proc, arg) {
                Some(info) => put(us, proc, arg, &self.node_debug_info(proc, info.ptr)),
                None => libc::EFAULT,
            },
            BINDER_GET_NODE_INFO_FOR_REF => match get::<BinderNodeInfoForRef>(us, proc, arg) {
                Some(info) => match self.node_info_for_ref(proc, info) {
                    Ok(out) => put(us, proc, arg, &out),
                    Err(e) => e,
                },
                None => libc::EFAULT,
            },
            BINDER_GET_FROZEN_INFO => match get::<BinderFrozenStatusInfo>(us, proc, arg) {
                Some(info) => match self.frozen_info(info.pid) {
                    Some(out) => put(us, proc, arg, &out),
                    None => libc::EINVAL,
                },
                None => libc::EFAULT,
            },
            BINDER_FEATURE_SET if self.ohos => put(us, proc, arg, &OHOS_FEATURES),
            BINDER_GET_ACCESS_TOKEN if self.ohos => {
                let tokens = self.thread_mut(proc, tid).sender.tokens;
                put(us, proc, arg, &tokens)
            }
            BINDER_GET_SENDER_INFO if self.ohos => {
                let sender = self.thread_mut(proc, tid).sender;
                put(us, proc, arg, &sender)
            }
            _ => libc::EINVAL,
        }
    }

    /// `binder_ioctl_write_read`: the write phase, then the read phase.
    fn write_read(&mut self, proc: ProcId, tid: u32, call: Call, us: &mut dyn Userspace) {
        let Some(mut bwr) = us
            .read(proc, call.arg, size_of::<BinderWriteRead>())
            .and_then(|b| read_pod::<BinderWriteRead>(&b, 0))
        else {
            us.answer(call, libc::EFAULT);
            return;
        };
        let read_avail = bwr.read_size.saturating_sub(bwr.read_consumed);
        if bwr.write_size > bwr.write_consumed {
            let len = (bwr.write_size - bwr.write_consumed) as usize;
            if len > MAX_WRITE {
                us.answer(call, libc::EINVAL);
                return;
            }
            let Some(write) = bwr
                .write_buffer
                .checked_add(bwr.write_consumed)
                .and_then(|a| us.read(proc, a, len))
            else {
                us.answer(call, libc::EFAULT);
                return;
            };
            // A write-only BC_ENTER_LOOPER is IPCThreadState::setupPolling():
            // this thread will epoll the binder fd (kernel: binder_poll marks
            // it BINDER_LOOPER_STATE_POLL). The fd is made pollable before
            // anything is consumed, so if that fails, the restarted ioctl (or
            // libbinder's retry on EINTR) does it all again.
            if read_avail == 0
                && self.thread_mut(proc, tid).looper & LOOPER_POLL == 0
                && has_cmd(&write, BC_ENTER_LOOPER)
            {
                if !us.make_pollable(proc, call) {
                    // A failure that persists (the broker out of fds) makes
                    // libbinder retry at once, in a loop; that is accepted.
                    us.answer(call, libc::EINTR);
                    return;
                }
                self.thread_mut(proc, tid).looper |= LOOPER_POLL;
                self.poll_dirty.insert(proc);
            }
            let (consumed, err) = self.thread_write(proc, tid, &write, us);
            bwr.write_consumed += consumed as u64;
            if err != 0 {
                let errno = if put_bwr(proc, call, &bwr, us) {
                    err
                } else {
                    libc::EFAULT
                };
                us.answer(call, errno);
                return;
            }
        }
        // A signal can end the seccomp wait even when NOTIF_SEND succeeds.
        // In that case these read bytes are already committed and published;
        // libbinder retries with their read_consumed intact. Return them before
        // waiting for new work (which can depend on the caller freeing them).
        if read_avail > 0 && bwr.read_consumed == 0 {
            let nonblock = self.procs[&proc].nonblock;
            self.serve_read(proc, tid, Read { call, bwr }, nonblock, us);
        } else {
            let errno = if put_bwr(proc, call, &bwr, us) {
                0
            } else {
                libc::EFAULT
            };
            us.answer(call, errno);
        }
    }

    /// The read phase: deliver what there is to `tid`, or block it.
    fn serve_read(
        &mut self,
        proc: ProcId,
        tid: u32,
        read: Read,
        nonblock: bool,
        us: &mut dyn Userspace,
    ) {
        let avail = read.bwr.read_size.saturating_sub(read.bwr.read_consumed) as usize;
        match self.prepare_read(proc, tid, avail, nonblock, read.call, us) {
            Prepared::Ready(d) => self.deliver(proc, tid, read, d, us),
            Prepared::Blocked { proc_work } => {
                // Record the write's progress first, so a restarted ioctl never
                // replays it. A thread waits only once that has worked.
                if !put_bwr(proc, read.call, &read.bwr, us) {
                    us.answer(read.call, libc::EFAULT);
                    return;
                }
                let since = self.id();
                self.thread_mut(proc, tid).wait = Wait::Blocked {
                    read,
                    proc_work,
                    since,
                };
            }
            Prepared::Again => {
                let errno = if put_bwr(proc, read.call, &read.bwr, us) {
                    libc::EAGAIN
                } else {
                    libc::EFAULT
                };
                us.answer(read.call, errno);
            }
            Prepared::Interrupted(d) => self.abandon(proc, tid, read, d, us),
        }
        self.pass_on_proc_work(proc);
    }

    /// Process work a read left (it was rolled back, the buffer was full, or
    /// the reader is gone) is for another waiting thread, not for that one to
    /// come back for.
    fn pass_on_proc_work(&mut self, proc: ProcId) {
        if !self.procs[&proc].todo.is_empty() {
            self.wakeup_proc(proc);
        }
    }

    /// Copy a prepared read out and answer its call. The delivery is
    /// committed only if the thread got it, and abandoned otherwise.
    fn deliver(&mut self, proc: ProcId, tid: u32, read: Read, d: Delivery, us: &mut dyn Userspace) {
        let mut bwr = read.bwr;
        bwr.read_consumed += d.bytes.len() as u64;
        let delivered = read
            .bwr
            .read_buffer
            .checked_add(read.bwr.read_consumed)
            .is_some_and(|at| us.write(proc, at, &d.bytes))
            && put_bwr(proc, read.call, &bwr, us)
            && us.answer(read.call, 0);
        if delivered {
            self.commit(proc, tid, d);
        } else {
            self.abandon(proc, tid, read, d, us);
        }
    }

    /// The reader never gets `d`: a copy failed, or its call is gone. Roll it
    /// back, and end the call with EFAULT (a no-op if it is gone) and its bwr
    /// as it was after the write, as binder_ioctl_write_read does: the
    /// restarted ioctl then reads the same work again without replaying the
    /// write.
    fn abandon(&mut self, proc: ProcId, tid: u32, read: Read, d: Delivery, us: &mut dyn Userspace) {
        self.rollback(proc, tid, d);
        put_bwr(proc, read.call, &read.bwr, us);
        us.answer(read.call, libc::EFAULT);
    }

    /// The reader has the delivery: apply what reading it means.
    fn commit(&mut self, proc: ProcId, tid: u32, d: Delivery) {
        for Taken { work, .. } in d.taken {
            match work {
                Work::ReturnError => self.thread_mut(proc, tid).return_error = BR_OK,
                Work::ReplyError => self.thread_mut(proc, tid).reply_error = BR_OK,
                Work::TransactionComplete
                | Work::OnewaySpamSuspect
                | Work::TransactionPendingFrozen
                | Work::FailedReply => {}
                Work::Node(nid) => self.apply_node_work(nid),
                Work::Death(death) => self.commit_death(proc, death),
                Work::Freeze(f) => self.commit_freeze(proc, f),
                Work::Transaction(txid) => self.commit_transaction(proc, tid, txid),
            }
        }
    }

    /// The reader never got the delivery: put its work back at the front of
    /// the lists it came from.
    fn rollback(&mut self, proc: ProcId, tid: u32, d: Delivery) {
        tlog!(
            "proc={proc} tid={tid} read rolled back: {} items",
            d.taken.len()
        );
        let p = self.procs.get_mut(&proc).unwrap();
        if d.spawn_looper {
            p.requested_threads -= 1;
        }
        for Taken { work, from_proc } in d.taken.into_iter().rev() {
            if from_proc {
                p.todo.push_front(work);
            } else {
                p.threads.get_mut(&tid).unwrap().todo.push_front(work);
            }
        }
        // Restore the flag the read cleared, keeping it set if work arrived
        // for the thread meanwhile.
        let t = p.threads.get_mut(&tid).unwrap();
        t.process_todo = (d.process_todo || t.process_todo) && !t.todo.is_empty();
        self.poll_dirty.insert(proc);
    }

    /// Serve the reads of the threads woken during this ioctl, then publish
    /// poll readiness.
    fn settle(&mut self, us: &mut dyn Userspace) {
        loop {
            while let Some((proc, tid)) = self.woken.pop_front() {
                let Some(t) = self
                    .procs
                    .get_mut(&proc)
                    .and_then(|p| p.threads.get_mut(&tid))
                else {
                    continue;
                };
                let Wait::Woken(read) = t.wait else {
                    continue;
                };
                t.wait = Wait::None;
                if us.pending(read.call) {
                    self.serve_read(proc, tid, read, false, us);
                } else {
                    // A signal ended its wait: the read is abandoned, and
                    // nothing is written to the thread, which may be stopped.
                    // It finds its own work when it restarts.
                    tlog!("proc={proc} tid={tid} woken, but its wait is over");
                    self.pass_on_proc_work(proc);
                }
            }
            // Freezes waiting for a target that drained go on (which may
            // queue freeze notifications, and wake readers).
            if self.drained.is_empty() {
                break;
            }
            let drained = std::mem::take(&mut self.drained);
            for (proc, tid, target) in self.freezing() {
                if drained.contains(&target) {
                    self.resume_freeze(proc, tid, us);
                }
            }
        }
        for proc in std::mem::take(&mut self.poll_dirty) {
            if self.procs.contains_key(&proc) {
                us.set_poll_ready(proc, self.proc_pollable(proc));
            }
        }
    }

    /// `binder_thread_write`. Returns (bytes consumed, errno).
    fn thread_write(
        &mut self,
        proc: ProcId,
        tid: u32,
        buf: &[u8],
        us: &mut dyn Userspace,
    ) -> (usize, i32) {
        let mut off = 0usize;
        while off < buf.len() {
            if self.procs[&proc].threads[&tid].return_error != BR_OK {
                break;
            }
            let Some(cmd) = read_pod::<u32>(buf, off) else {
                return (off, libc::EFAULT);
            };
            let start = off;
            off += 4;
            macro_rules! arg {
                ($t:ty) => {{
                    match read_pod::<$t>(buf, off) {
                        Some(v) => {
                            off += size_of::<$t>();
                            v
                        }
                        None => return (start, libc::EFAULT),
                    }
                }};
            }
            match cmd {
                BC_INCREFS | BC_ACQUIRE | BC_RELEASE | BC_DECREFS => {
                    let target: u32 = arg!(u32);
                    let strong = cmd == BC_ACQUIRE || cmd == BC_RELEASE;
                    let inc = cmd == BC_INCREFS || cmd == BC_ACQUIRE;
                    let r = if inc && target == 0 {
                        let ctx = self.procs[&proc].context as usize;
                        match self.contexts[ctx].mgr_node {
                            Some(n) if self.nodes[&n].proc == Some(proc) => Err(()),
                            Some(n) => self
                                .inc_ref_for_node(proc, n, strong, None)
                                .map(|_| ())
                                .map_err(|_| ()),
                            None => self.inc_ref(proc, 0, strong, None).map_err(|_| ()),
                        }
                    } else if inc {
                        if self.ref_node(proc, target, strong).is_some() {
                            self.inc_ref(proc, target, strong, None).map_err(|_| ())
                        } else {
                            Err(())
                        }
                    } else if self.ref_node(proc, target, strong).is_some() {
                        self.dec_ref(proc, target, strong).map_err(|_| ())
                    } else {
                        Err(())
                    };
                    if r.is_err() {
                        tlog!("proc={proc} refcount change {cmd:#x} on invalid ref {target}");
                    }
                }
                BC_INCREFS_DONE | BC_ACQUIRE_DONE => {
                    let ptr: u64 = arg!(u64);
                    let cookie: u64 = arg!(u64);
                    let Some(&nid) = self.procs[&proc].nodes.get(&ptr) else {
                        continue;
                    };
                    let n = self.nodes.get_mut(&nid).unwrap();
                    if n.cookie != cookie {
                        continue;
                    }
                    let strong = cmd == BC_ACQUIRE_DONE;
                    if strong {
                        if !n.pending_strong {
                            continue;
                        }
                        n.pending_strong = false;
                    } else {
                        if !n.pending_weak {
                            continue;
                        }
                        n.pending_weak = false;
                    }
                    self.dec_node(nid, strong, false);
                }
                BC_ATTEMPT_ACQUIRE | BC_ACQUIRE_RESULT => return (start, libc::EINVAL),
                BC_FREE_BUFFER => {
                    let ptr: u64 = arg!(u64);
                    self.free_buffer_cmd(proc, ptr, us);
                }
                BC_TRANSACTION_SG | BC_REPLY_SG => {
                    let td: BinderTransactionData = arg!(BinderTransactionData);
                    let extra: u64 = arg!(u64);
                    self.transaction(proc, tid, &td, cmd == BC_REPLY_SG, extra, us);
                }
                BC_TRANSACTION | BC_REPLY => {
                    let td: BinderTransactionData = arg!(BinderTransactionData);
                    self.transaction(proc, tid, &td, cmd == BC_REPLY, 0, us);
                }
                BC_REGISTER_LOOPER => {
                    let p = self.procs.get_mut(&proc).unwrap();
                    let t = p.threads.get_mut(&tid).unwrap();
                    if t.looper & LOOPER_ENTERED != 0 || p.requested_threads == 0 {
                        t.looper |= LOOPER_INVALID;
                    } else {
                        p.requested_threads -= 1;
                        p.requested_threads_started += 1;
                    }
                    t.looper |= LOOPER_REGISTERED;
                }
                BC_ENTER_LOOPER => {
                    let t = self.thread_mut(proc, tid);
                    if t.looper & LOOPER_REGISTERED != 0 {
                        t.looper |= LOOPER_INVALID;
                    }
                    t.looper |= LOOPER_ENTERED;
                }
                BC_EXIT_LOOPER => {
                    self.thread_mut(proc, tid).looper |= LOOPER_EXITED;
                }
                BC_REQUEST_DEATH_NOTIFICATION | BC_CLEAR_DEATH_NOTIFICATION => {
                    let target: u32 = arg!(u32);
                    let cookie: u64 = arg!(u64);
                    self.death_cmd(
                        proc,
                        tid,
                        target,
                        cookie,
                        cmd == BC_REQUEST_DEATH_NOTIFICATION,
                    );
                }
                BC_DEAD_BINDER_DONE => {
                    let cookie: u64 = arg!(u64);
                    self.dead_binder_done(proc, tid, cookie);
                }
                BC_REQUEST_FREEZE_NOTIFICATION | BC_CLEAR_FREEZE_NOTIFICATION => {
                    let target: u32 = arg!(u32);
                    let cookie: u64 = arg!(u64);
                    let r = if cmd == BC_REQUEST_FREEZE_NOTIFICATION {
                        self.request_freeze_notification(proc, target, cookie)
                    } else {
                        self.clear_freeze_notification(proc, target, cookie)
                    };
                    if let Err(e) = r {
                        return (start, e);
                    }
                }
                BC_FREEZE_NOTIFICATION_DONE => {
                    let cookie: u64 = arg!(u64);
                    if let Err(e) = self.freeze_notification_done(proc, cookie) {
                        return (start, e);
                    }
                }
                _ => return (start, libc::EINVAL),
            }
        }
        (off, 0)
    }

    // ---- transactions -------------------------------------------------------

    /// Record a failed transaction on the sending thread (the error part of
    /// `binder_transaction`).
    fn txn_error(
        &mut self,
        proc: ProcId,
        tid: u32,
        err: u32,
        errno: i32,
        in_reply_to: Option<TxnId>,
    ) {
        tlog!("txn fail proc={proc} tid={tid} err={err:#x} errno={errno} reply_to={in_reply_to:?}");
        let t = self.thread_mut(proc, tid);
        t.ee = BinderExtendedError {
            id: 0,
            command: err,
            param: errno,
        };
        if let Some(irt) = in_reply_to {
            t.return_error = BR_TRANSACTION_COMPLETE;
            self.enqueue_thread_work(proc, tid, Work::ReturnError);
            self.send_failed_reply(irt, err);
        } else {
            t.return_error = err;
            self.enqueue_thread_work(proc, tid, Work::ReturnError);
        }
    }

    fn transaction(
        &mut self,
        proc: ProcId,
        tid: u32,
        td: &BinderTransactionData,
        reply: bool,
        extra: u64,
        us: &mut dyn Userspace,
    ) {
        let oneway = td.flags & TF_ONE_WAY != 0;
        tlog!(
            "txn proc={proc} tid={tid} reply={reply} oneway={oneway} handle={} code={:#x} dsz={} osz={}",
            td.target as u32,
            td.code,
            td.data_size,
            td.offsets_size
        );
        let target_proc;
        let mut target_thread: Option<u32> = None;
        let mut target_node: Option<NodeId> = None;
        let mut in_reply_to: Option<TxnId> = None;

        if reply {
            let Some(irt) = self.procs[&proc].threads[&tid].transaction_stack else {
                return self.txn_error(proc, tid, BR_FAILED_REPLY, -libc::EPROTO, None);
            };
            if self.txns[&irt].to_thread != Some(tid) || self.txns[&irt].to_proc != Some(proc) {
                return self.txn_error(proc, tid, BR_FAILED_REPLY, -libc::EPROTO, None);
            }
            let to_parent = self.txns[&irt].to_parent;
            self.thread_mut(proc, tid).transaction_stack = to_parent;
            in_reply_to = Some(irt);
            let Some((fp, ft)) = self.txns[&irt].from else {
                return self.txn_error(proc, tid, BR_DEAD_REPLY, 0, in_reply_to);
            };
            let ok = self
                .procs
                .get(&fp)
                .and_then(|p| p.threads.get(&ft))
                .map(|t| t.transaction_stack == Some(irt))
                .unwrap_or(false);
            if !ok {
                return self.txn_error(proc, tid, BR_FAILED_REPLY, -libc::EPROTO, None);
            }
            target_proc = fp;
            target_thread = Some(ft);
        } else {
            let handle = td.target as u32;
            let node = if handle != 0 {
                self.ref_node(proc, handle, true)
            } else {
                self.contexts[self.procs[&proc].context as usize].mgr_node
            };
            let Some(node) = node else {
                let e = if handle == 0 {
                    BR_DEAD_REPLY
                } else {
                    BR_FAILED_REPLY
                };
                return self.txn_error(proc, tid, e, -libc::EINVAL, None);
            };
            let Some(tp) = self.nodes[&node].proc else {
                return self.txn_error(proc, tid, BR_DEAD_REPLY, 0, None);
            };
            if tp == proc {
                return self.txn_error(proc, tid, BR_FAILED_REPLY, -libc::EINVAL, None);
            }
            target_proc = tp;
            target_node = Some(node);
            if !oneway && let Some(top) = self.procs[&proc].threads[&tid].transaction_stack {
                if self.txns[&top].to_thread != Some(tid) || self.txns[&top].to_proc != Some(proc) {
                    return self.txn_error(proc, tid, BR_FAILED_REPLY, -libc::EPROTO, None);
                }
                let mut cur = Some(top);
                while let Some(c) = cur {
                    let t = &self.txns[&c];
                    if let Some((fp, ft)) = t.from
                        && fp == target_proc
                    {
                        target_thread = Some(ft);
                        break;
                    }
                    cur = t.from_parent;
                }
            }
        }
        if !self.procs.contains_key(&target_proc) {
            return self.txn_error(proc, tid, BR_DEAD_REPLY, 0, in_reply_to);
        }

        // Keep the target node alive and owned for the buffer's lifetime
        // (binder_get_node_refs_for_txn takes a local strong ref).
        if let Some(n) = target_node {
            self.inc_node(n, true, false, None);
        }
        let is_async = !reply && oneway;
        let built = self.build_buffer(
            proc,
            tid,
            td,
            extra,
            target_proc,
            target_node,
            in_reply_to,
            is_async,
            us,
        );
        let (addr, spam) = match built {
            Ok(v) => v,
            Err((e, errno)) => {
                if let Some(n) = target_node {
                    self.dec_node(n, true, false);
                }
                return self.txn_error(proc, tid, e, errno, in_reply_to);
            }
        };
        let txid = self.id();
        self.txns.insert(
            txid,
            Txn {
                from: if !reply && !oneway {
                    Some((proc, tid))
                } else {
                    None
                },
                from_parent: None,
                to_proc: Some(target_proc),
                to_thread: target_thread,
                to_parent: None,
                buffer: Some((target_proc, addr)),
                code: td.code,
                flags: td.flags,
                sender_euid: self.procs[&proc].euid,
                tokens: if self.ohos {
                    us.access_token(proc, tid)
                } else {
                    (0, 0)
                },
                outstanding: None,
            },
        );
        self.buffers
            .get_mut(&(target_proc, addr))
            .unwrap()
            .transaction = Some(txid);
        let tc = if spam {
            Work::OnewaySpamSuspect
        } else {
            Work::TransactionComplete
        };

        if reply {
            self.enqueue_thread_work(proc, tid, tc);
            let ft = target_thread.unwrap();
            let irt = in_reply_to.unwrap();
            if !self.procs[&target_proc].threads.contains_key(&ft) {
                self.dequeue_tc(proc, tid);
                self.abort_txn(txid, us);
                return self.txn_error(proc, tid, BR_DEAD_REPLY, 0, in_reply_to);
            }
            self.pop_transaction(target_proc, ft, irt);
            self.enqueue_thread_work(target_proc, ft, Work::Transaction(txid));
            self.count_outstanding(txid, target_proc);
            self.free_txn(irt);
        } else if !oneway {
            self.enqueue_deferred_thread_work(proc, tid, tc);
            let cur = self.procs[&proc].threads[&tid].transaction_stack;
            self.txns.get_mut(&txid).unwrap().from_parent = cur;
            self.thread_mut(proc, tid).transaction_stack = Some(txid);
            if let Err(e) = self.proc_transaction(txid, target_proc, target_thread) {
                // binder_pop_transaction_ilocked(thread, t)
                self.thread_mut(proc, tid).transaction_stack = cur;
                self.dequeue_tc(proc, tid);
                self.abort_txn(txid, us);
                self.txn_error(proc, tid, e, 0, None);
            }
        } else {
            match self.proc_transaction(txid, target_proc, None) {
                // The kernel reports a oneway to a frozen process in place of
                // its TRANSACTION_COMPLETE (or ONEWAY_SPAM_SUSPECT).
                Ok(true) => self.enqueue_thread_work(proc, tid, Work::TransactionPendingFrozen),
                Ok(false) => self.enqueue_thread_work(proc, tid, tc),
                Err(e) => {
                    self.abort_txn(txid, us);
                    self.txn_error(proc, tid, e, 0, None);
                }
            }
        }
    }

    /// Remove the deferred TRANSACTION_COMPLETE queued for a failed call.
    fn dequeue_tc(&mut self, proc: ProcId, tid: u32) {
        let t = self.thread_mut(proc, tid);
        if let Some(pos) = t
            .todo
            .iter()
            .rposition(|w| matches!(w, Work::TransactionComplete | Work::OnewaySpamSuspect))
        {
            t.todo.remove(pos);
        }
    }

    /// Undo a transaction that was built but never queued.
    fn abort_txn(&mut self, txid: TxnId, us: &mut dyn Userspace) {
        if let Some(t) = self.txns.remove(&txid)
            && let Some(key) = t.buffer
        {
            self.release_buffer(key, us);
        }
    }

    /// `binder_proc_transaction`. Ok(true): a oneway queued for a frozen
    /// process (BR_TRANSACTION_PENDING_FROZEN).
    fn proc_transaction(
        &mut self,
        txid: TxnId,
        target_proc: ProcId,
        target_thread: Option<u32>,
    ) -> Result<bool, u32> {
        let (oneway, node) = {
            let t = &self.txns[&txid];
            let node = t
                .buffer
                .and_then(|k| self.buffers.get(&k))
                .and_then(|b| b.target_node);
            (t.flags & TF_ONE_WAY != 0, node)
        };
        let mut pending_async = false;
        if oneway && let Some(n) = node {
            let n = self.nodes.get_mut(&n).unwrap();
            if n.has_async_transaction {
                pending_async = true;
            } else {
                n.has_async_transaction = true;
            }
        }
        let mut frozen = false;
        if let Some(p) = self.procs.get_mut(&target_proc)
            && p.is_frozen
        {
            frozen = true;
            p.sync_recv |= !oneway;
            p.async_recv |= oneway;
        }
        let dead = !self.procs.contains_key(&target_proc)
            || target_thread.is_some_and(|t| !self.procs[&target_proc].threads.contains_key(&t));
        if dead || (frozen && !oneway) {
            if oneway
                && !pending_async
                && let Some(n) = node
            {
                self.nodes.get_mut(&n).unwrap().has_async_transaction = false;
            }
            return Err(if frozen {
                BR_FROZEN_REPLY
            } else {
                BR_DEAD_REPLY
            });
        }
        // A call for a thread up the transaction stack goes on its list. Any
        // other goes on the process's and wakes the thread that waited
        // longest. (The kernel queues it on that thread's own list, but here
        // a waiting thread may be gone without the driver knowing yet: then
        // its read is abandoned, and the work stays for the next one.)
        if let Some(th) = target_thread {
            self.enqueue_thread_work(target_proc, th, Work::Transaction(txid));
        } else if pending_async {
            let n = node.unwrap();
            self.nodes.get_mut(&n).unwrap().async_todo.push_back(txid);
        } else {
            self.enqueue_proc_work(target_proc, Work::Transaction(txid));
        }
        self.count_outstanding(txid, target_proc);
        Ok(oneway && frozen)
    }

    /// The transaction is queued for `proc` (`outstanding_txns++`).
    fn count_outstanding(&mut self, txid: TxnId, proc: ProcId) {
        if let Some(t) = self.txns.get_mut(&txid)
            && let Some(p) = self.procs.get_mut(&proc)
        {
            debug_assert!(t.outstanding.is_none());
            t.outstanding = Some(proc);
            p.outstanding_txns += 1;
        }
    }

    /// A transaction `outstanding_txns` counted is done with
    /// (`binder_free_transaction`, or its reader thread exited). A freeze
    /// may be waiting for the last one.
    fn uncount_outstanding(&mut self, proc: Option<ProcId>) {
        if let Some(proc) = proc
            && let Some(p) = self.procs.get_mut(&proc)
        {
            p.outstanding_txns -= 1;
            if p.outstanding_txns == 0 && p.is_frozen {
                self.drained.insert(proc);
            }
        }
    }

    /// `binder_pop_transaction_ilocked`.
    fn pop_transaction(&mut self, proc: ProcId, tid: u32, txid: TxnId) {
        let parent = self.txns.get(&txid).and_then(|t| t.from_parent);
        if let Some(t) = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.threads.get_mut(&tid))
            && t.transaction_stack == Some(txid)
        {
            t.transaction_stack = parent;
        }
        if let Some(t) = self.txns.get_mut(&txid) {
            t.from = None;
        }
    }

    /// `binder_free_transaction`: drop the record; its buffer lives on until
    /// BC_FREE_BUFFER.
    fn free_txn(&mut self, txid: TxnId) {
        let Some(t) = self.txns.remove(&txid) else {
            return;
        };
        self.uncount_outstanding(t.outstanding);
        if let Some(key) = t.buffer
            && let Some(b) = self.buffers.get_mut(&key)
        {
            b.transaction = None;
        }
    }

    /// `binder_send_failed_reply`: fail the synchronous caller of `txid` (or
    /// the first live caller up its chain).
    fn send_failed_reply(&mut self, txid: TxnId, err: u32) {
        let mut cur = Some(txid);
        while let Some(t) = cur {
            let Some(tx) = self.txns.get(&t) else {
                return;
            };
            if tx.flags & TF_ONE_WAY != 0 {
                self.free_txn(t);
                return;
            }
            if let Some((fp, ft)) = tx.from
                && self
                    .procs
                    .get(&fp)
                    .and_then(|p| p.threads.get(&ft))
                    .is_some()
            {
                self.pop_transaction(fp, ft, t);
                let th = self.thread_mut(fp, ft);
                if th.reply_error == BR_OK {
                    th.reply_error = err;
                    self.enqueue_thread_work(fp, ft, Work::ReplyError);
                }
                self.free_txn(t);
                return;
            }
            let next = tx.from_parent;
            self.free_txn(t);
            cur = next;
        }
    }

    /// Copy the sender's data into a new buffer in `target_proc`, translating
    /// every object (`binder_transaction` from `binder_alloc_new_buf` to the
    /// end of the object loop). Returns the buffer address.
    #[allow(clippy::too_many_arguments)]
    fn build_buffer(
        &mut self,
        proc: ProcId,
        tid: u32,
        td: &BinderTransactionData,
        extra: u64,
        target_proc: ProcId,
        target_node: Option<NodeId>,
        in_reply_to: Option<TxnId>,
        is_async: bool,
        us: &mut dyn Userspace,
    ) -> Result<(u64, bool), (u32, i32)> {
        let fail = (BR_FAILED_REPLY, -libc::EINVAL);
        let data_size = usize::try_from(td.data_size).map_err(|_| fail)?;
        let offsets_size = usize::try_from(td.offsets_size).map_err(|_| fail)?;
        let extra = usize::try_from(extra).map_err(|_| fail)?;
        if !offsets_size.is_multiple_of(8) || !extra.is_multiple_of(8) {
            return Err(fail);
        }
        let want_secctx = target_node
            .map(|n| self.nodes[&n].txn_security_ctx)
            .unwrap_or(false);
        const SECCTX: &[u8] = b"u:r:su:s0\0";
        // Checked: a huge data_size must fail here, not wrap to a small total.
        let off_start = data_size.checked_next_multiple_of(8).ok_or(fail)?;
        let sg_start = off_start.checked_add(offsets_size).ok_or(fail)?;
        let sg_end = sg_start.checked_add(extra).ok_or(fail)?;
        let total = if want_secctx {
            sg_end.checked_add(align_up(SECCTX.len(), 8)).ok_or(fail)?
        } else {
            sg_end
        }
        .max(8);
        // Bound everything by the target's mapped region before reading.
        let space = us.recv_size(target_proc);
        if total > space {
            return Err((BR_FAILED_REPLY, -libc::ENOSPC));
        }
        {
            let p = self.procs.get_mut(&target_proc).unwrap();
            if p.free_async_space < 0 {
                p.free_async_space = (space / 2) as i64;
            }
            if is_async && p.free_async_space < total as i64 {
                return Err((BR_FAILED_REPLY, -libc::ENOSPC));
            }
        }
        let mut data = us
            .read(proc, td.data_buffer, data_size)
            .ok_or((BR_FAILED_REPLY, -libc::EFAULT))?;
        let offsets = us
            .read(proc, td.data_offsets, offsets_size)
            .ok_or((BR_FAILED_REPLY, -libc::EFAULT))?;
        let addr = us
            .alloc_recv(target_proc, total, is_async)
            .ok_or((BR_FAILED_REPLY, -libc::ENOSPC))?;
        let spam = {
            let p = self.procs.get_mut(&target_proc).unwrap();
            if is_async {
                p.free_async_space -= total as i64;
            }
            is_async && p.oneway_spam_detection && p.free_async_space < (space / 10) as i64
        };
        self.buffers.insert(
            (target_proc, addr),
            Buffer {
                data_size,
                offsets_size,
                total,
                is_async,
                target_node,
                transaction: None,
                allow_user_free: false,
                objects: Vec::new(),
                fd_fixups: Vec::new(),
                secctx: 0,
            },
        );
        let key = (target_proc, addr);
        let r = self.translate(
            proc,
            tid,
            &mut data,
            &offsets,
            sg_start,
            extra,
            key,
            in_reply_to,
            us,
        );
        let mut out = match r {
            Ok(sg) => {
                let mut out = vec![0u8; total];
                out[..data_size].copy_from_slice(&data);
                out[off_start..off_start + offsets_size].copy_from_slice(&offsets);
                out[sg_start..sg_end].copy_from_slice(&sg);
                out
            }
            Err(e) => {
                // The caller drops the target-node ref it took.
                self.buffers.get_mut(&key).unwrap().target_node = None;
                self.release_buffer(key, us);
                return Err(e);
            }
        };
        if want_secctx {
            out[sg_end..sg_end + SECCTX.len()].copy_from_slice(SECCTX);
            self.buffers.get_mut(&key).unwrap().secctx = addr + sg_end as u64;
        }
        us.write_recv(target_proc, addr, &out);
        Ok((addr, spam))
    }

    /// Translate the objects in `data` for `key.0`; returns the gathered
    /// scatter-gather area. Objects translated so far are recorded on the
    /// buffer, so the caller releases them on failure.
    #[allow(clippy::too_many_arguments)]
    fn translate(
        &mut self,
        proc: ProcId,
        tid: u32,
        data: &mut [u8],
        offsets: &[u8],
        sg_start: usize,
        extra: usize,
        key: (ProcId, u64),
        in_reply_to: Option<TxnId>,
        us: &mut dyn Userspace,
    ) -> Result<Vec<u8>, (u32, i32)> {
        let fail = (BR_FAILED_REPLY, -libc::EINVAL);
        let (target_proc, addr) = key;
        let allow_fds = match in_reply_to {
            Some(t) => self
                .txns
                .get(&t)
                .map(|t| t.flags & TF_ACCEPT_FDS != 0)
                .unwrap_or(false),
            None => self.buffers[&key]
                .target_node
                .map(|n| self.nodes[&n].accept_fds)
                .unwrap_or(false),
        };
        let mut sg = vec![0u8; extra];
        let mut sg_used = 0usize;
        let mut off_min = 0usize;
        // (object offset in data, sg offset, length) of validated PTR objects
        let mut ptrs: Vec<Option<(usize, usize)>> = Vec::new();
        for i in 0..offsets.len() / 8 {
            let obj_off = read_pod::<u64>(offsets, i * 8).unwrap();
            let obj_off = usize::try_from(obj_off).map_err(|_| fail)?;
            if obj_off < off_min
                || !obj_off.is_multiple_of(4)
                || obj_off.checked_add(8).ok_or(fail)? > data.len()
            {
                tlog!(
                    "xlate: bad object offset {obj_off} (min {off_min}, size {})",
                    data.len()
                );
                return Err(fail);
            }
            let hdr = read_pod::<u32>(data, obj_off).unwrap();
            let mut this_ptr = None;
            match hdr {
                BINDER_TYPE_BINDER | BINDER_TYPE_WEAK_BINDER => {
                    let mut fbo: FlatBinderObject = read_pod(data, obj_off).ok_or(fail)?;
                    let strong = hdr == BINDER_TYPE_BINDER;
                    let nid =
                        self.get_or_new_node(proc, fbo.binder_or_handle, fbo.cookie, fbo.flags);
                    if self.nodes[&nid].cookie != fbo.cookie {
                        return Err(fail);
                    }
                    let desc = self
                        .inc_ref_for_node(target_proc, nid, strong, Some((proc, tid)))
                        .map_err(|e| (e, -libc::EINVAL))?;
                    self.buffers
                        .get_mut(&key)
                        .unwrap()
                        .objects
                        .push(Obj::Handle { desc, strong });
                    fbo.hdr_type = if strong {
                        BINDER_TYPE_HANDLE
                    } else {
                        BINDER_TYPE_WEAK_HANDLE
                    };
                    fbo.binder_or_handle = desc as u64;
                    fbo.cookie = 0;
                    data[obj_off..obj_off + size_of::<FlatBinderObject>()]
                        .copy_from_slice(pod_bytes(&fbo));
                    off_min = obj_off + size_of::<FlatBinderObject>();
                }
                BINDER_TYPE_HANDLE | BINDER_TYPE_WEAK_HANDLE => {
                    let mut fbo: FlatBinderObject = read_pod(data, obj_off).ok_or(fail)?;
                    let strong = hdr == BINDER_TYPE_HANDLE;
                    let h = fbo.binder_or_handle as u32;
                    let Some(nid) = self.ref_node(proc, h, strong) else {
                        tlog!("xlate: proc={proc} handle {h} not found");
                        return Err(fail);
                    };
                    if self.nodes[&nid].proc == Some(target_proc) {
                        let n = &self.nodes[&nid];
                        fbo.hdr_type = if strong {
                            BINDER_TYPE_BINDER
                        } else {
                            BINDER_TYPE_WEAK_BINDER
                        };
                        fbo.binder_or_handle = n.ptr;
                        fbo.cookie = n.cookie;
                        self.inc_node(nid, strong, false, None);
                        self.buffers
                            .get_mut(&key)
                            .unwrap()
                            .objects
                            .push(Obj::Node { node: nid, strong });
                    } else {
                        let desc = self
                            .inc_ref_for_node(target_proc, nid, strong, None)
                            .map_err(|e| (e, -libc::EINVAL))?;
                        self.buffers
                            .get_mut(&key)
                            .unwrap()
                            .objects
                            .push(Obj::Handle { desc, strong });
                        fbo.binder_or_handle = desc as u64;
                        fbo.cookie = 0;
                    }
                    data[obj_off..obj_off + size_of::<FlatBinderObject>()]
                        .copy_from_slice(pod_bytes(&fbo));
                    off_min = obj_off + size_of::<FlatBinderObject>();
                }
                BINDER_TYPE_FD => {
                    let mut fdo: BinderFdObject = read_pod(data, obj_off).ok_or(fail)?;
                    if !allow_fds {
                        return Err((BR_FAILED_REPLY, -libc::EPERM));
                    }
                    let tok = us
                        .capture_fd(proc, fdo.fd_or_pad as u32)
                        .ok_or((BR_FAILED_REPLY, -libc::EBADF))?;
                    self.buffers
                        .get_mut(&key)
                        .unwrap()
                        .fd_fixups
                        .push((obj_off + 8, tok));
                    fdo.fd_or_pad = 0;
                    data[obj_off..obj_off + size_of::<BinderFdObject>()]
                        .copy_from_slice(pod_bytes(&fdo));
                    off_min = obj_off + size_of::<BinderFdObject>();
                }
                BINDER_TYPE_PTR => {
                    let mut bbo: BinderBufferObject = read_pod(data, obj_off).ok_or(fail)?;
                    let len = usize::try_from(bbo.length).map_err(|_| fail)?;
                    if len > extra - sg_used {
                        return Err(fail);
                    }
                    let payload = us
                        .read(proc, bbo.buffer, len)
                        .ok_or((BR_FAILED_REPLY, -libc::EFAULT))?;
                    let at = sg_used;
                    sg[at..at + len].copy_from_slice(&payload);
                    sg_used += align_up(len, 8).min(extra - sg_used);
                    bbo.buffer = addr + (sg_start + at) as u64;
                    if bbo.flags & BINDER_BUFFER_FLAG_HAS_PARENT != 0 {
                        let (psg, plen) = parent_ptr(&ptrs, bbo.parent).ok_or(fail)?;
                        let po = usize::try_from(bbo.parent_offset).map_err(|_| fail)?;
                        if plen < 8 || po > plen - 8 {
                            return Err(fail);
                        }
                        sg[psg + po..psg + po + 8].copy_from_slice(&bbo.buffer.to_le_bytes());
                    }
                    data[obj_off..obj_off + size_of::<BinderBufferObject>()]
                        .copy_from_slice(pod_bytes(&bbo));
                    this_ptr = Some((at, len));
                    off_min = obj_off + size_of::<BinderBufferObject>();
                }
                BINDER_TYPE_FDA => {
                    let fda: BinderFdArrayObject = read_pod(data, obj_off).ok_or(fail)?;
                    if !allow_fds {
                        return Err((BR_FAILED_REPLY, -libc::EPERM));
                    }
                    let (psg, plen) = parent_ptr(&ptrs, fda.parent).ok_or(fail)?;
                    let po = usize::try_from(fda.parent_offset).map_err(|_| fail)?;
                    let n = usize::try_from(fda.num_fds).map_err(|_| fail)?;
                    let bytes = n.checked_mul(4).ok_or(fail)?;
                    if po.checked_add(bytes).ok_or(fail)? > plen
                        || !(sg_start + psg + po).is_multiple_of(4)
                    {
                        return Err(fail);
                    }
                    for k in 0..n {
                        let at = psg + po + k * 4;
                        let fd = u32::from_le_bytes(sg[at..at + 4].try_into().unwrap());
                        let tok = us
                            .capture_fd(proc, fd)
                            .ok_or((BR_FAILED_REPLY, -libc::EBADF))?;
                        self.buffers
                            .get_mut(&key)
                            .unwrap()
                            .fd_fixups
                            .push((sg_start + at, tok));
                        sg[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                    off_min = obj_off + size_of::<BinderFdArrayObject>();
                }
                _ => {
                    tlog!("xlate: bad object type {hdr:#x}");
                    return Err(fail);
                }
            }
            ptrs.push(this_ptr);
        }
        Ok(sg)
    }

    // ---- buffers ------------------------------------------------------------

    /// `BC_FREE_BUFFER`.
    fn free_buffer_cmd(&mut self, proc: ProcId, ptr: u64, us: &mut dyn Userspace) {
        let key = (proc, ptr);
        let Some(b) = self.buffers.get(&key) else {
            tlog!("proc={proc} BC_FREE_BUFFER {ptr:#x} no match");
            return;
        };
        if !b.allow_user_free {
            tlog!("proc={proc} BC_FREE_BUFFER {ptr:#x} not freeable");
            return;
        }
        self.free_buf(key, us);
    }

    /// `binder_free_buf`: the reader is done with a buffer. If it held a
    /// oneway, the next one queued for its node becomes process work.
    fn free_buf(&mut self, key: (ProcId, u64), us: &mut dyn Userspace) {
        if let Some(b) = self.buffers.get(&key)
            && b.is_async
            && let Some(n) = b.target_node
            && let Some(node) = self.nodes.get_mut(&n)
        {
            match node.async_todo.pop_front() {
                None => node.has_async_transaction = false,
                Some(next) => self.enqueue_proc_work(key.0, Work::Transaction(next)),
            }
        }
        self.release_buffer(key, us);
    }

    /// `binder_transaction_buffer_release` + `binder_alloc_free_buf`.
    fn release_buffer(&mut self, key: (ProcId, u64), us: &mut dyn Userspace) {
        let Some(b) = self.buffers.remove(&key) else {
            return;
        };
        let (proc, addr) = key;
        if let Some(t) = b.transaction
            && let Some(tx) = self.txns.get_mut(&t)
        {
            tx.buffer = None;
        }
        if let Some(n) = b.target_node {
            self.dec_node(n, true, false);
        }
        for o in b.objects {
            match o {
                Obj::Node { node, strong } => self.dec_node(node, strong, false),
                Obj::Handle { desc, strong } => {
                    let _ = self.dec_ref(proc, desc, strong);
                }
            }
        }
        for (_, tok) in b.fd_fixups {
            us.drop_fd(tok);
        }
        if b.is_async
            && let Some(p) = self.procs.get_mut(&proc)
        {
            p.free_async_space += b.total as i64;
        }
        us.free_recv(proc, addr);
    }

    // ---- death notifications ------------------------------------------------

    fn death_cmd(&mut self, proc: ProcId, tid: u32, desc: u32, cookie: u64, request: bool) {
        let Some(r) = self.procs[&proc].refs_by_desc.get(&desc) else {
            tlog!("proc={proc} death notification on invalid ref {desc}");
            return;
        };
        let nid = r.node;
        if request {
            if r.death.is_some() {
                return;
            }
            let d = self.id();
            self.deaths.insert(
                d,
                Death {
                    cookie,
                    kind: DeathKind::DeadBinder,
                    queued: false,
                },
            );
            self.procs
                .get_mut(&proc)
                .unwrap()
                .refs_by_desc
                .get_mut(&desc)
                .unwrap()
                .death = Some(d);
            if self
                .nodes
                .get(&nid)
                .map(|n| n.proc.is_none())
                .unwrap_or(true)
            {
                self.deaths.get_mut(&d).unwrap().queued = true;
                self.enqueue_proc_work(proc, Work::Death(d));
            }
        } else {
            let Some(d) = r.death else {
                return;
            };
            if self.deaths[&d].cookie != cookie {
                return;
            }
            self.procs
                .get_mut(&proc)
                .unwrap()
                .refs_by_desc
                .get_mut(&desc)
                .unwrap()
                .death = None;
            let death = self.deaths.get_mut(&d).unwrap();
            if !death.queued {
                death.kind = DeathKind::Clear;
                death.queued = true;
                self.queue_death_to_thread_or_proc(proc, tid, d);
            } else {
                death.kind = DeathKind::DeadBinderAndClear;
            }
        }
    }

    fn queue_death_to_thread_or_proc(&mut self, proc: ProcId, tid: u32, d: DeathId) {
        if self.thread_mut(proc, tid).looper & (LOOPER_REGISTERED | LOOPER_ENTERED) != 0 {
            self.enqueue_thread_work(proc, tid, Work::Death(d));
        } else {
            self.enqueue_proc_work(proc, Work::Death(d));
        }
    }

    fn dead_binder_done(&mut self, proc: ProcId, tid: u32, cookie: u64) {
        let found = self.procs[&proc].delivered_death.iter().copied().find(|d| {
            self.deaths
                .get(d)
                .map(|x| x.cookie == cookie)
                .unwrap_or(false)
        });
        let Some(d) = found else {
            return;
        };
        self.procs
            .get_mut(&proc)
            .unwrap()
            .delivered_death
            .retain(|x| *x != d);
        let death = self.deaths.get_mut(&d).unwrap();
        death.queued = false;
        if death.kind == DeathKind::DeadBinderAndClear {
            death.kind = DeathKind::Clear;
            death.queued = true;
            self.queue_death_to_thread_or_proc(proc, tid, d);
        }
    }

    // ---- the freezer ----------------------------------------------------------

    /// `BINDER_FREEZE`: freeze or thaw every binder instance of a pid (in
    /// Android's pid namespace), in turn.
    fn freeze_ioctl(&mut self, proc: ProcId, tid: u32, call: Call, us: &mut dyn Userspace) {
        let Some(info) = us
            .read(proc, call.arg, size_of::<BinderFreezeInfo>())
            .and_then(|b| read_pod::<BinderFreezeInfo>(&b, 0))
        else {
            us.answer(call, libc::EFAULT);
            return;
        };
        let mut targets: Vec<ProcId> = self
            .procs
            .iter()
            .filter(|(_, p)| p.pid as u32 == info.pid)
            .map(|(&id, _)| id)
            .collect();
        if targets.is_empty() {
            us.answer(call, libc::EINVAL);
            return;
        }
        targets.sort();
        tlog!(
            "proc={proc} tid={tid} freeze pid={} enable={} timeout={}",
            info.pid,
            info.enable,
            info.timeout_ms
        );
        let f = Freezing {
            call,
            enable: info.enable != 0,
            timeout_ms: info.timeout_ms,
            targets,
            deadline: None,
        };
        self.run_freeze(proc, tid, f, us);
    }

    /// `binder_ioctl_freeze` for each remaining target: answer the call, or
    /// wait for a target to drain.
    fn run_freeze(&mut self, proc: ProcId, tid: u32, mut f: Freezing, us: &mut dyn Userspace) {
        while let Some(&target) = f.targets.first() {
            let Some(p) = self.procs.get_mut(&target) else {
                // Released (which thaws it): nothing left to freeze there.
                f.targets.remove(0);
                f.deadline = None;
                continue;
            };
            if !f.enable {
                p.sync_recv = false;
                p.async_recv = false;
                p.is_frozen = false;
                self.add_freeze_work(target, false);
                f.targets.remove(0);
                continue;
            }
            let now = us.now_ms();
            let deadline = *f.deadline.get_or_insert_with(|| {
                p.sync_recv = false;
                p.async_recv = false;
                p.is_frozen = true;
                now.saturating_add(f.timeout_ms as u64)
            });
            if p.outstanding_txns > 0 && now < deadline {
                us.arm_timer(deadline);
                self.thread_mut(proc, tid).wait = Wait::Freezing(f);
                return;
            }
            if self.txns_pending(target) {
                self.procs.get_mut(&target).unwrap().is_frozen = false;
                us.answer(f.call, libc::EAGAIN);
                return;
            }
            self.add_freeze_work(target, true);
            f.targets.remove(0);
            f.deadline = None;
        }
        us.answer(f.call, 0);
    }

    /// A freeze's target drained or its deadline passed: go on, unless the
    /// call is gone.
    fn resume_freeze(&mut self, proc: ProcId, tid: u32, us: &mut dyn Userspace) {
        let Some(t) = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.threads.get_mut(&tid))
        else {
            return;
        };
        if !matches!(t.wait, Wait::Freezing(_)) {
            return;
        }
        let Wait::Freezing(f) = std::mem::take(&mut t.wait) else {
            unreachable!()
        };
        if us.pending(f.call) {
            self.run_freeze(proc, tid, f, us);
        } else {
            self.freeze_interrupted(&f);
        }
    }

    /// A signal ended a freeze's wait (`-ERESTARTSYS`): its target thaws.
    fn freeze_interrupted(&mut self, f: &Freezing) {
        tlog!("freeze of {:?} interrupted", f.targets.first());
        if f.deadline.is_some()
            && let Some(p) = f.targets.first().and_then(|t| self.procs.get_mut(t))
        {
            p.is_frozen = false;
        }
    }

    /// The threads waiting in `BINDER_FREEZE`, and the target each waits for.
    fn freezing(&self) -> Vec<(ProcId, u32, ProcId)> {
        let mut v = Vec::new();
        for (&proc, p) in &self.procs {
            for (&tid, t) in &p.threads {
                if let Wait::Freezing(f) = &t.wait {
                    v.push((proc, tid, f.targets[0]));
                }
            }
        }
        v.sort();
        v
    }

    /// The timer [`Userspace::arm_timer`] asked for fired: freezes whose
    /// deadline passed go on.
    pub fn expire(&mut self, us: &mut dyn Userspace) {
        let now = us.now_ms();
        let mut next: Option<u64> = None;
        for (proc, tid, _) in self.freezing() {
            let Wait::Freezing(f) = &self.procs[&proc].threads[&tid].wait else {
                continue;
            };
            match f.deadline {
                Some(d) if d <= now => self.resume_freeze(proc, tid, us),
                Some(d) => next = Some(next.map_or(d, |n| n.min(d))),
                None => {}
            }
        }
        if let Some(d) = next {
            us.arm_timer(d);
        }
        self.settle(us);
    }

    /// `binder_txns_pending_ilocked`: transactions queued for `proc`, or one
    /// of its threads serving a call.
    fn txns_pending(&self, proc: ProcId) -> bool {
        let p = &self.procs[&proc];
        p.outstanding_txns > 0
            || p.threads.iter().any(|(&tid, t)| {
                t.transaction_stack
                    .and_then(|x| self.txns.get(&x))
                    .is_some_and(|x| x.to_thread == Some(tid) && x.to_proc == Some(proc))
            })
    }

    /// `BINDER_GET_FROZEN_INFO` (bit 1 of `sync_recv`: transactions pending).
    fn frozen_info(&self, pid: u32) -> Option<BinderFrozenStatusInfo> {
        let mut info = BinderFrozenStatusInfo {
            pid,
            ..Default::default()
        };
        let mut found = false;
        for (&id, p) in &self.procs {
            if p.pid as u32 == pid {
                found = true;
                info.sync_recv |= p.sync_recv as u32 | (self.txns_pending(id) as u32) << 1;
                info.async_recv |= p.async_recv as u32;
            }
        }
        found.then_some(info)
    }

    /// `binder_add_freeze_work`: tell every holder of a ref to `proc`'s
    /// nodes that asked for it. A notification not delivered yet just takes
    /// the new state; one delivered but not acknowledged is sent again.
    fn add_freeze_work(&mut self, proc: ProcId, is_frozen: bool) {
        let mut nodes: Vec<NodeId> = self.procs[&proc].nodes.values().copied().collect();
        nodes.sort();
        for nid in nodes {
            let holders: Vec<ProcId> = self.nodes[&nid].refs.iter().copied().collect();
            for h in holders {
                let Some(f) = self.procs.get(&h).and_then(|hp| {
                    let d = hp.node_to_desc.get(&nid)?;
                    hp.refs_by_desc[d].freeze
                }) else {
                    continue;
                };
                let x = self.freezes.get_mut(&f).unwrap();
                if !x.queued {
                    x.is_frozen = is_frozen;
                    x.queued = true;
                    self.enqueue_proc_work(h, Work::Freeze(f));
                } else {
                    x.resend |= x.sent && x.is_frozen != is_frozen;
                    x.is_frozen = is_frozen;
                }
            }
        }
    }

    /// `BC_REQUEST_FREEZE_NOTIFICATION`: the current state is sent at once.
    fn request_freeze_notification(
        &mut self,
        proc: ProcId,
        desc: u32,
        cookie: u64,
    ) -> Result<(), i32> {
        let r = self.procs[&proc]
            .refs_by_desc
            .get(&desc)
            .ok_or(libc::EINVAL)?;
        if r.freeze.is_some() {
            return Err(libc::EINVAL);
        }
        let is_frozen = self.nodes[&r.node]
            .proc
            .and_then(|o| self.procs.get(&o))
            .is_some_and(|o| o.is_frozen);
        let f = self.id();
        self.freezes.insert(
            f,
            FreezeListener {
                cookie,
                is_frozen,
                cleared: false,
                sent: false,
                resend: false,
                queued: true,
            },
        );
        let p = self.procs.get_mut(&proc).unwrap();
        p.refs_by_desc.get_mut(&desc).unwrap().freeze = Some(f);
        self.enqueue_proc_work(proc, Work::Freeze(f));
        Ok(())
    }

    /// `BC_CLEAR_FREEZE_NOTIFICATION`: detach it from its ref; its next
    /// delivery is BR_CLEAR_FREEZE_NOTIFICATION_DONE.
    fn clear_freeze_notification(
        &mut self,
        proc: ProcId,
        desc: u32,
        cookie: u64,
    ) -> Result<(), i32> {
        let r = self
            .procs
            .get_mut(&proc)
            .unwrap()
            .refs_by_desc
            .get_mut(&desc)
            .ok_or(libc::EINVAL)?;
        let f = r.freeze.ok_or(libc::EINVAL)?;
        if self.freezes[&f].cookie != cookie {
            return Err(libc::EINVAL);
        }
        r.freeze = None;
        let x = self.freezes.get_mut(&f).unwrap();
        x.cleared = true;
        if !x.queued {
            x.queued = true;
            self.enqueue_proc_work(proc, Work::Freeze(f));
        } else if x.sent {
            x.resend = true;
        }
        Ok(())
    }

    /// `BC_FREEZE_NOTIFICATION_DONE`.
    fn freeze_notification_done(&mut self, proc: ProcId, cookie: u64) -> Result<(), i32> {
        let p = self.procs.get_mut(&proc).unwrap();
        let pos = p
            .delivered_freeze
            .iter()
            .position(|f| self.freezes.get(f).is_some_and(|x| x.cookie == cookie))
            .ok_or(libc::EINVAL)?;
        let f = p.delivered_freeze.remove(pos);
        let x = self.freezes.get_mut(&f).unwrap();
        x.sent = false;
        if x.resend {
            x.resend = false;
            self.enqueue_proc_work(proc, Work::Freeze(f));
        } else {
            x.queued = false;
        }
        Ok(())
    }

    /// A delivered freeze notification: a cleared one is done, a state waits
    /// for BC_FREEZE_NOTIFICATION_DONE.
    fn commit_freeze(&mut self, proc: ProcId, f: FreezeId) {
        let Some(x) = self.freezes.get_mut(&f) else {
            return;
        };
        if x.cleared {
            self.freezes.remove(&f);
        } else {
            x.sent = true;
            self.procs.get_mut(&proc).unwrap().delivered_freeze.push(f);
        }
    }

    // ---- thread / process release -------------------------------------------

    /// `binder_thread_release` (BINDER_THREAD_EXIT, or the process is going).
    fn thread_exit(&mut self, proc: ProcId, tid: u32) {
        let Some(t) = self
            .procs
            .get_mut(&proc)
            .and_then(|p| p.threads.remove(&tid))
        else {
            return;
        };
        if let Wait::Freezing(f) = &t.wait {
            self.freeze_interrupted(f);
        }
        let mut send_reply = None;
        let mut cur = t.transaction_stack;
        if let Some(c) = cur
            && self
                .txns
                .get(&c)
                .map(|x| x.to_thread == Some(tid) && x.to_proc == Some(proc))
                .unwrap_or(false)
        {
            send_reply = Some(c);
        }
        while let Some(c) = cur {
            let Some(tx) = self.txns.get_mut(&c) else {
                break;
            };
            if tx.to_thread == Some(tid) && tx.to_proc == Some(proc) {
                tx.to_proc = None;
                tx.to_thread = None;
                cur = tx.to_parent;
                let outstanding = tx.outstanding.take();
                if let Some(key) = tx.buffer.take()
                    && let Some(b) = self.buffers.get_mut(&key)
                {
                    b.transaction = None;
                }
                self.uncount_outstanding(outstanding);
            } else if tx.from == Some((proc, tid)) {
                tx.from = None;
                cur = tx.from_parent;
            } else {
                break;
            }
        }
        if let Some(c) = send_reply {
            self.send_failed_reply(c, BR_DEAD_REPLY);
        }
        self.release_work(proc, t.todo);
        self.poll_dirty.insert(proc);
    }

    /// `binder_release_work`: fail synchronous transactions nobody will read.
    fn release_work(&mut self, _proc: ProcId, list: VecDeque<Work>) {
        for w in list {
            match w {
                Work::Transaction(t) => {
                    let Some(tx) = self.txns.get(&t) else {
                        continue;
                    };
                    let has_node = tx
                        .buffer
                        .and_then(|k| self.buffers.get(&k))
                        .and_then(|b| b.target_node)
                        .is_some();
                    if has_node && tx.flags & TF_ONE_WAY == 0 {
                        self.send_failed_reply(t, BR_DEAD_REPLY);
                    } else {
                        self.free_txn(t);
                    }
                }
                Work::Death(d) => {
                    if self
                        .deaths
                        .get(&d)
                        .map(|x| x.kind != DeathKind::DeadBinder)
                        .unwrap_or(false)
                    {
                        self.deaths.remove(&d);
                    } else if let Some(x) = self.deaths.get_mut(&d) {
                        x.queued = false;
                    }
                }
                Work::Node(n) => {
                    if let Some(n) = self.nodes.get_mut(&n) {
                        n.work_queued = false;
                    }
                }
                // A cleared notification is gone; one its ref holds stays
                // until the ref goes.
                Work::Freeze(f) => match self.freezes.get_mut(&f) {
                    Some(x) if x.cleared => {
                        self.freezes.remove(&f);
                    }
                    Some(x) => x.queued = false,
                    None => {}
                },
                _ => {}
            }
        }
    }

    /// The process closed its binder fd or died (`binder_deferred_release`).
    /// Its waiting threads get `EBADF`; threads it wakes elsewhere are served.
    pub fn release_proc(&mut self, proc: ProcId, us: &mut dyn Userspace) {
        let Some(p) = self.procs.get(&proc) else {
            return;
        };
        for t in p.threads.values() {
            match &t.wait {
                Wait::Blocked { read, .. } => us.answer(read.call, libc::EBADF),
                Wait::Freezing(f) => us.answer(f.call, libc::EBADF),
                _ => true,
            };
        }
        let ctx = p.context as usize;
        if let Some(n) = self.contexts[ctx].mgr_node
            && self
                .nodes
                .get(&n)
                .map(|n| n.proc == Some(proc))
                .unwrap_or(true)
        {
            self.contexts[ctx].mgr_node = None;
        }
        let tids: Vec<u32> = self.procs[&proc].threads.keys().copied().collect();
        for tid in tids {
            self.thread_exit(proc, tid);
        }
        // Nodes: notify every holder that asked for a death notification.
        let nodes: Vec<NodeId> = self.procs[&proc].nodes.values().copied().collect();
        self.procs.get_mut(&proc).unwrap().nodes.clear();
        for nid in nodes {
            let Some(n) = self.nodes.get_mut(&nid) else {
                continue;
            };
            let async_todo: VecDeque<Work> =
                n.async_todo.drain(..).map(Work::Transaction).collect();
            let queued = n.work_queued;
            n.work_queued = false;
            n.proc = None;
            n.local_strong = 0;
            n.local_weak = 0;
            let holders: Vec<ProcId> = n.refs.iter().copied().collect();
            self.release_work(proc, async_todo);
            if queued {
                self.dequeue_work(proc, Work::Node(nid));
            }
            if holders.is_empty() {
                self.nodes.remove(&nid);
                continue;
            }
            for h in holders {
                let Some(&desc) = self.procs.get(&h).and_then(|p| p.node_to_desc.get(&nid)) else {
                    continue;
                };
                let Some(d) = self.procs[&h].refs_by_desc[&desc].death else {
                    continue;
                };
                if let Some(death) = self.deaths.get_mut(&d)
                    && !death.queued
                {
                    death.kind = DeathKind::DeadBinder;
                    death.queued = true;
                    self.enqueue_proc_work(h, Work::Death(d));
                }
            }
        }
        // Outgoing refs.
        let descs: Vec<u32> = self.procs[&proc].refs_by_desc.keys().copied().collect();
        for d in descs {
            self.delete_ref(proc, d);
        }
        let todo = std::mem::take(&mut self.procs.get_mut(&proc).unwrap().todo);
        self.release_work(proc, todo);
        let p = self.procs.get_mut(&proc).unwrap();
        let delivered: VecDeque<Work> = std::mem::take(&mut p.delivered_death)
            .into_iter()
            .map(Work::Death)
            .chain(
                std::mem::take(&mut p.delivered_freeze)
                    .into_iter()
                    .map(Work::Freeze),
            )
            .collect();
        self.release_work(proc, delivered);
        // Buffers still held by the process.
        let keys: Vec<(ProcId, u64)> = self
            .buffers
            .keys()
            .filter(|k| k.0 == proc)
            .copied()
            .collect();
        for k in keys {
            if let Some(t) = self.buffers[&k].transaction {
                if self.txns.get(&t).map(|t| t.from.is_some()).unwrap_or(false) {
                    self.send_failed_reply(t, BR_DEAD_REPLY);
                } else {
                    self.free_txn(t);
                }
            }
            // binder_alloc_deferred_release frees the memory only: the refs
            // these buffers held died with the process's nodes and handles.
            if let Some(b) = self.buffers.remove(&k) {
                for (_, tok) in b.fd_fixups {
                    us.drop_fd(tok);
                }
                us.free_recv(k.0, k.1);
            }
        }
        // Transactions still pointing at this proc as the target.
        let stale: Vec<TxnId> = self
            .txns
            .iter()
            .filter(|(_, t)| t.to_proc == Some(proc))
            .map(|(&id, _)| id)
            .collect();
        for t in stale {
            if self.txns.get(&t).map(|t| t.from.is_some()).unwrap_or(false) {
                self.send_failed_reply(t, BR_DEAD_REPLY);
            } else {
                self.txns.remove(&t);
            }
        }
        self.procs.remove(&proc);
        // A freeze waiting for it to drain goes on to its next target.
        self.drained.insert(proc);
        self.settle(us);
    }

    // ---- small ioctls ---------------------------------------------------------

    fn node_debug_info(&self, proc: ProcId, after_ptr: u64) -> BinderNodeDebugInfo {
        let mut best: Option<&Node> = None;
        if let Some(p) = self.procs.get(&proc) {
            for (&ptr, nid) in p.nodes.iter() {
                if ptr > after_ptr {
                    let n = &self.nodes[nid];
                    if best.map(|b| n.ptr < b.ptr).unwrap_or(true) {
                        best = Some(n);
                    }
                }
            }
        }
        match best {
            Some(n) => BinderNodeDebugInfo {
                ptr: n.ptr,
                cookie: n.cookie,
                has_strong_ref: n.has_strong_ref as u32,
                has_weak_ref: n.has_weak_ref as u32,
            },
            None => BinderNodeDebugInfo::default(),
        }
    }

    /// `BINDER_GET_NODE_INFO_FOR_REF` (context manager only).
    fn node_info_for_ref(
        &self,
        proc: ProcId,
        info: BinderNodeInfoForRef,
    ) -> Result<BinderNodeInfoForRef, i32> {
        if info.strong_count != 0
            || info.weak_count != 0
            || info.reserved1 != 0
            || info.reserved2 != 0
            || info.reserved3 != 0
        {
            return Err(libc::EINVAL);
        }
        let p = &self.procs[&proc];
        let mgr = self.contexts[p.context as usize]
            .mgr_node
            .ok_or(libc::EPERM)?;
        if self.nodes.get(&mgr).and_then(|n| n.proc) != Some(proc) {
            return Err(libc::EPERM);
        }
        let nid = self
            .ref_node(proc, info.handle, false)
            .ok_or(libc::EINVAL)?;
        let n = &self.nodes[&nid];
        Ok(BinderNodeInfoForRef {
            handle: info.handle,
            strong_count: (n.local_strong + n.internal_strong) as u32,
            weak_count: n.local_weak as u32,
            ..Default::default()
        })
    }

    // ---- the read path ---------------------------------------------------------

    /// `binder_thread_read`, up to copying out: take work off the thread's
    /// and (if it takes process work) the process's lists and render the
    /// BR_* stream for `size` bytes. Work that renders nothing is consumed
    /// here; the rest is committed or rolled back by the caller.
    fn prepare_read(
        &mut self,
        proc: ProcId,
        tid: u32,
        size: usize,
        nonblock: bool,
        call: Call,
        us: &mut dyn Userspace,
    ) -> Prepared {
        let mut d = Delivery {
            bytes: Vec::new(),
            taken: Vec::new(),
            process_todo: self.procs[&proc].threads[&tid].process_todo,
            spawn_looper: false,
        };
        if size < 4 {
            return Prepared::Ready(d);
        }
        d.bytes.reserve(size.min(512));
        write_pod(&mut d.bytes, &BR_NOOP);
        'retry: loop {
            let (proc_work, has_work) = {
                let p = &self.procs[&proc];
                let t = &p.threads[&tid];
                let w = Self::available_for_proc_work(t);
                (w, Self::has_work(p, t, w))
            };
            if !has_work {
                debug_assert!(d.taken.is_empty());
                return if nonblock {
                    Prepared::Again
                } else {
                    Prepared::Blocked { proc_work }
                };
            }
            loop {
                let from_proc = self.procs[&proc].threads[&tid].todo.is_empty();
                if from_proc && (self.procs[&proc].todo.is_empty() || !proc_work) {
                    if d.bytes.len() == 4 {
                        continue 'retry;
                    }
                    break 'retry;
                }
                if size - d.bytes.len() < MIN_READ_SPACE {
                    break 'retry;
                }
                let work = {
                    let p = self.procs.get_mut(&proc).unwrap();
                    let w = if from_proc {
                        p.todo.pop_front()
                    } else {
                        p.threads.get_mut(&tid).unwrap().todo.pop_front()
                    };
                    let t = p.threads.get_mut(&tid).unwrap();
                    if t.todo.is_empty() {
                        t.process_todo = false;
                    }
                    w.unwrap()
                };
                self.poll_dirty.insert(proc);
                let taken = Taken { work, from_proc };
                let out = &mut d.bytes;
                match work {
                    Work::ReturnError => {
                        write_pod(out, &self.procs[&proc].threads[&tid].return_error)
                    }
                    Work::ReplyError => {
                        write_pod(out, &self.procs[&proc].threads[&tid].reply_error)
                    }
                    Work::TransactionComplete => write_pod(out, &BR_TRANSACTION_COMPLETE),
                    Work::OnewaySpamSuspect => write_pod(out, &BR_ONEWAY_SPAM_SUSPECT),
                    Work::TransactionPendingFrozen => {
                        write_pod(out, &BR_TRANSACTION_PENDING_FROZEN)
                    }
                    Work::FailedReply => {
                        // Like the reply it stands for, it ends the read.
                        write_pod(out, &BR_FAILED_REPLY);
                        d.taken.push(taken);
                        break 'retry;
                    }
                    Work::Node(nid) => {
                        let Some(n) = self.nodes.get(&nid) else {
                            continue;
                        };
                        let ch = Self::node_changes(n);
                        if !ch.reports() {
                            self.apply_node_work(nid);
                            continue;
                        }
                        for (cmd, report) in [
                            (BR_INCREFS, ch.incref),
                            (BR_ACQUIRE, ch.acquire),
                            (BR_RELEASE, ch.release),
                            (BR_DECREFS, ch.decref),
                        ] {
                            if report {
                                write_pod(out, &cmd);
                                write_pod(out, &n.ptr);
                                write_pod(out, &n.cookie);
                            }
                        }
                    }
                    Work::Death(death) => {
                        let Some(death) = self.deaths.get(&death) else {
                            continue;
                        };
                        let dead_binder = death.kind != DeathKind::Clear;
                        write_pod(
                            out,
                            &if dead_binder {
                                BR_DEAD_BINDER
                            } else {
                                BR_CLEAR_DEATH_NOTIFICATION_DONE
                            },
                        );
                        write_pod(out, &death.cookie);
                        if dead_binder {
                            // DEAD_BINDER notifications can cause transactions.
                            d.taken.push(taken);
                            break 'retry;
                        }
                    }
                    Work::Freeze(f) => {
                        let Some(x) = self.freezes.get(&f) else {
                            continue;
                        };
                        if x.cleared {
                            write_pod(out, &BR_CLEAR_FREEZE_NOTIFICATION_DONE);
                            write_pod(out, &x.cookie);
                        } else {
                            write_pod(out, &BR_FROZEN_BINDER);
                            write_pod(
                                out,
                                &BinderFrozenStateInfo {
                                    cookie: x.cookie,
                                    is_frozen: x.is_frozen as u32,
                                    reserved: 0,
                                },
                            );
                            // Like BR_DEAD_BINDER, it can cause transactions.
                            d.taken.push(taken);
                            break 'retry;
                        }
                    }
                    Work::Transaction(txid) => {
                        match self.read_transaction(proc, tid, txid, call, d.taken.is_empty(), us) {
                            TxnRead::Rendered(bytes) => {
                                d.bytes.extend_from_slice(&bytes);
                                d.taken.push(taken);
                                break 'retry;
                            }
                            TxnRead::Dropped => continue,
                            TxnRead::FailedReply => {
                                let failed = Taken {
                                    work: Work::FailedReply,
                                    from_proc,
                                };
                                self.put_back(proc, tid, failed);
                                continue;
                            }
                            TxnRead::NotFirst => {
                                self.put_back(proc, tid, taken);
                                break 'retry;
                            }
                            TxnRead::Interrupted => {
                                d.taken.push(taken);
                                return Prepared::Interrupted(d);
                            }
                        }
                    }
                }
                d.taken.push(taken);
            }
        }
        // Ask for another looper thread if none is idle. (A waiter a signal
        // stopped still counts until it is woken and found gone: that only
        // delays a request, and checking every waiter would cost a syscall
        // on every read.)
        let p = &self.procs[&proc];
        let looper = p.threads[&tid].looper & (LOOPER_REGISTERED | LOOPER_ENTERED) != 0;
        let idle = p.threads.values().any(|t| {
            matches!(
                t.wait,
                Wait::Blocked {
                    proc_work: true,
                    ..
                }
            )
        });
        if p.requested_threads == 0
            && !idle
            && p.requested_threads_started < p.max_threads
            && looper
        {
            d.spawn_looper = true;
            d.bytes[..4].copy_from_slice(&BR_SPAWN_LOOPER.to_le_bytes());
            self.procs.get_mut(&proc).unwrap().requested_threads += 1;
        }
        Prepared::Ready(d)
    }

    /// Put a work item a read took back at the front of its list.
    fn put_back(&mut self, proc: ProcId, tid: u32, taken: Taken) {
        let p = self.procs.get_mut(&proc).unwrap();
        if taken.from_proc {
            p.todo.push_front(taken.work);
        } else {
            let t = p.threads.get_mut(&tid).unwrap();
            t.todo.push_front(taken.work);
            t.process_todo = true;
        }
    }

    /// What a node's work item reports to its owner (BINDER_WORK_NODE in
    /// `binder_thread_read`).
    fn node_changes(n: &Node) -> NodeChanges {
        let (strong, weak) = Self::node_wants(n);
        NodeChanges {
            incref: weak && !n.has_weak_ref,
            acquire: strong && !n.has_strong_ref,
            release: !strong && n.has_strong_ref,
            decref: !weak && n.has_weak_ref,
            free: !strong && !weak,
        }
    }

    /// Apply a node's work item: the owner was told its changes.
    fn apply_node_work(&mut self, nid: NodeId) {
        let Some(n) = self.nodes.get_mut(&nid) else {
            return;
        };
        let ch = Self::node_changes(n);
        n.work_queued = false;
        if ch.incref {
            n.has_weak_ref = true;
            n.pending_weak = true;
            n.local_weak += 1;
        }
        if ch.acquire {
            n.has_strong_ref = true;
            n.pending_strong = true;
            n.local_strong += 1;
        }
        if ch.release {
            n.has_strong_ref = false;
        }
        if ch.decref {
            n.has_weak_ref = false;
        }
        if ch.free {
            self.free_node(nid);
        }
    }

    /// A delivered death notification: a cleared one is done, a dead binder
    /// waits for BC_DEAD_BINDER_DONE.
    fn commit_death(&mut self, proc: ProcId, death: DeathId) {
        if self.deaths[&death].kind == DeathKind::Clear {
            self.deaths.remove(&death);
        } else {
            self.procs
                .get_mut(&proc)
                .unwrap()
                .delivered_death
                .push(death);
        }
    }

    /// Render BR_TRANSACTION(_SEC_CTX) / BR_REPLY for a reader, installing
    /// the buffer's fds into it first (`binder_apply_fd_fixups`). Fds that
    /// can't be installed fail the transaction, which is only done as the
    /// first item of a read, so it never reorders against work already taken.
    fn read_transaction(
        &mut self,
        proc: ProcId,
        tid: u32,
        txid: TxnId,
        call: Call,
        first: bool,
        us: &mut dyn Userspace,
    ) -> TxnRead {
        let Some(t) = self.txns.get(&txid) else {
            return TxnRead::Dropped;
        };
        let Some(key) = t.buffer else {
            self.free_txn(txid);
            return TxnRead::Dropped;
        };
        let (code, flags, euid, from, tokens) = (t.code, t.flags, t.sender_euid, t.from, t.tokens);
        let fixups = self.buffers[&key].fd_fixups.clone();
        let mut installed = 0;
        let mut failure = None;
        for &(off, tok) in &fixups {
            match us.install_fd(proc, call, tok) {
                Ok(fd) => {
                    us.write_recv(proc, key.1 + off as u64, &fd.to_le_bytes());
                    installed += 1;
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        // Installed fds are the reader's now.
        for &(_, tok) in &fixups[..installed] {
            us.drop_fd(tok);
        }
        self.buffers
            .get_mut(&key)
            .unwrap()
            .fd_fixups
            .drain(..installed);
        match failure {
            Some(InstallError::Interrupted) => return TxnRead::Interrupted,
            Some(InstallError::Failed) if !first => return TxnRead::NotFirst,
            Some(InstallError::Failed) => {
                tlog!("proc={proc} fd fixups failed for txn {txid}");
                let reply = self.buffers[&key].target_node.is_none();
                if flags & TF_ONE_WAY == 0 && !reply {
                    self.send_failed_reply(txid, BR_FAILED_REPLY);
                } else {
                    self.free_txn(txid);
                }
                self.free_buf(key, us);
                return if reply {
                    TxnRead::FailedReply
                } else {
                    TxnRead::Dropped
                };
            }
            None => {}
        }
        let b = &self.buffers[&key];
        let mut td = BinderTransactionData::default();
        let cmd = match b.target_node {
            Some(n) => {
                let n = &self.nodes[&n];
                td.target = n.ptr;
                td.cookie = n.cookie;
                if b.secctx != 0 {
                    BR_TRANSACTION_SEC_CTX
                } else {
                    BR_TRANSACTION
                }
            }
            None => BR_REPLY,
        };
        td.code = code;
        td.flags = flags;
        td.sender_euid = euid;
        td.sender_pid = from
            .and_then(|(fp, _)| self.procs.get(&fp))
            .map(|p| p.pid)
            .unwrap_or(0);
        td.data_size = b.data_size as u64;
        td.offsets_size = b.offsets_size as u64;
        td.data_buffer = key.1;
        td.data_offsets = key.1 + align_up(b.data_size, 8) as u64;
        let mut out = Vec::new();
        write_pod(&mut out, &cmd);
        if cmd == BR_TRANSACTION_SEC_CTX {
            write_pod(
                &mut out,
                &BinderTransactionDataSecctx {
                    transaction_data: td,
                    secctx: b.secctx,
                },
            );
        } else {
            write_pod(&mut out, &td);
        }
        if self.ohos {
            let sender_pid_nr = td.sender_pid as u64;
            self.thread_mut(proc, tid).sender = BinderSenderInfo {
                tokens: AccessToken {
                    sender_tokenid: tokens.0,
                    first_tokenid: tokens.1,
                    reserved: [0; 2],
                },
                sender_pid_nr,
            };
        }
        tlog!("read proc={proc} tid={tid} txn={txid} cmd={cmd:#x} code={code:#x}");
        TxnRead::Rendered(out)
    }

    /// The reader has a transaction: its buffer is the reader's to free, and
    /// a synchronous call goes on the reader's stack until it replies.
    fn commit_transaction(&mut self, proc: ProcId, tid: u32, txid: TxnId) {
        let t = &self.txns[&txid];
        let key = t.buffer.unwrap();
        let b = self.buffers.get_mut(&key).unwrap();
        b.allow_user_free = true;
        if b.target_node.is_some() && t.flags & TF_ONE_WAY == 0 {
            let cur = self.procs[&proc].threads[&tid].transaction_stack;
            let tx = self.txns.get_mut(&txid).unwrap();
            tx.to_parent = cur;
            tx.to_thread = Some(tid);
            tx.to_proc = Some(proc);
            self.thread_mut(proc, tid).transaction_stack = Some(txid);
        } else {
            self.free_txn(txid);
        }
    }

    // ---- introspection for tests ----------------------------------------------

    #[cfg(test)]
    pub(crate) fn ref_counts(&self, proc: ProcId, desc: u32) -> Option<(i32, i32)> {
        self.procs
            .get(&proc)?
            .refs_by_desc
            .get(&desc)
            .map(|r| (r.strong, r.weak))
    }

    /// Internal consistency checks between ioctls, used by tests.
    pub fn check_invariants(&self) {
        assert!(self.woken.is_empty(), "woken threads left unserved");
        let mut strong_refs: HashMap<NodeId, i32> = HashMap::new();
        for (&pid, p) in &self.procs {
            for (&d, r) in &p.refs_by_desc {
                assert!(r.strong >= 0 && r.weak >= 0);
                assert_eq!(p.node_to_desc.get(&r.node), Some(&d));
                let n = self.nodes.get(&r.node).expect("ref to freed node");
                assert!(n.refs.contains(&pid));
                if r.strong > 0 {
                    *strong_refs.entry(r.node).or_default() += 1;
                }
            }
            for (&tid, t) in &p.threads {
                match &t.wait {
                    Wait::None => {}
                    // No work is ever left with a thread that waits instead
                    // of reading it, and only threads that can take process
                    // work wait for it.
                    &Wait::Blocked { proc_work, .. } => {
                        assert!(
                            !Self::has_work(p, t, proc_work),
                            "proc {pid} thread {tid} waits with work to read: todo {:?} \
                             (process_todo {}), proc todo {:?} (proc_work {proc_work})",
                            t.todo,
                            t.process_todo,
                            p.todo
                        );
                        assert!(!proc_work || Self::available_for_proc_work(t));
                    }
                    Wait::Woken(_) => panic!("proc {pid} thread {tid} woken but not served"),
                    // A freeze waits for a live target it marked frozen. (As
                    // in the kernel, another BINDER_FREEZE may thaw it
                    // meanwhile; the wait then ends at its deadline.)
                    Wait::Freezing(f) => {
                        assert!(f.enable && f.deadline.is_some());
                        assert!(self.procs.contains_key(&f.targets[0]));
                    }
                }
            }
            // outstanding_txns counts exactly the transactions queued for it
            // or served by it.
            let counted = self
                .txns
                .values()
                .filter(|t| t.outstanding == Some(pid))
                .count();
            assert_eq!(
                p.outstanding_txns as usize, counted,
                "proc {pid} outstanding_txns"
            );
            for f in &p.delivered_freeze {
                let x = &self.freezes[f];
                assert!(x.sent && x.queued);
            }
        }
        // A freeze notification belongs to one ref until cleared, and a
        // cleared one only lives until it is delivered.
        let mut attached = 0;
        for p in self.procs.values() {
            for r in p.refs_by_desc.values() {
                if let Some(f) = r.freeze {
                    assert!(!self.freezes[&f].cleared);
                    attached += 1;
                }
            }
        }
        let cleared = self.freezes.values().filter(|x| x.cleared).count();
        assert_eq!(
            attached + cleared,
            self.freezes.len(),
            "freeze notifications leaked"
        );
        assert!(self.freezes.values().all(|x| !x.cleared || x.queued));
        assert!(self.drained.is_empty());
        for (id, n) in &self.nodes {
            assert!(
                n.internal_strong >= 0 && n.local_strong >= 0 && n.local_weak >= 0,
                "node {id} owner {:?} ptr {:#x}: internal_strong {} local_strong {} local_weak {}",
                n.proc,
                n.ptr,
                n.internal_strong,
                n.local_strong,
                n.local_weak
            );
            // A node's internal strong count is the number of processes
            // holding a strong ref to it, and its ref set is exactly theirs.
            assert_eq!(
                n.internal_strong,
                strong_refs.get(id).copied().unwrap_or(0),
                "node {id}: internal_strong vs strong refs"
            );
            for h in &n.refs {
                assert!(
                    self.procs
                        .get(h)
                        .is_some_and(|p| p.node_to_desc.contains_key(id)),
                    "node {id} lists a ref from proc {h} that it doesn't hold"
                );
            }
        }
    }

    /// Number of live transactions, buffers and nodes (leak checks).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.txns.len(), self.buffers.len(), self.nodes.len())
    }

    /// Freeze notifications alive, and whether any binder instance is
    /// frozen (leak checks).
    pub fn freezer_state(&self) -> (usize, bool) {
        (self.freezes.len(), self.procs.values().any(|p| p.is_frozen))
    }

    /// Work items queued anywhere that a read would return for: process
    /// lists, and thread lists flagged `process_todo`.
    pub fn queued_work(&self) -> usize {
        self.procs
            .values()
            .map(|p| {
                p.todo.len()
                    + p.threads
                        .values()
                        .filter(|t| t.process_todo)
                        .map(|t| t.todo.len())
                        .sum::<usize>()
            })
            .sum()
    }

    /// The threads blocked in a read or a freeze (tests).
    pub fn blocked_threads(&self) -> Vec<(ProcId, u32)> {
        let mut v: Vec<(ProcId, u32)> = self
            .procs
            .iter()
            .flat_map(|(&p, proc)| {
                proc.threads
                    .iter()
                    .filter(|(_, t)| matches!(t.wait, Wait::Blocked { .. } | Wait::Freezing(_)))
                    .map(move |(&tid, _)| (p, tid))
            })
            .collect();
        v.sort();
        v
    }
}

/// Write `bwr` back to the caller of a `BINDER_WRITE_READ`.
fn put_bwr(proc: ProcId, call: Call, bwr: &BinderWriteRead, us: &mut dyn Userspace) -> bool {
    us.write(proc, call.arg, pod_bytes(bwr))
}

/// The (sg offset, length) of a validated PTR object `parent` (an index into
/// the objects already translated).
fn parent_ptr(ptrs: &[Option<(usize, usize)>], parent: u64) -> Option<(usize, usize)> {
    let i = usize::try_from(parent).ok()?;
    *ptrs.get(i)?
}

/// Does the BC_* stream contain `want`? (Walks command sizes; stops at the
/// first unknown command.)
fn has_cmd(buf: &[u8], want: u32) -> bool {
    let mut off = 0;
    while let Some(cmd) = read_pod::<u32>(buf, off) {
        if cmd == want {
            return true;
        }
        off += 4 + ((cmd >> 16) & 0x3fff) as usize;
    }
    false
}
