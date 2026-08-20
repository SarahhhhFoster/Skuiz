//! Realtime-safe building blocks shared by every adapter.
//!
//! The contract these implement is `docs/concepts/invariants.md`: the audio
//! thread owns the processor, never locks, and never allocates; every other
//! thread reaches it through the queues and the mirror defined here.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// An ordinary realtime command from a non-realtime thread to the audio
/// thread: cheap, bounded work the audio thread drains in full at the top
/// of every block.
///
/// Expensive state operations are deliberately *not* here — they travel on
/// their own queue ([`StateCommand`]) so a flood of parameter moves cannot
/// delay a state op, and a heavyweight state op cannot extend the drain of
/// ordinary commands.
#[derive(Debug)]
pub enum Command {
    /// Apply a parameter change at the top of the next block.
    SetParam {
        /// Parameter id, as declared in `Processor::params`.
        id: u32,
        /// New value; the processor clamps.
        value: f64,
    },
    /// Reset DSP state (delay lines, envelopes, filter memory) between
    /// blocks. Never carries parameter changes.
    Reset,
}

/// A state operation for the audio thread: potentially expensive (the
/// processor (de)serializes its whole state), so these travel on a
/// separate, single-producer queue and the audio thread services at most
/// one per block. Project state can arrive while the transport is stopped
/// *or* running; see the threading docs. `Vec` payloads are recycled back
/// to the main thread after use so the audio thread never frees them.
#[derive(Debug)]
pub enum StateCommand {
    /// Restore project state (a `Processor::load_state` payload).
    LoadState(Vec<u8>),
    /// Serialize project state; the audio thread answers on the instance's
    /// state-response ring with the `Processor::save_state` bytes.
    SaveState,
}

/// One cache line apart so the two threads' counters never share one.
#[repr(align(64))]
struct CacheLine(AtomicUsize);

struct Ring<T> {
    /// Power-of-two slots; `mask = slots.len() - 1` indexes them. UnsafeCell
    /// because both halves reach their slots through the shared `Arc`; the
    /// head/tail protocol guarantees each slot has one accessor at a time.
    slots: Box<[UnsafeCell<std::mem::MaybeUninit<T>>]>,
    mask: usize,
    /// Next slot to write. Owned by the producer, read by the consumer.
    head: CacheLine,
    /// Next slot to read. Owned by the consumer, read by the producer.
    tail: CacheLine,
}

impl<T> Ring<T> {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(
            capacity,
            || UnsafeCell::new(std::mem::MaybeUninit::uninit()),
        );
        Self {
            slots: slots.into_boxed_slice(),
            mask: capacity - 1,
            head: CacheLine(AtomicUsize::new(0)),
            tail: CacheLine(AtomicUsize::new(0)),
        }
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Drop whatever is still inside. Rings are torn down at instance
        // destroy, on the main thread, after processing has stopped — so
        // freeing payloads here never lands on the audio thread.
        let mut tail = *self.tail.0.get_mut();
        let head = *self.head.0.get_mut();
        while tail != head {
            // SAFETY: [tail, head) is exactly the initialized region.
            unsafe { (*self.slots[tail & self.mask].get()).assume_init_drop() };
            tail += 1;
        }
    }
}

/// The pushing half of a single-producer/single-consumer ring. Lock-free.
pub struct Producer<T> {
    ring: Arc<Ring<T>>,
}

/// The popping half. Lock-free.
pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
}

/// A bounded, wait-free SPSC ring. Capacity is fixed at construction;
/// `push` on a full ring hands the value back instead of dropping it
/// silently (invariant 8: the caller decides the overflow policy).
pub fn spsc<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let ring = Arc::new(Ring::new(capacity));
    (Producer { ring: ring.clone() }, Consumer { ring })
}

impl<T> Producer<T> {
    /// Realtime-safe. `Err(value)` means the ring is full.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let head = self.ring.head.0.load(Ordering::Relaxed);
        let tail = self.ring.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) > self.ring.mask {
            return Err(value);
        }
        // SAFETY: only the producer writes this slot, and the full check
        // above proves the consumer is not reading it.
        unsafe {
            (*self.ring.slots[head & self.ring.mask].get()).write(value);
        }
        self.ring.head.0.store(head + 1, Ordering::Release);
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Realtime-safe. `None` means empty.
    pub fn pop(&mut self) -> Option<T> {
        let tail = self.ring.tail.0.load(Ordering::Relaxed);
        let head = self.ring.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // SAFETY: only the consumer reads this slot, and the empty check
        // above proves the producer has initialized it.
        let value = unsafe { (*self.ring.slots[tail & self.ring.mask].get()).assume_init_read() };
        self.ring.tail.0.store(tail + 1, Ordering::Release);
        Some(value)
    }
}

// SAFETY: the head/tail protocol above guarantees each slot has exactly one
// accessor at a time; the Arc just shares the buffer across threads.
unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}
unsafe impl<T: Send> Sync for Producer<T> {}
unsafe impl<T: Send> Sync for Consumer<T> {}

/// How many ordinary commands may be queued for the audio thread before
/// pushes start failing. Overflow policy: the push fails and the caller
/// bumps a diag counter; nothing is dropped silently.
pub const COMMAND_CAPACITY: usize = 1024;

/// How many state commands may be queued for the audio thread. State ops
/// are host-initiated and serialized by the engine (at most one round-trip
/// in flight), so a capacity of two can never overflow: one slot for the
/// in-flight op's command, one spare. See `Engine`'s state-op lock.
pub const STATE_COMMAND_CAPACITY: usize = 2;

/// The producing half of the command queue: cloneable, callable from any
/// non-realtime thread (editor handler, bus callback). The mutex lives on
/// the producer side only — the audio thread never sees it (invariant 2).
#[derive(Clone)]
pub struct CommandProducer {
    inner: Arc<Mutex<Producer<Command>>>,
}

/// The audio thread's half of the command queue.
pub struct CommandConsumer {
    inner: Consumer<Command>,
}

/// A bounded multi-producer/single-consumer command channel.
pub fn command_queue() -> (CommandProducer, CommandConsumer) {
    let (p, c) = spsc(COMMAND_CAPACITY);
    (
        CommandProducer {
            inner: Arc::new(Mutex::new(p)),
        },
        CommandConsumer { inner: c },
    )
}

impl CommandProducer {
    /// Queue a command for the audio thread. `Err` means the queue is
    /// full; the caller's overflow policy applies (count it, drop the
    /// newest). Never call from the audio thread.
    pub fn push(&self, cmd: Command) -> Result<(), Command> {
        let mut p = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        p.push(cmd)
    }
}

impl CommandConsumer {
    /// Realtime-safe. `None` means empty.
    pub fn pop(&mut self) -> Option<Command> {
        self.inner.pop()
    }
}

/// A wait-free mirror of every parameter value, so non-audio threads read
/// parameters without touching the processor (invariant 6).
///
/// The audio thread publishes after applying changes; hosts, editors, and
/// the state snapshot read from here. Reads of a single value are atomic;
/// `snapshot` uses a seqlock so a multi-param snapshot is consistent.
pub struct ParamMirror {
    ids: Vec<u32>,
    values: Vec<AtomicU64>,
    /// Seqlock: odd while a publish is in flight, bumped twice per publish.
    seq: AtomicU64,
}

impl ParamMirror {
    /// One slot per declared parameter, initialized to `initial(id)`.
    pub fn new(defs: &[crate::ParamDef], initial: impl Fn(u32) -> f64) -> Self {
        Self {
            ids: defs.iter().map(|d| d.id).collect(),
            values: defs
                .iter()
                .map(|d| AtomicU64::new(initial(d.id).to_bits()))
                .collect(),
            seq: AtomicU64::new(0),
        }
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.ids.iter().position(|&i| i == id)
    }

    /// Publish one value. Audio thread (or main thread while processing is
    /// stopped) only — there is exactly one publisher at a time.
    pub fn publish(&self, id: u32, value: f64) {
        self.publish_all(&[(id, value)]);
    }

    /// Publish a batch under one seqlock generation. Single publisher.
    pub fn publish_all(&self, changes: &[(u32, f64)]) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s + 1, Ordering::Release);
        for &(id, value) in changes {
            if let Some(i) = self.index_of(id) {
                self.values[i].store(value.to_bits(), Ordering::Relaxed);
            }
        }
        self.seq.store(s + 2, Ordering::Release);
    }

    /// Read one value. Wait-free, any thread. Unknown ids give `None`.
    pub fn get(&self, id: u32) -> Option<f64> {
        self.index_of(id)
            .map(|i| f64::from_bits(self.values[i].load(Ordering::Relaxed)))
    }

    /// A consistent `(id, value)` snapshot of every parameter. Any thread;
    /// retries if a publish raced. The retry bound is generous: a publisher
    /// stalls only if preempted mid-publish, which is short and rare.
    pub fn snapshot(&self) -> Vec<(u32, f64)> {
        let mut retries = 0u32;
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                retries += 1;
                std::hint::spin_loop();
                continue;
            }
            let out: Vec<(u32, f64)> = self
                .ids
                .iter()
                .zip(&self.values)
                .map(|(&id, v)| (id, f64::from_bits(v.load(Ordering::Relaxed))))
                .collect();
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return out;
            }
            retries += 1;
            if retries > 1_000_000 {
                // A million stalled retries means the publisher is dead, not
                // slow; a torn snapshot is less bad than a hung main thread.
                return out;
            }
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParamDef;

    #[test]
    fn ring_round_trips_and_reports_full() {
        let (mut p, mut c) = spsc::<u32>(4);
        assert!(c.pop().is_none());
        for i in 0..4 {
            p.push(i).unwrap();
        }
        assert_eq!(p.push(99), Err(99)); // full: value handed back, not dropped
        assert_eq!(c.pop(), Some(0));
        p.push(4).unwrap(); // wrap-around
        for want in 1..=4 {
            assert_eq!(c.pop(), Some(want));
        }
        assert!(c.pop().is_none());
    }

    #[test]
    fn ring_crosses_threads() {
        let (mut p, mut c) = spsc::<usize>(256);
        let t = std::thread::spawn(move || {
            let mut got = Vec::new();
            while got.len() < 10_000 {
                if let Some(v) = c.pop() {
                    got.push(v);
                }
            }
            got
        });
        for i in 0..10_000 {
            while p.push(i).is_err() {
                std::hint::spin_loop();
            }
        }
        let got = t.join().unwrap();
        assert_eq!(got, (0..10_000).collect::<Vec<_>>());
    }

    #[test]
    fn command_queue_multi_producer() {
        let (prod, mut cons) = command_queue();
        let p2 = prod.clone();
        prod.push(Command::SetParam { id: 1, value: 0.5 }).unwrap();
        p2.push(Command::Reset).unwrap();
        match cons.pop() {
            Some(Command::SetParam { id, value }) => assert_eq!((id, value), (1, 0.5)),
            _ => panic!("expected SetParam"),
        }
        assert!(matches!(cons.pop(), Some(Command::Reset)));
        assert!(cons.pop().is_none());
    }

    #[test]
    fn mirror_publish_and_snapshot() {
        let defs = [
            ParamDef {
                id: 7,
                name: "a",
                min: 0.0,
                max: 1.0,
                default: 0.5,
                choices: &[],
                shared: true,
            },
            ParamDef {
                id: 9,
                name: "b",
                min: 0.0,
                max: 1.0,
                default: 0.0,
                choices: &[],
                shared: true,
            },
        ];
        let m = ParamMirror::new(&defs, |id| if id == 7 { 0.5 } else { 0.0 });
        assert_eq!(m.get(7), Some(0.5));
        assert_eq!(m.get(9), Some(0.0));
        assert_eq!(m.get(42), None);
        m.publish_all(&[(7, 0.75), (9, 1.0)]);
        assert_eq!(m.snapshot(), vec![(7, 0.75), (9, 1.0)]);
    }
}
