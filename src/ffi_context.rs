use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;


/// Wraps a raw pointer so it can be captured into a `marshal_execution`
/// closure. That closure must be `Send` unconditionally -- even for a
/// `Direct`-backed context that will only ever run it on the calling
/// thread -- because the choice of backend is a runtime value, not
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

/// A job dispatched to a `Marshaled` `LockingContext`'s worker thread: does
/// whatever it likes to `inner`, then reports its own result back over
/// whatever channel it closed over -- `marshal_execution` is what actually
/// constructs these.
type Job = Box<dyn FnOnce(&mut LockingContextInner) + Send>;

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
/// are recycled once a thread exits. So a context whose creating thread has since died could be
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

/// Returned by `marshal_execution` when a `Direct`-backed context is used
/// from a thread other than the one that created it -- a statically
/// knowable condition, checked before anything else happens (no lock taken,
/// nothing to unwind), so it's reported as a normal value instead of a
/// panic. Carries both thread ids so a caller's own diagnostics can report
/// which threads were actually involved.
#[derive(Debug, Clone, Copy)]
pub struct WrongThreadError {
    pub owner_thread: ThreadFingerprint,
    pub actual_thread: ThreadFingerprint,
}
impl std::fmt::Display for WrongThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LockingContext used from thread {:?}, but it was created on thread {:?} -- \
             a Direct-backed context may only be used from the thread that created it",
            self.actual_thread, self.owner_thread,
        )
    }
}
impl std::error::Error for WrongThreadError {}

/// Shared by `lock()` and `marshal_execution`'s `Direct` arm so the
/// thread-identity check itself isn't duplicated -- only what each caller
/// *does* with a mismatch (panic vs. return `Err`) differs.
fn check_owner_thread(owner_thread: ThreadFingerprint) -> Result<(), WrongThreadError> {
    let actual_thread = ThreadFingerprint::current();
    if actual_thread == owner_thread {
        Ok(())
    } else {
        Err(WrongThreadError { owner_thread, actual_thread })
    }
}

/// The two ways a `LockingContext` can gate access to its own inner state.
enum Backend {
    /// Every call must come from the one thread that created this context
    /// -- `marshal_execution` runs `f` directly, on the calling thread,
    /// under the mutex. Zero overhead beyond today's plain mutex lock.
    Direct {
        owner_thread: ThreadFingerprint,
        inner: Mutex<LockingContextInner>,
    },
    /// A dedicated background thread owns `LockingContextInner` outright;
    /// `marshal_execution` ships `f` to it over a channel and blocks for
    /// the result. Safe to call from any thread.
    Marshaled {
        /// `None` only during/after `Drop` -- taken and dropped first (to
        /// close the channel) so the worker's receive loop can end before
        /// it's joined.
        sender: Option<mpsc::Sender<Job>>,
        worker: Option<thread::JoinHandle<()>>,
    },
}

/// A `LockingContext` may only ever be accessed the way it was constructed:
/// a `Direct`-backed context only from the thread that created it, a
/// `Marshaled`-backed one from any thread (marshaled onto its own worker
/// thread internally). `Backend::Direct`'s `owner_thread` + the assert in
/// `marshal_execution` enforce the former at runtime: Rust's `Send`/`Sync`
/// type-checking can't catch a misbehaving C caller handing our raw context
/// pointer to a different thread, since the C ABI boundary reconstructs
/// `Arc<LockingContext>` independently on each call from just a pointer.
pub struct LockingContext {
    backend: Backend,
}

// SAFETY: `LockingContextInner`'s generic store can hold genuinely `!Send`
// values (e.g. a session's `SteppableTraining`, which is deliberately
// `Rc`/`RefCell`-based single-threaded machinery) -- so `LockingContext`
// can't auto-derive Send/Sync, and legitimately shouldn't in the general
// case. But that data is never actually touched from more than one
// thread: `Direct` enforces it via `owner_thread`'s runtime assert in
// `marshal_execution` (a misuse panics, it doesn't race), and `Marshaled`
// by construction -- only its one dedicated worker thread ever holds a
// `&mut LockingContextInner`. The `LockingContext`/`Arc<LockingContext>`
// *handle* itself (a `ThreadFingerprint`, a `Mutex`, an `mpsc::Sender`, a
// `JoinHandle`) has nothing unsound about crossing threads; only the
// payload behind the gate would be, and the gate is what prevents that.
unsafe impl Send for LockingContext {}
unsafe impl Sync for LockingContext {}

/// A typed handle into a `LockingContextInner`'s generic store -- pairs a
/// plain `u64` id with a compile-time tag of what type it's supposed to
/// point at. The tag is `fn() -> T`, not `T` directly, so the key stays
/// unconditionally `Copy`/`Send`/`Sync` regardless of `T` (e.g. a consuming
/// crate's `LockingContextKey<SomeSessionType>` must stay `Copy` even when
/// `SomeSessionType` itself is `!Send`, as rogare's own `OngoingTrainingSession`
/// is) -- the key is just a tagged `u64`, never the value itself.
pub struct LockingContextKey<T> {
    id: u64,
    _marker: PhantomData<fn() -> T>,
}

// Manual impls, not `#[derive(..)]`: deriving Clone/Copy/Eq/Hash on a struct
// with a `PhantomData<T>` field adds a `T: Clone`/`T: Copy`/etc bound by
// default, which is exactly what the `fn() -> T` trick above is meant to avoid.
impl<T> Clone for LockingContextKey<T> {
    fn clone(&self) -> Self { *self }
}
impl<T> Copy for LockingContextKey<T> {}
impl<T> PartialEq for LockingContextKey<T> {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}
impl<T> Eq for LockingContextKey<T> {}
impl<T> std::hash::Hash for LockingContextKey<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { self.id.hash(state) }
}
impl<T> std::fmt::Debug for LockingContextKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LockingContextKey<{}>({})", std::any::type_name::<T>(), self.id)
    }
}

impl<T> LockingContextKey<T> {
    /// Reconstructs a key from a raw id -- the only way one of these crosses
    /// the C ABI is as a plain `u64`-newtype handle (e.g.
    /// `EmpowerOpsRogareModelHandle`); this is the trusted-but-verified seam
    /// where a misbehaving caller could hand back the wrong kind of id. See
    /// `LockingContextInner::get`'s downcast check for the runtime guard.
    pub fn from_raw(id: u64) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub fn into_raw(self) -> u64 {
        self.id
    }
}

pub struct LockingContextInner {
    next_id: u64,
    values: HashMap<u64, Box<dyn Any>>,
}

impl LockingContextInner {
    fn new() -> Self {
        Self {
            next_id: 1, // id 0 is reserved as empty
            values: HashMap::new(),
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Stores `value` under a freshly allocated id and returns a typed key
    /// to retrieve it later via `get`/`get_mut`/`take`.
    pub fn insert<T: Any>(&mut self, value: T) -> LockingContextKey<T> {
        let id = self.next_id();
        let prev = self.values.insert(id, Box::new(value));
        assert!(prev.is_none(), "duplicate id {id}? next_id allocation bug");
        LockingContextKey::from_raw(id)
    }

    /// `None` if `key`'s id isn't currently tracked (id `0`, never issued,
    /// or already `take`n).
    ///
    /// # Panics
    /// Panics if `key`'s id is tracked but doesn't hold a `T` -- this can
    /// only happen if a raw handle from a different resource kind (e.g. a
    /// string handle) was reconstructed against the wrong `T` and passed
    /// here; static typing catches every other case.
    pub fn get<T: Any>(&self, key: LockingContextKey<T>) -> Option<&T> {
        if key.id == 0 {
            return None;
        }
        let boxed = self.values.get(&key.id)?;
        Some(boxed.downcast_ref::<T>().unwrap_or_else(|| {
            panic!(
                "LockingContext: id {} is registered but does not hold a {} value \
                 -- likely an id/handle from a different resource kind was passed here",
                key.id,
                std::any::type_name::<T>(),
            )
        }))
    }

    /// Mutable counterpart to `get`; same `None`/panic semantics.
    pub fn get_mut<T: Any>(&mut self, key: LockingContextKey<T>) -> Option<&mut T> {
        if key.id == 0 {
            return None;
        }
        let id = key.id;
        let type_name = std::any::type_name::<T>();
        let boxed = self.values.get_mut(&id)?;
        Some(boxed.downcast_mut::<T>().unwrap_or_else(|| {
            panic!(
                "LockingContext: id {id} is registered but does not hold a {type_name} value \
                 -- likely an id/handle from a different resource kind was passed here",
            )
        }))
    }

    /// Removes and returns the value under `key`, releasing its slot.
    /// `None` if `key`'s id isn't currently tracked. Same panic semantics as
    /// `get` on a type mismatch.
    pub fn take<T: Any>(&mut self, key: LockingContextKey<T>) -> Option<T> {
        if key.id == 0 {
            return None;
        }
        let boxed = self.values.remove(&key.id)?;
        Some(*boxed.downcast::<T>().unwrap_or_else(|_| {
            panic!(
                "LockingContext: id {} is registered but does not hold a {} value \
                 -- likely an id/handle from a different resource kind was passed here",
                key.id,
                std::any::type_name::<T>(),
            )
        }))
    }
}

impl LockingContext {


    /// Creates a context whose state may only be touched from the thread that created it
    /// (enforced at runtime by `marshal_execution`'s `owner_thread` check).
    ///
    /// This is the right choice when the C caller is single-threaded, or makes one blocking
    /// call at a time on its own thread: it costs no more than a plain mutex.
    ///
    /// Anything the consumer needs to keep alongside the context -- a licensor, a session, a
    /// handle table -- goes into the generic store via `insert`, so this crate stays ignorant
    /// of what any particular consumer's state is.
    pub fn new_direct() -> Self {
        let inner_ctx = LockingContextInner::new();
        Self {
            backend: Backend::Direct {
                owner_thread: ThreadFingerprint::current(),
                inner: Mutex::new(inner_ctx),
            },
        }
    }

    /// Same as `new_direct`, but spawns a dedicated background thread
    /// that owns all inner state outright -- `marshal_execution` ships each
    /// operation to it over a channel and blocks for the result, instead of
    /// requiring every call to originate from one specific thread.
    /// `marshal_execution` is safe to call from any thread afterward; the
    /// underlying state itself is still only ever touched by that one
    /// worker thread, one job at a time.
    pub fn new_marshalled() -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker = thread::spawn(move || {
            let mut inner_ctx = LockingContextInner::new();
            for job in receiver {
                job(&mut inner_ctx);
            }
        });
        Self {
            backend: Backend::Marshaled { sender: Some(sender), worker: Some(worker) },
        }
    }

    /// Direct/`Mutex`-based access to this context's inner state -- only
    /// valid for a `Direct`-backed context (i.e. constructed via `new`/
    /// `new_direct`, never `new_marshalled`). Kept
    /// alongside `marshal_execution` for callers (surro, smart-sampler)
    /// that never construct a `Marshaled` context and have no use for
    /// thread marshaling -- their operations are quick and always run
    /// synchronously on the calling thread anyway, so there's no reason to
    /// make their closures pay `marshal_execution`'s uniform `Send` bound.
    ///
    /// # Panics
    /// Panics if called from a thread other than the one that created this
    /// context, or if this context is `Marshaled`-backed (no existing
    /// caller does this; it would be a bug if one started to).
    pub fn lock(&self) -> std::sync::MutexGuard<'_, LockingContextInner> {
        match &self.backend {
            Backend::Direct { owner_thread, inner } => {
                if let Err(err) = check_owner_thread(*owner_thread) {
                    panic!("{err}");
                }
                inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            }
            Backend::Marshaled { .. } => {
                panic!(
                    "LockingContext::lock() called on a Marshaled-backed context -- \
                     use marshal_execution instead"
                );
            }
        }
    }

    /// Runs `f` against this context's inner state and returns its result --
    /// either directly, on the calling thread (`Direct`, which must be the
    /// thread that created this context), or marshaled onto this context's
    /// dedicated worker thread and blocked on (`Marshaled`, safe to call
    /// from any thread), depending on which was chosen at construction.
    ///
    /// `f` must be `Send + 'static` unconditionally, even when this
    /// context happens to be `Direct` -- the backend is a runtime choice,
    /// not something the type system can see, so the bound has to hold for
    /// both. A raw pointer captured from the C ABI needs `SendPtr`/
    /// `SendMutPtr` to satisfy this; see their own doc comments for why
    /// that's sound.
    ///
    /// A panic inside `f` propagates to the caller either way: directly,
    /// for `Direct`; caught on the worker thread and re-raised on the
    /// calling thread via `resume_unwind` for `Marshaled`, so one panicking
    /// call doesn't take down the worker (and thus every later call on
    /// this context) along with it.
    ///
    /// # Errors
    /// Returns `Err(WrongThreadError)` if this is `Direct` and called from
    /// a different thread than the one that created it -- checked before
    /// the underlying `Mutex` is ever locked, so this never contends with
    /// or disturbs poison-recovery on that lock (`Marshaled` has no
    /// per-thread restriction at all and never returns this).
    pub fn marshal_execution<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut LockingContextInner) -> R + Send + 'static,
    ) -> Result<R, WrongThreadError> {
        match &self.backend {
            Backend::Direct { owner_thread, inner } => {
                check_owner_thread(*owner_thread)?;
                let mut guard = inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                Ok(f(&mut guard))
            }
            Backend::Marshaled { sender, .. } => {
                let (tx, rx) = mpsc::channel::<thread::Result<R>>();
                sender
                    .as_ref()
                    .expect("rogare: marshal_execution called while this context is being dropped")
                    .send(Box::new(move |inner: &mut LockingContextInner| {
                        let _ = tx.send(panic::catch_unwind(AssertUnwindSafe(|| f(inner))));
                    }))
                    .expect("rogare: internal worker thread is gone");
                match rx.recv().expect("rogare: internal worker thread dropped without responding") {
                    Ok(r) => Ok(r),
                    Err(payload) => panic::resume_unwind(payload),
                }
            }
        }
    }
}

impl Drop for LockingContext {
    fn drop(&mut self) {
        if let Backend::Marshaled { sender, worker } = &mut self.backend {
            // Drop the sender first to close the channel, so the worker's
            // `for job in receiver` loop actually ends -- otherwise the
            // join below would hang forever waiting for a thread that's
            // still blocked on an empty, open channel.
            drop(sender.take());
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use super::*;

    fn fake_context() -> LockingContext {
        LockingContext::new_direct()
    }

    fn fake_context_multi_threaded() -> LockingContext {
        LockingContext::new_marshalled()
    }

    #[test]
    fn insert_get_get_mut_take_round_trip() {
        let ctx = fake_context();

        let key = ctx.marshal_execution(|inner| inner.insert(42u32)).unwrap();
        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), Some(42));

        ctx.marshal_execution(move |inner| *inner.get_mut(key).unwrap() += 1).unwrap();
        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), Some(43));

        assert_eq!(ctx.marshal_execution(move |inner| inner.take(key)).unwrap(), Some(43));
        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), None);
    }

    #[test]
    fn get_returns_none_for_id_zero_and_unknown_id() {
        let ctx = fake_context();
        let _ = ctx.marshal_execution(|inner| inner.insert(1u32)).unwrap(); // just to move next_id off zero

        assert_eq!(ctx.marshal_execution(|inner| inner.get(LockingContextKey::<u32>::from_raw(0)).copied()).unwrap(), None);
        assert_eq!(ctx.marshal_execution(|inner| inner.get(LockingContextKey::<u32>::from_raw(999)).copied()).unwrap(), None);
    }

    #[test]
    #[should_panic(expected = "does not hold a")]
    fn get_panics_when_id_exists_but_holds_a_different_type() {
        let ctx = fake_context();

        let key = ctx.marshal_execution(|inner| inner.insert(42u32)).unwrap();
        let mismatched_key: LockingContextKey<String> = LockingContextKey::from_raw(key.into_raw());
        let _ = ctx.marshal_execution(move |inner| { inner.get(mismatched_key); });
    }

    /// Proves the actual point of `Marshaled`: unlike `Direct` (which
    /// panics if used from a thread other than the one that created it),
    /// a `Marshaled` context must be safe to call from *any* thread, with
    /// operations from different threads still correctly serialized
    /// against each other (no lost updates, no races) by the one
    /// underlying worker thread.
    #[test]
    fn marshaled_context_is_usable_correctly_from_multiple_threads() {
        let ctx = Arc::new(fake_context_multi_threaded());
        let key = ctx.marshal_execution(|inner| inner.insert(0u64)).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ctx = Arc::clone(&ctx);
                thread::spawn(move || {
                    for _ in 0..100 {
                        ctx.marshal_execution(move |inner| *inner.get_mut(key).unwrap() += 1).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread itself must not panic");
        }

        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), Some(800));
    }

    /// A panic inside `marshal_execution`'s closure must propagate to the
    /// caller, not silently vanish and not poison the worker thread for
    /// later calls on the same context.
    #[test]
    fn marshaled_context_panic_propagates_and_worker_survives() {
        let ctx = fake_context_multi_threaded();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.marshal_execution(|_inner| -> () { panic!("boom") }).unwrap();
        }))
        .is_err();
        assert!(panicked, "the panic inside marshal_execution's closure must propagate to the caller");

        // The worker thread must still be alive and correct afterward.
        let key = ctx.marshal_execution(|inner| inner.insert(7u32)).unwrap();
        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), Some(7));
    }

    /// Unlike the panic this replaced, a `Direct`-backed context used from a
    /// thread other than the one that created it now returns `Err`, not a
    /// panic -- checked before the underlying `Mutex` is ever touched.
    #[test]
    fn direct_context_from_wrong_thread_returns_err_instead_of_panicking() {
        let ctx = Arc::new(fake_context());

        let result = thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || ctx.marshal_execution(|inner| inner.insert(1u32))
        })
        .join()
        .expect("spawned thread itself must not panic");

        assert!(result.is_err(), "a Direct-backed context used from a different thread should return Err, not Ok");

        // Still fully usable from its actual owner thread afterward.
        assert!(ctx.marshal_execution(|inner| inner.insert(2u32)).is_ok());
    }

    /// `lock()` is deliberately left with its original panicking behavior --
    /// only `marshal_execution` was converted to a non-panicking `Result`.
    /// Pins that choice so a future change doesn't silently drift the two
    /// apart without a decision (surro/smart-sampler depend on `lock()`
    /// specifically, and are out of scope for this change).
    #[test]
    fn direct_context_lock_from_wrong_thread_still_panics() {
        let ctx = Arc::new(fake_context());

        let panicked = thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { drop(ctx.lock()); })).is_err()
        })
        .join()
        .expect("spawned thread itself must not panic");

        assert!(panicked, "lock() from a different thread must still panic");
    }

    /// The `Direct`-mode sibling of `marshaled_context_panic_propagates_and_worker_survives`:
    /// a panic inside `marshal_execution`'s closure poisons the underlying
    /// `Mutex` in the std sense (the panic happens *while* the guard is
    /// held, unlike the wrong-thread check above, which runs before any
    /// lock is taken) -- but the next `marshal_execution` call on the same
    /// context, from its owner thread, must still succeed.
    /// `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` is what
    /// recovers from this, and this change must not disturb it.
    #[test]
    fn direct_context_survives_panic_inside_marshal_execution_then_recovers() {
        let ctx = fake_context();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = ctx.marshal_execution(|_inner| -> () {
                panic!("simulated unrelated panic while the lock is held")
            });
        }))
        .is_err();
        assert!(panicked, "the panic inside marshal_execution's closure must propagate to the caller");

        let key = ctx.marshal_execution(|inner| inner.insert(7u32)).unwrap();
        assert_eq!(ctx.marshal_execution(move |inner| inner.get(key).copied()).unwrap(), Some(7));
    }
}
