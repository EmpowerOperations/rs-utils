//! The threads `Marshalled` contexts run on: a process-wide pool of workers, each context pinned to
//! one of them for its whole life.
//!
//! A worker owns the states of every context pinned to it, in a map that never leaves its thread,
//! and runs their jobs one at a time in the order they arrive. So a context's state is still only
//! ever touched by one thread, as when each context had a thread of its own, while the number of
//! threads stays bounded however many contexts are alive. A worker exits once its last context is
//! gone, so no thread outlives the contexts it served.
//!
//! The price of sharing a worker is that one context's long job delays the others pinned beside
//! it, and a job must never wait on other marshalled work: that work may be queued behind it on
//! the same worker, which would then wait forever. Compute is fine; blocking on another context is
//! not, and the blocking calls this crate owns refuse to run on a worker (`refuse_on_pool_worker`).

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak, mpsc};
use std::thread;

/// The environment variable that sets the process-wide pool's size: a positive whole number of
/// threads. Read once, when the first `Marshalled` context is made.
pub const POOL_SIZE_VARIABLE: &str = "EMPOWEROPS_RS_UTILS_THREAD_POOL_SIZE";

/// The pool's size when the variable is unset and the machine's parallelism is unknown.
const FALLBACK_POOL_SIZE: usize = 4;

/// The pool's size, from the variable's value if set, else the machine's available parallelism.
///
/// # Panics
/// If the variable is set to anything but a positive whole number: a mistyped setting is the
/// operator's to fix, and silently ignoring it would hide that it never took effect.
pub fn pool_size(variable: Option<&str>, available: Option<NonZeroUsize>) -> usize {
    match variable {
        Some(text) => match text.trim().parse::<usize>() {
            Ok(size) if size > 0 => size,
            _ => panic!("{POOL_SIZE_VARIABLE} is {text:?}, but must be a positive whole number of threads"),
        },
        None => available.map_or(FALLBACK_POOL_SIZE, NonZeroUsize::get),
    }
}

/// The states of the contexts pinned to one worker, by context id. Lives on that worker only.
pub(crate) type States = HashMap<u64, Box<dyn Any>>;

/// Work for a worker: runs on its thread, against its states.
pub(crate) type Job = Box<dyn FnOnce(&mut States) + Send>;

/// The sending end of one worker. Each context pinned to the worker holds an `Arc` of it; when the
/// last is dropped the channel closes and the worker exits once it has run what was queued.
pub(crate) struct Worker {
    sender: mpsc::Sender<Job>,
}

impl Worker {
    pub(crate) fn send(&self, job: Job) {
        self.sender
            .send(job)
            .expect("AbiThreadMarshaller: a pool worker exited while a context was still pinned to it");
    }
}

struct Slot {
    /// Weak, so the pool never keeps a worker alive: its contexts do.
    worker: Weak<Worker>,
    /// The worker's thread, kept only so tests can watch it exit; otherwise it is detached.
    #[cfg(test)]
    thread: Option<thread::JoinHandle<()>>,
}

pub(crate) struct Pool {
    slots: Mutex<Vec<Slot>>,
}

impl Pool {
    pub(crate) fn new(size: usize) -> Self {
        assert!(size > 0, "a worker pool needs at least one slot");
        let slots = (0..size)
            .map(|_| Slot {
                worker: Weak::new(),
                #[cfg(test)]
                thread: None,
            })
            .collect();
        Self { slots: Mutex::new(slots) }
    }

    /// The process-wide pool, sized by [`POOL_SIZE_VARIABLE`] when first used.
    pub(crate) fn global() -> &'static Pool {
        static GLOBAL: LazyLock<Pool> = LazyLock::new(|| {
            let variable = std::env::var(POOL_SIZE_VARIABLE).ok();
            Pool::new(pool_size(variable.as_deref(), thread::available_parallelism().ok()))
        });
        &GLOBAL
    }

    /// The worker a new context is pinned to: the slot with the fewest live contexts, its worker
    /// started if it has none.
    pub(crate) fn assign(&self) -> Arc<Worker> {
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Every live worker's count includes the upgrade made here, so they compare fairly, and an
        // empty slot counts zero.
        let (index, live) = slots
            .iter()
            .map(|slot| slot.worker.upgrade())
            .enumerate()
            .min_by_key(|(_, worker)| worker.as_ref().map_or(0, Arc::strong_count))
            .expect("a pool has at least one slot");
        if let Some(worker) = live {
            return worker;
        }
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker = Arc::new(Worker { sender });
        let thread = thread::Builder::new()
            .name(format!("rs-utils-marshal-{index}"))
            .spawn(move || run(receiver))
            .expect("AbiThreadMarshaller: could not start a pool worker thread");
        slots[index] = Slot {
            worker: Arc::downgrade(&worker),
            #[cfg(test)]
            thread: Some(thread),
        };
        #[cfg(not(test))]
        drop(thread);
        worker
    }

    /// The thread of slot `index`'s most recent worker, for a test to watch it exit.
    #[cfg(test)]
    pub(crate) fn take_thread(&self, index: usize) -> Option<thread::JoinHandle<()>> {
        let mut slots = self.slots.lock().unwrap();
        slots[index].thread.take()
    }
}

thread_local! {
    // Const-initialised and without a destructor, so it registers nothing with the platform's
    // thread-exit machinery: see `ThreadFingerprint` for why that matters to a library that may be
    // unloaded.
    static ON_POOL_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is one of the pool's workers.
pub(crate) fn on_pool_worker() -> bool {
    ON_POOL_WORKER.with(Cell::get)
}

/// Panics if called on a pool worker: `what` would block it on marshalled work, which may be
/// queued behind the job doing the blocking.
pub(crate) fn refuse_on_pool_worker(what: &str) {
    if on_pool_worker() {
        panic!(
            "AbiThreadMarshaller: {what} called on a pool worker thread -- a marshalled job must not wait on \
             marshalled work, which may be queued behind it on the same worker"
        );
    }
}

fn run(receiver: mpsc::Receiver<Job>) {
    ON_POOL_WORKER.with(|flag| flag.set(true));
    let mut states = States::new();
    for job in receiver {
        job(&mut states);
    }
}

/// A fresh id for a context's state in its worker's map.
pub(crate) fn next_context_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};

    use super::*;

    fn panic_message(f: impl FnOnce() -> usize) -> String {
        let payload = panic::catch_unwind(AssertUnwindSafe(f)).expect_err("expected a panic");
        match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(payload) => (*payload.downcast::<&str>().expect("a string panic payload")).to_owned(),
        }
    }

    #[test]
    fn when_the_variable_is_unset_should_size_the_pool_by_available_parallelism() {
        // act
        let size = pool_size(None, NonZeroUsize::new(6));

        // assert
        assert_eq!(size, 6);
    }

    #[test]
    fn when_the_variable_is_unset_and_parallelism_unknown_should_fall_back_to_four() {
        // act
        let size = pool_size(None, None);

        // assert
        assert_eq!(size, 4);
    }

    #[test]
    fn when_the_variable_is_a_positive_number_should_use_it() {
        // act
        let size = pool_size(Some(" 3 "), NonZeroUsize::new(6));

        // assert
        assert_eq!(size, 3);
    }

    #[test]
    fn when_the_variable_is_zero_or_not_a_number_should_panic_naming_it() {
        for value in ["0", "lots", "-2", ""] {
            // act
            let message = panic_message(|| pool_size(Some(value), NonZeroUsize::new(6)));

            // assert
            assert_eq!(
                message,
                format!("EMPOWEROPS_RS_UTILS_THREAD_POOL_SIZE is {value:?}, but must be a positive whole number of threads")
            );
        }
    }
}
