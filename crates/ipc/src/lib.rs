#![cfg_attr(not(test), no_std)]
//! ipc — synchronous endpoints (seL4-style), decoupled from the scheduler.
//!
//! VERIFIED TCB crate. See docs/nucleus-design.md and docs/verification.md.
//!
//! An [`Endpoint`] is a pure rendezvous state machine. It never touches threads or
//! the scheduler directly: every operation returns an [`IpcAction`] describing what
//! the integrator must do to the run queue (wake a thread, block a thread, nothing).
//! This keeps the message-transfer logic — the part that must preserve authority —
//! small, `unsafe`-free, and host-testable.
//!
//! Rendezvous semantics (synchronous, no message buffering beyond the blocked
//! sender's own captured words):
//!   * `send` with a waiting receiver → deliver immediately, wake the receiver.
//!   * `send` with no receiver        → block the sender (its message + words are
//!                                       captured verbatim into the wait queue).
//!   * `recv` with a waiting sender   → deliver that sender's captured message,
//!                                       wake the sender.
//!   * `recv` with no sender          → block the receiver.
//!
//! Authority preservation: a delivered message is *always* a byte-for-byte copy of
//! some real prior `send`'s arguments (`from`, `msg`, `words`). Nothing is forged; a
//! receiver can never be handed a message that no thread actually sent, and the
//! reported sender identity is the true one. See the PROOF(later) notes below.
//! TODO(M1): add pinned Verus support crates and turn the PROOF(later) notes into
//! `verus!{ ... }` lemmas (substantive lemmas start admitted — repo-structure.md §5).

use abi::{MessageInfo, ThreadId};

/// Maximum number of message words transferred inline per IPC. Words beyond this are
/// dropped at capture time (the fixed per-sender buffer is exactly this wide).
pub const MAX_WORDS: usize = 8;

/// What the integrator must do to the scheduler as a result of an IPC operation.
///
/// The endpoint returns intent only; it never mutates thread/scheduler state. The
/// caller of the syscall is always known to the integrator, so `Deliver` only needs
/// to name the *other* party to wake (see [`Endpoint::send`] / [`Endpoint::recv`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpcAction {
    /// A message rendezvoused. `to` receives the message that `from` sent, carrying
    /// `from`'s authority. Exactly one of `{to, from}` is the running caller and the
    /// other was blocked and must be made runnable:
    ///
    ///   * from `send`: `to` was a blocked receiver → wake `to`; `from` is the caller
    ///     and continues; `wake_sender == false`.
    ///   * from `recv`: `from` was a blocked sender → wake `from`; `to` is the caller
    ///     and continues; `wake_sender == true`.
    ///
    /// PROOF(later): `(from, msg, words)` here equals, field-for-field, the arguments
    /// of the `send` call that originated this message — no fabrication, and the
    /// sender's authority (its true `ThreadId`) is preserved unchanged.
    Deliver {
        /// Thread that receives the message.
        to: ThreadId,
        /// Thread that sent the message (whose authority the message carries).
        from: ThreadId,
        /// The sender-supplied message header, verbatim.
        msg: MessageInfo,
        /// The sender-supplied message words, verbatim (zero-padded past the payload).
        words: [u64; MAX_WORDS],
        /// True iff `from` was a previously-blocked sender that must be woken.
        wake_sender: bool,
    },
    /// The operation had no counterpart and the thread was enqueued to wait; the
    /// integrator must remove it from the run queue (block it).
    Block(ThreadId),
    /// The wait queue was full, so the thread could not be blocked and no message was
    /// delivered. The integrator should fault / error-return the operation. This is a
    /// resource limit, never a silent drop of a delivered message.
    QueueFull(ThreadId),
    /// No scheduling action is required. Not produced by `send`/`recv`; reserved for
    /// no-op operations on an endpoint (e.g. a future cancel of an idle endpoint).
    Nothing,
}

/// A sender parked on an endpoint, holding its message verbatim until a receiver
/// arrives. Captured at `send` time so the delivered message is authentic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BlockedSender {
    tid: ThreadId,
    msg: MessageInfo,
    words: [u64; MAX_WORDS],
}

/// Fixed-capacity FIFO ring buffer — no heap. Capacity `N` is a const generic so an
/// endpoint's wait queues live entirely inline.
#[derive(Clone, Copy, Debug)]
struct Queue<T: Copy, const N: usize> {
    buf: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Queue<T, N> {
    fn new() -> Self {
        Queue {
            buf: [None; N],
            head: 0,
            len: 0,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.len == N
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    /// Enqueue at the back. `Err(v)` (returning the item) iff the queue is full.
    fn push_back(&mut self, v: T) -> Result<(), T> {
        if self.is_full() {
            return Err(v);
        }
        // `N > len >= 0`, so `N > 0` here; the modulo is well-defined.
        let idx = (self.head + self.len) % N;
        self.buf[idx] = Some(v);
        self.len += 1;
        Ok(())
    }

    /// Dequeue from the front, or `None` if empty.
    fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        v
    }
}

/// A synchronous IPC endpoint with wait queues of capacity `N`.
///
/// State-machine invariant: at most one of the two queues is ever non-empty — an
/// endpoint holds *either* blocked senders *or* blocked receivers, never both,
/// because a sender and receiver present at the same time rendezvous immediately.
/// PROOF(later): `send_q.is_empty() || recv_q.is_empty()` holds after every operation.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint<const N: usize> {
    /// Senders blocked waiting for a receiver (FIFO). Non-empty ⇒ `recv_q` empty.
    send_q: Queue<BlockedSender, N>,
    /// Receivers blocked waiting for a sender (FIFO). Non-empty ⇒ `send_q` empty.
    recv_q: Queue<ThreadId, N>,
}

impl<const N: usize> Default for Endpoint<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Endpoint<N> {
    /// Create an idle endpoint with empty wait queues.
    pub fn new() -> Self {
        Endpoint {
            send_q: Queue::new(),
            recv_q: Queue::new(),
        }
    }

    /// Wait-queue capacity (per direction).
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// No thread is blocked on this endpoint.
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.send_q.is_empty() && self.recv_q.is_empty()
    }

    /// One or more senders are blocked waiting to be received from.
    #[inline]
    pub fn has_blocked_senders(&self) -> bool {
        !self.send_q.is_empty()
    }

    /// One or more receivers are blocked waiting for a message.
    #[inline]
    pub fn has_blocked_receivers(&self) -> bool {
        !self.recv_q.is_empty()
    }

    /// Number of senders currently blocked on this endpoint.
    #[inline]
    pub fn blocked_sender_count(&self) -> usize {
        self.send_q.len()
    }

    /// Number of receivers currently blocked on this endpoint.
    #[inline]
    pub fn blocked_receiver_count(&self) -> usize {
        self.recv_q.len()
    }

    /// Send `msg` + `words` from thread `from`.
    ///
    /// If a receiver is already waiting, the message is delivered to it immediately
    /// ([`IpcAction::Deliver`] with `wake_sender == false`) and `from` keeps running.
    /// Otherwise `from` is captured (message copied verbatim) and blocked
    /// ([`IpcAction::Block`]), unless the sender queue is full ([`IpcAction::QueueFull`]).
    ///
    /// `words` longer than [`MAX_WORDS`] are truncated to the inline buffer; shorter
    /// slices are zero-padded. The captured copy is exactly what a later `recv` delivers.
    pub fn send(&mut self, from: ThreadId, msg: MessageInfo, words: &[u64]) -> IpcAction {
        let buf = copy_words(words);

        // A waiting receiver ⇒ (by the invariant) no senders are queued; deliver now.
        if let Some(to) = self.recv_q.pop_front() {
            return IpcAction::Deliver {
                to,
                from,
                msg,
                words: buf,
                wake_sender: false,
            };
        }

        // No receiver: park the sender with its message captured verbatim.
        // PROOF(later): the (tid, msg, words) stored here is returned unmodified by the
        // matching `recv`, so no delivered message is ever fabricated.
        match self.send_q.push_back(BlockedSender {
            tid: from,
            msg,
            words: buf,
        }) {
            Ok(()) => IpcAction::Block(from),
            Err(_) => IpcAction::QueueFull(from),
        }
    }

    /// Receive on behalf of thread `by`.
    ///
    /// If a sender is already blocked, its captured message is delivered to `by`
    /// ([`IpcAction::Deliver`] with `wake_sender == true`, naming the sender to wake)
    /// and `by` keeps running. Otherwise `by` is blocked ([`IpcAction::Block`]),
    /// unless the receiver queue is full ([`IpcAction::QueueFull`]).
    pub fn recv(&mut self, by: ThreadId) -> IpcAction {
        // A waiting sender ⇒ (by the invariant) no receivers are queued; deliver now.
        if let Some(s) = self.send_q.pop_front() {
            // PROOF(later): `s` is delivered field-for-field as it was captured in
            // `send`; the reported `from` is the real sender's ThreadId (authority).
            return IpcAction::Deliver {
                to: by,
                from: s.tid,
                msg: s.msg,
                words: s.words,
                wake_sender: true,
            };
        }

        // No sender: park the receiver.
        match self.recv_q.push_back(by) {
            Ok(()) => IpcAction::Block(by),
            Err(_) => IpcAction::QueueFull(by),
        }
    }
}

/// Copy a caller slice into the fixed inline word buffer: first `min(len, MAX_WORDS)`
/// words verbatim, the rest zero. No `unsafe`; truncation is the only lossy step and
/// it never alters the words that *are* carried.
fn copy_words(src: &[u64]) -> [u64; MAX_WORDS] {
    let mut buf = [0u64; MAX_WORDS];
    let n = if src.len() < MAX_WORDS {
        src.len()
    } else {
        MAX_WORDS
    };
    buf[..n].copy_from_slice(&src[..n]);
    buf
}

// ------------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    const TID: fn(usize) -> ThreadId = ThreadId;

    fn padded(words: &[u64]) -> [u64; MAX_WORDS] {
        copy_words(words)
    }

    #[test]
    fn new_endpoint_is_idle() {
        let ep = Endpoint::<4>::new();
        assert!(ep.is_idle());
        assert!(!ep.has_blocked_senders());
        assert!(!ep.has_blocked_receivers());
        assert_eq!(ep.capacity(), 4);
    }

    #[test]
    fn send_then_recv_delivers_exact_message() {
        let mut ep = Endpoint::<4>::new();
        let msg = MessageInfo::new(0xABCD, 3);
        let words = [1u64, 2, 3];

        // No receiver yet → sender blocks.
        assert_eq!(ep.send(TID(1), msg, &words), IpcAction::Block(TID(1)));
        assert!(ep.has_blocked_senders());
        assert_eq!(ep.blocked_sender_count(), 1);

        // Receiver arrives → gets exactly what was sent, and the sender is woken.
        let action = ep.recv(TID(2));
        assert_eq!(
            action,
            IpcAction::Deliver {
                to: TID(2),
                from: TID(1),
                msg,
                words: padded(&words),
                wake_sender: true,
            }
        );
        assert!(ep.is_idle(), "rendezvous should drain both queues");
    }

    #[test]
    fn recv_then_send_delivers() {
        let mut ep = Endpoint::<4>::new();
        let msg = MessageInfo::new(7, 2);
        let words = [10u64, 20];

        // Receiver waits first.
        assert_eq!(ep.recv(TID(9)), IpcAction::Block(TID(9)));
        assert!(ep.has_blocked_receivers());

        // Sender arrives → delivered immediately, sender NOT woken (it is the caller).
        let action = ep.send(TID(3), msg, &words);
        assert_eq!(
            action,
            IpcAction::Deliver {
                to: TID(9),
                from: TID(3),
                msg,
                words: padded(&words),
                wake_sender: false,
            }
        );
        assert!(ep.is_idle());
    }

    #[test]
    fn sender_blocks_when_no_receiver() {
        let mut ep = Endpoint::<4>::new();
        assert_eq!(
            ep.send(TID(1), MessageInfo::new(1, 0), &[]),
            IpcAction::Block(TID(1))
        );
        assert!(ep.has_blocked_senders());
        assert!(!ep.has_blocked_receivers());
    }

    #[test]
    fn receiver_blocks_when_no_sender() {
        let mut ep = Endpoint::<4>::new();
        assert_eq!(ep.recv(TID(5)), IpcAction::Block(TID(5)));
        assert!(ep.has_blocked_receivers());
        assert!(!ep.has_blocked_senders());
    }

    #[test]
    fn sender_queue_full_is_reported() {
        let mut ep = Endpoint::<2>::new();
        let msg = MessageInfo::new(0, 0);
        assert_eq!(ep.send(TID(1), msg, &[]), IpcAction::Block(TID(1)));
        assert_eq!(ep.send(TID(2), msg, &[]), IpcAction::Block(TID(2)));
        // Third sender cannot be parked — capacity is 2.
        assert_eq!(ep.send(TID(3), msg, &[]), IpcAction::QueueFull(TID(3)));
        assert_eq!(ep.blocked_sender_count(), 2, "full queue is not mutated");
    }

    #[test]
    fn receiver_queue_full_is_reported() {
        let mut ep = Endpoint::<1>::new();
        assert_eq!(ep.recv(TID(1)), IpcAction::Block(TID(1)));
        assert_eq!(ep.recv(TID(2)), IpcAction::QueueFull(TID(2)));
        assert_eq!(ep.blocked_receiver_count(), 1);
    }

    #[test]
    fn fifo_order_senders() {
        let mut ep = Endpoint::<4>::new();
        let m1 = MessageInfo::new(11, 1);
        let m2 = MessageInfo::new(22, 1);
        ep.send(TID(1), m1, &[111]);
        ep.send(TID(2), m2, &[222]);

        // First receiver drains the first sender (FIFO).
        assert_eq!(
            ep.recv(TID(100)),
            IpcAction::Deliver {
                to: TID(100),
                from: TID(1),
                msg: m1,
                words: padded(&[111]),
                wake_sender: true,
            }
        );
        // Second receiver drains the second sender.
        assert_eq!(
            ep.recv(TID(101)),
            IpcAction::Deliver {
                to: TID(101),
                from: TID(2),
                msg: m2,
                words: padded(&[222]),
                wake_sender: true,
            }
        );
        assert!(ep.is_idle());
    }

    #[test]
    fn no_message_is_fabricated_on_empty_recv() {
        // recv on an idle endpoint must never manufacture a Deliver.
        let mut ep = Endpoint::<4>::new();
        match ep.recv(TID(1)) {
            IpcAction::Block(t) => assert_eq!(t, TID(1)),
            other => panic!("empty recv forged an action: {other:?}"),
        }
    }

    #[test]
    fn delivered_content_matches_sent_exactly() {
        // Property-ish: whatever bytes go in via send come out via recv, unaltered,
        // with the true sender identity — nothing forged.
        let mut ep = Endpoint::<4>::new();
        let msg = MessageInfo::new(0xDEAD_BEEF, 4);
        let words = [0xAAu64, 0xBB, 0xCC, 0xDD];
        ep.send(TID(42), msg, &words);

        if let IpcAction::Deliver {
            to,
            from,
            msg: got_msg,
            words: got_words,
            wake_sender,
        } = ep.recv(TID(7))
        {
            assert_eq!(to, TID(7));
            assert_eq!(from, TID(42), "sender authority preserved");
            assert_eq!(got_msg, msg, "header not fabricated");
            assert_eq!(got_words, padded(&words), "payload not fabricated");
            assert!(wake_sender);
        } else {
            panic!("expected delivery");
        }
    }

    #[test]
    fn words_longer_than_buffer_are_truncated_not_corrupted() {
        let mut ep = Endpoint::<2>::new();
        let long = [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 10 > MAX_WORDS(8)
        let msg = MessageInfo::new(1, 8);
        ep.send(TID(1), msg, &long);

        if let IpcAction::Deliver { words, .. } = ep.recv(TID(2)) {
            // First MAX_WORDS carried verbatim; overflow dropped, carried words intact.
            assert_eq!(words, [1u64, 2, 3, 4, 5, 6, 7, 8]);
        } else {
            panic!("expected delivery");
        }
    }

    #[test]
    fn short_words_are_zero_padded() {
        let mut ep = Endpoint::<2>::new();
        ep.send(TID(1), MessageInfo::new(0, 1), &[99]);
        if let IpcAction::Deliver { words, .. } = ep.recv(TID(2)) {
            assert_eq!(words, [99u64, 0, 0, 0, 0, 0, 0, 0]);
        } else {
            panic!("expected delivery");
        }
    }

    #[test]
    fn mutual_exclusion_invariant_holds_across_ops() {
        // The endpoint never holds both senders and receivers simultaneously.
        let mut ep = Endpoint::<4>::new();
        let check = |ep: &Endpoint<4>| {
            assert!(
                !(ep.has_blocked_senders() && ep.has_blocked_receivers()),
                "invariant violated: senders and receivers both queued"
            );
        };

        ep.send(TID(1), MessageInfo::new(1, 0), &[]);
        check(&ep);
        ep.send(TID(2), MessageInfo::new(2, 0), &[]);
        check(&ep);
        ep.recv(TID(10)); // drains sender 1
        check(&ep);
        ep.recv(TID(11)); // drains sender 2 → idle
        check(&ep);
        ep.recv(TID(12)); // now a receiver blocks
        check(&ep);
        ep.send(TID(3), MessageInfo::new(3, 0), &[]); // rendezvous
        check(&ep);
        assert!(ep.is_idle());
    }

    #[test]
    fn ring_buffer_wraps_correctly() {
        // Fill, drain, refill to exercise head/tail wraparound.
        let mut ep = Endpoint::<2>::new();
        let m = MessageInfo::new(0, 0);
        ep.send(TID(1), m, &[]);
        ep.send(TID(2), m, &[]);
        assert!(matches!(ep.recv(TID(10)), IpcAction::Deliver { from, .. } if from == TID(1)));
        // Head advanced; enqueue again to force wrap.
        ep.send(TID(3), m, &[]);
        assert!(matches!(ep.recv(TID(11)), IpcAction::Deliver { from, .. } if from == TID(2)));
        assert!(matches!(ep.recv(TID(12)), IpcAction::Deliver { from, .. } if from == TID(3)));
        assert!(ep.is_idle());
    }
}
