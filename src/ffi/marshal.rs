//! Thread confinement for state that crosses a C ABI.
//!
//! Rust's `Send`/`Sync` checking cannot follow a pointer through C: the boundary reconstructs a
//! context independently on every call, so nothing stops a C caller handing it to another thread.
//! [`AbiThreadMarshaller`] re-establishes the guarantee at runtime, either by checking the calling
//! thread ([`ThreadStrategy::Direct`]) or by pinning the state to a pool worker
//! ([`ThreadStrategy::Marshalled`]). The state itself may be `!Send`; this module never lets it be
//! touched from anywhere else. State that is `Send` needs none of this: see
//! [`SharedSequential`](super::SharedSequential).

use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use super::worker_pool::{self, Pool, Worker, refuse_on_pool_worker};

/// Wraps a raw pointer so it can be captured into a `marshal_execution`
/// closure. That closure must be `Send` unconditionally -- even for a
/// `Direct` marshaller that will only ever run it on the calling
/// thread -- because the choice of strategy is a runtime value, not
/// something the type system can see. Sound because `marshal_execution`
/// blocks the calling thread for the closure's entire execution: the
/// pointed-to memory (owned by the C/C++/Python caller's own stack frame
/// for the duration of that synchronous call) stays valid and untouched by
/// anyone else regardless of which thread actually dereferences it --
/// exactly as if this were still running on the calling thread directly.
///
/// The field is deliberately private, accessed only via `get()`, not as a
/// public tuple field: Rust's disjoint closure captures (RFC 2229) would
/// otherwise let a `move` closure that only ever writes `x.0` capture just
/// that raw-pointer *field* directly, bypassing this wrapper's `unsafe impl
/// Send` entirely (the thing actually captured would be `*const T`, not
/// `SendPtr<T>`). A method call forces the whole wrapper to be captured
/// instead, since method resolution isn't a place-projection the way field
/// access is.
pub struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> SendPtr<T> {
    pub fn new(ptr: *const T) -> Self { Self(ptr) }
    pub fn get(&self) -> *const T { self.0 }
}
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self { *self }
}
impl<T> Copy for SendPtr<T> {}

/// Mutable counterpart to `SendPtr`. Same justification, including for
/// `get()` over a public field.
pub struct SendMutPtr<T>(*mut T);
unsafe impl<T> Send for SendMutPtr<T> {}
impl<T> SendMutPtr<T> {
    pub fn new(ptr: *mut T) -> Self { Self(ptr) }
    pub fn get(&self) -> *mut T { self.0 }
}
impl<T> Clone for SendMutPtr<T> {
    fn clone(&self) -> Self { *self }
}
impl<T> Copy for SendMutPtr<T> {}

/// An identifier for the calling thread, cheap enough to take on every call and -- unlike
/// [`std::thread::ThreadId`] -- carrying no thread-local state.
///
/// `thread::current().id()` looks like the obvious choice and is the wrong one here. Asking for it
/// initialises std's thread-identity machinery, which registers a destructor through
/// `pthread_key_create`; that destructor is a pointer *into this cdylib*, and glibc does not
/// refcount the owning library for that registration path. A host that `dlclose`s us after any
/// call has been made is then left with libc invoking a function in unmapped memory when the
/// calling thread exits. It presents as a SIGSEGV with no frame of ours on the stack.
///
/// The platform handles below are a register read (`fs:0` on x86-64) and a TEB read respectively:
/// no allocation, no key, nothing registered, nothing to outlive an unload.
///
/// **The tradeoff is uniqueness.** `ThreadId` is unique for the life of the process; these values
/// are recycled once a thread exits. So a marshaller whose creating thread has since died could be
/// used from a *different* thread that inherited its handle, and this check would wrongly pass.
/// That is a best-effort guard where the previous one was exact -- accepted deliberately, because
/// a caller in that position is already using a context whose owner is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadFingerprint(u64);

impl ThreadFingerprint {
    /// The calling thread's handle.
    pub fn current() -> Self {
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn pthread_self() -> usize;
            }
            ThreadFingerprint(unsafe { pthread_self() } as u64)
        }
        #[cfg(windows)]
        {
            unsafe extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            ThreadFingerprint(unsafe { GetCurrentThreadId() } as u64)
        }
    }
}

/// Returned when a `Direct` marshaller is used from a thread other than the
/// one that created it -- a statically knowable condition, checked before
/// anything else happens (no lock taken, nothing to unwind), so it's
/// reported as a normal value instead of a panic. Carries both thread ids
/// so a caller's own diagnostics can report which threads were involved.
#[derive(Debug, Clone, Copy)]
pub struct WrongThreadError {
    pub owner_thread: ThreadFingerprint,
    pub actual_thread: ThreadFingerprint,
}
impl std::fmt::Display for WrongThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AbiThreadMarshaller used from thread {:?}, but it was created on thread {:?} -- \
             a Direct marshaller may only be used from the thread that created it",
            self.actual_thread, self.owner_thread,
        )
    }
}
impl std::error::Error for WrongThreadError {}

fn check_owner_thread(owner_thread: ThreadFingerprint) -> Result<(), WrongThreadError> {
    let actual_thread = ThreadFingerprint::current();
    if actual_thread == owner_thread {
        Ok(())
    } else {
        Err(WrongThreadError { owner_thread, actual_thread })
    }
}

/// How an [`AbiThreadMarshaller`] keeps its state on one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStrategy {
    /// Every call must come from the one thread that created the marshaller;
    /// `marshal_execution` runs the closure right there, under a mutex. The
    /// right choice when the C caller is single-threaded, or makes one
    /// blocking call at a time on its own thread: it costs no more than a
    /// plain mutex.
    Direct,
    /// The state is pinned to one worker of a process-wide pool, which builds
    /// it, owns it and runs every call on it; `marshal_execution` ships each
    /// closure there and blocks for the result. Safe to call, and to drop,
    /// from any thread.
    ///
    /// The pool has [`POOL_SIZE_VARIABLE`](super::POOL_SIZE_VARIABLE)
    /// workers if that environment variable is set, else one per available CPU;
    /// a worker starts when a context is first pinned to it and exits when its
    /// last context is dropped. Contexts sharing a worker take turns, so a
    /// closure must never wait on another marshalled call: that call may be
    /// queued behind it on the same worker. Calling `marshal_execution` (or
    /// making a `Marshalled` marshaller) from inside a marshalled closure
    /// panics rather than risk that.
    Marshalled,
}

enum Backend<T> {
    Direct {
        owner_thread: ThreadFingerprint,
        state: Mutex<T>,
    },
    Marshalled {
        /// The worker the state is pinned to. Holding it keeps the worker alive.
        worker: Arc<Worker>,
        /// The state's key in that worker's map.
        id: u64,
        /// The state's type, which lives on the worker, not here.
        state: PhantomData<fn() -> T>,
    },
}

/// Owns a `T` and only ever lets it be touched from one thread at a time, and
/// always the same one: for `Direct`, the thread that created the marshaller
/// (checked on every call); for `Marshalled`, the pool worker it is pinned to.
///
/// `T` is built by the `init` closure given to [`new`](Self::new), *on* the
/// thread that will own it, so `T` itself never has to be `Send`.
pub struct AbiThreadMarshaller<T> {
    backend: Backend<T>,
}

// SAFETY: `T` may be genuinely `!Send` (e.g. a store holding rogare's
// `SteppableTraining`, which is deliberately `Rc`/`RefCell`-based
// single-threaded machinery) -- so the marshaller can't auto-derive
// Send/Sync, and legitimately shouldn't in the general case. But `T` is
// never actually touched from more than one thread: `Direct` enforces it via
// `owner_thread`'s runtime check in `marshal_execution` (a misuse returns
// `Err`, it doesn't race), and `Marshalled` by construction -- `T` is built
// on, only ever reached from, and dropped on the one pool worker it is pinned
// to. The marshaller *handle* itself (a `ThreadFingerprint`, a `Mutex`, an
// `Arc` of a worker's sender and an id) has nothing unsound about crossing
// threads; only the payload behind the gate would be, and the gate is what
// prevents that.
//
// The one path the gate does not cover is `Drop` of a `Direct` marshaller,
// which drops `T` on whichever thread drops the marshaller. Callers that can
// be torn down from C check `owner_check` first.
unsafe impl<T> Send for AbiThreadMarshaller<T> {}
unsafe impl<T> Sync for AbiThreadMarshaller<T> {}

impl<T: 'static> AbiThreadMarshaller<T> {
    /// Builds the state with `init` on the thread that will own it -- the
    /// calling thread for `Direct`, its pool worker for `Marshalled` (waiting
    /// for it there, so a panic in `init` reaches the caller).
    ///
    /// Anything the consumer needs alongside its handles -- a licensor, a
    /// config -- belongs in `T`, so this type stays ignorant of what any
    /// particular consumer's state is.
    pub fn new(strategy: ThreadStrategy, init: impl FnOnce() -> T + Send + 'static) -> Self {
        let backend = match strategy {
            ThreadStrategy::Direct => Backend::Direct {
                owner_thread: ThreadFingerprint::current(),
                state: Mutex::new(init()),
            },
            ThreadStrategy::Marshalled => return Self::marshalled_in(Pool::global(), init),
        };
        Self { backend }
    }

    /// A `Marshalled` marshaller pinned to a worker of `pool`: the process-wide
    /// pool in production, a private one in tests.
    pub(crate) fn marshalled_in(pool: &Pool, init: impl FnOnce() -> T + Send + 'static) -> Self {
        refuse_on_pool_worker("AbiThreadMarshaller::new");
        let worker = pool.assign();
        let id = worker_pool::next_context_id();
        let (tx, rx) = mpsc::channel::<thread::Result<()>>();
        worker.send(Box::new(move |states| {
            let built = panic::catch_unwind(AssertUnwindSafe(init)).map(|state| {
                states.insert(id, Box::new(state));
            });
            let _ = tx.send(built);
        }));
        match rx.recv().expect("AbiThreadMarshaller: worker thread dropped without responding") {
            Ok(()) => Self { backend: Backend::Marshalled { worker, id, state: PhantomData } },
            Err(payload) => panic::resume_unwind(payload),
        }
    }

    /// `Err` if this is `Direct` and the calling thread is not its owner;
    /// always `Ok` for `Marshalled`, which has no per-thread restriction.
    ///
    /// For callers about to drop the marshaller: dropping a `Direct` one runs
    /// `T`'s destructors on the dropping thread.
    pub fn owner_check(&self) -> Result<(), WrongThreadError> {
        match &self.backend {
            Backend::Direct { owner_thread, .. } => check_owner_thread(*owner_thread),
            Backend::Marshalled { .. } => Ok(()),
        }
    }

    /// Runs `f` against the state and returns its result -- either directly,
    /// on the calling thread (`Direct`, which must be the thread that created
    /// this marshaller), or marshalled onto the worker thread and blocked on
    /// (`Marshalled`, safe to call from any thread).
    ///
    /// `f` must be `Send + 'static` unconditionally, even for `Direct` -- the
    /// strategy is a runtime choice, not something the type system can see, so
    /// the bound has to hold for both. A raw pointer captured from the C ABI
    /// needs `SendPtr`/`SendMutPtr` to satisfy this; see their own doc comments
    /// for why that's sound.
    ///
    /// A panic inside `f` propagates to the caller either way: directly, for
    /// `Direct`; caught on the worker thread and re-raised on the calling
    /// thread via `resume_unwind` for `Marshalled`, so one panicking call
    /// doesn't take down the worker (and thus every later call) along with it.
    ///
    /// # Errors
    /// Returns `Err(WrongThreadError)` if this is `Direct` and called from a
    /// different thread than the one that created it -- checked before the
    /// underlying `Mutex` is ever locked, so this never contends with or
    /// disturbs poison-recovery on that lock.
    pub fn marshal_execution<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> Result<R, WrongThreadError> {
        match &self.backend {
            Backend::Direct { owner_thread, state } => {
                check_owner_thread(*owner_thread)?;
                let mut guard = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                Ok(f(&mut guard))
            }
            Backend::Marshalled { worker, id, .. } => {
                refuse_on_pool_worker("marshal_execution");
                let id = *id;
                let (tx, rx) = mpsc::channel::<thread::Result<R>>();
                worker.send(Box::new(move |states| {
                    let state = states
                        .get_mut(&id)
                        .and_then(|state| state.downcast_mut::<T>())
                        .expect("AbiThreadMarshaller: a pinned context's state is missing from its worker");
                    let _ = tx.send(panic::catch_unwind(AssertUnwindSafe(|| f(state))));
                }));
                match rx.recv().expect("AbiThreadMarshaller: worker thread dropped without responding") {
                    Ok(r) => Ok(r),
                    Err(payload) => panic::resume_unwind(payload),
                }
            }
        }
    }
}

impl<T> Drop for AbiThreadMarshaller<T> {
    /// For `Marshalled`, removes the state from its worker and drops it there, waiting until that is
    /// done, so destruction is finished when this returns. A panic in the state's destructor is
    /// contained on the worker, which carries on serving its other contexts.
    ///
    /// The one exception is a drop *on* a pool worker -- a state that owns another marshaller, say.
    /// Waiting there could be waiting on the very worker doing the dropping, so the removal is queued
    /// behind the current job instead, and runs as soon as it finishes.
    fn drop(&mut self) {
        let Backend::Marshalled { worker, id, .. } = &self.backend else { return };
        let id = *id;
        let (tx, rx) = mpsc::channel::<()>();
        worker.send(Box::new(move |states| {
            let removed = states.remove(&id);
            let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(removed)));
            let _ = tx.send(());
        }));
        if !worker_pool::on_pool_worker() {
            let _ = rx.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::*;

    /// The point of `Marshalled`: callable from any thread, with calls from
    /// different threads still serialized by the one worker (no lost updates).
    #[test]
    fn when_marshalled_is_used_from_many_threads_should_serialize_every_call() {
        // setup
        let marshaller = Arc::new(AbiThreadMarshaller::new(ThreadStrategy::Marshalled, || 0u64));

        // act
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let marshaller = Arc::clone(&marshaller);
                thread::spawn(move || {
                    for _ in 0..100 {
                        marshaller.marshal_execution(|count| *count += 1).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("caller thread itself must not panic");
        }

        // assert
        assert_eq!(marshaller.marshal_execution(|count| *count).unwrap(), 800);
    }

    /// `T` is built on the worker and never leaves it, so it need not be `Send`. That this
    /// compiles with an `Rc` is half the test.
    #[test]
    fn when_marshalled_init_builds_non_send_state_should_serve_it_on_the_worker() {
        // setup
        let marshaller = AbiThreadMarshaller::new(ThreadStrategy::Marshalled, || Rc::new(RefCell::new(1u32)));

        // act
        marshaller.marshal_execution(|state| *state.borrow_mut() += 1).unwrap();
        let value = marshaller.marshal_execution(|state| *state.borrow()).unwrap();

        // assert
        assert_eq!(value, 2);
    }

    /// A panic inside the closure must reach the caller, and must not kill the worker.
    #[test]
    fn when_marshalled_closure_panics_should_propagate_and_keep_the_worker_alive() {
        // setup
        let marshaller = AbiThreadMarshaller::new(ThreadStrategy::Marshalled, || 7u32);

        // act
        let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
            marshaller.marshal_execution(|_state| -> () { panic!("boom") }).unwrap();
        })).is_err();
        let afterwards = marshaller.marshal_execution(|state| *state).unwrap();

        // assert
        assert!(panicked, "the panic inside marshal_execution's closure must propagate");
        assert_eq!(afterwards, 7, "the worker must survive the panic");
    }

    #[test]
    fn when_direct_is_used_from_another_thread_should_return_wrong_thread_error_and_stay_usable() {
        // setup
        let marshaller = Arc::new(AbiThreadMarshaller::new(ThreadStrategy::Direct, || 1u32));

        // act
        let from_elsewhere = thread::spawn({
            let marshaller = Arc::clone(&marshaller);
            move || marshaller.marshal_execution(|state| *state).is_err()
        })
        .join()
        .expect("spawned thread itself must not panic");
        let from_owner = marshaller.marshal_execution(|state| *state);

        // assert
        assert!(from_elsewhere, "a Direct marshaller used from another thread must return Err");
        assert_eq!(from_owner.unwrap(), 1);
    }

    /// A panic while the mutex is held poisons it in the std sense; the next call from the owner
    /// must still succeed.
    #[test]
    fn when_direct_closure_panics_while_locked_should_recover_on_the_next_call() {
        // setup
        let marshaller = AbiThreadMarshaller::new(ThreadStrategy::Direct, || 7u32);

        // act
        let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = marshaller.marshal_execution(|_state| -> () { panic!("simulated panic while locked") });
        })).is_err();
        let afterwards = marshaller.marshal_execution(|state| *state);

        // assert
        assert!(panicked, "the panic inside marshal_execution's closure must propagate");
        assert_eq!(afterwards.unwrap(), 7);
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(payload) => (*payload.downcast::<&str>().expect("a string panic payload")).to_owned(),
        }
    }

    /// The point of the pool: however many contexts there are, they share its workers, spread
    /// across all of them.
    #[test]
    fn when_many_contexts_are_marshalled_should_share_the_pools_workers() {
        // setup
        let pool = Pool::new(4);
        let contexts: Vec<_> = (0..64).map(|n| AbiThreadMarshaller::marshalled_in(&pool, move || n)).collect();

        // act
        let threads: std::collections::HashSet<_> = contexts
            .iter()
            .map(|context| context.marshal_execution(|_| thread::current().id()).unwrap())
            .collect();

        // assert
        assert_eq!(threads.len(), 4, "64 contexts on a pool of 4 must use exactly its 4 workers");
        let values: Vec<u32> = contexts.iter().map(|context| context.marshal_execution(|n| *n).unwrap()).collect();
        assert_eq!(values, (0..64).collect::<Vec<u32>>(), "each context keeps its own state");
    }

    /// Records the thread it is dropped on.
    struct DropWitness {
        dropped_on: Arc<Mutex<Option<thread::ThreadId>>>,
    }

    impl Drop for DropWitness {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(thread::current().id());
        }
    }

    #[test]
    fn when_a_marshalled_context_is_dropped_on_another_thread_should_drop_its_state_on_its_worker() {
        // setup
        let pool = Pool::new(2);
        let dropped_on = Arc::new(Mutex::new(None));
        let context = AbiThreadMarshaller::marshalled_in(&pool, {
            let dropped_on = Arc::clone(&dropped_on);
            move || DropWitness { dropped_on }
        });
        let worker = context.marshal_execution(|_| thread::current().id()).unwrap();

        // act
        thread::spawn(move || drop(context)).join().expect("dropping thread itself must not panic");

        // assert
        assert_eq!(*dropped_on.lock().unwrap(), Some(worker), "dropped on its worker, before drop returned");
    }

    #[test]
    fn when_a_marshalled_closure_waits_on_another_marshalled_context_should_panic_instead_of_deadlocking() {
        // setup
        let pool = Pool::new(1);
        let inner = Arc::new(AbiThreadMarshaller::marshalled_in(&pool, || 1u32));
        let outer = AbiThreadMarshaller::marshalled_in(&pool, || 2u32);

        // act
        let payload = panic::catch_unwind(AssertUnwindSafe(|| {
            outer.marshal_execution(move |_| inner.marshal_execution(|n| *n).unwrap()).unwrap()
        }))
        .expect_err("a nested marshalled call must panic");

        // assert
        assert_eq!(
            panic_message(payload),
            "AbiThreadMarshaller: marshal_execution called on a pool worker thread -- a marshalled job must not \
             wait on marshalled work, which may be queued behind it on the same worker"
        );
        assert_eq!(outer.marshal_execution(|n| *n).unwrap(), 2, "the worker survives the refusal");
    }

    #[test]
    fn when_a_workers_last_context_is_dropped_should_exit_the_worker() {
        // setup
        let pool = Pool::new(1);
        let context = AbiThreadMarshaller::marshalled_in(&pool, || 0u32);
        let worker = pool.take_thread(0).expect("the context started slot 0's worker");

        // act
        drop(context);

        // assert
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(worker.is_finished(), "a worker with no contexts left must exit");
    }

    #[test]
    fn when_a_marshalled_init_panics_should_propagate_and_leave_the_worker_serving() {
        // setup
        let pool = Pool::new(1);
        let survivor = AbiThreadMarshaller::marshalled_in(&pool, || 5u32);

        // act
        let payload = panic::catch_unwind(AssertUnwindSafe(|| {
            AbiThreadMarshaller::<u32>::marshalled_in(&pool, || panic!("simulated failure in init"))
        }))
        .err()
        .expect("a panicking init must propagate");

        // assert
        assert_eq!(panic_message(payload), "simulated failure in init");
        assert_eq!(survivor.marshal_execution(|n| *n).unwrap(), 5);
    }

    #[test]
    fn when_direct_marshaller_owner_checked_from_another_thread_should_err() {
        // setup
        let direct = Arc::new(AbiThreadMarshaller::new(ThreadStrategy::Direct, || ()));
        let marshalled = Arc::new(AbiThreadMarshaller::new(ThreadStrategy::Marshalled, || ()));

        // act
        let (direct_elsewhere, marshalled_elsewhere) = thread::spawn({
            let (direct, marshalled) = (Arc::clone(&direct), Arc::clone(&marshalled));
            move || (direct.owner_check().is_ok(), marshalled.owner_check().is_ok())
        })
        .join()
        .expect("spawned thread itself must not panic");

        // assert
        assert!(!direct_elsewhere, "Direct must refuse a thread that is not its owner");
        assert!(direct.owner_check().is_ok(), "Direct must accept its owner");
        assert!(marshalled_elsewhere, "Marshalled has no owner thread to refuse");
    }
}
