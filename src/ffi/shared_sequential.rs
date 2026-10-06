//! State any thread may use, one call at a time, for state that is `Send`.
//!
//! [`AbiThreadMarshaller`](super::AbiThreadMarshaller) exists for state that must stay on one
//! thread, because it may be `!Send`. State that is `Send` needs none of that: a mutex already
//! gives it what a C caller calling from arbitrary threads requires, mutual exclusion and a
//! happens-before edge between one call and the next. [`SharedSequential`] is that mutex, with the
//! `Send` bound stated where the state is wrapped, so a consumer declares "my state is `Send`" by
//! naming this type and the compiler checks the claim there.
//!
//! The proof is the compiler's: `Send` is derived field by field, so a `!Send` value anywhere in
//! `T` (an `Rc`, a `RefCell` shared through one, a raw pointer) fails the build at the
//! construction site. What it cannot see through is an `unsafe impl Send` beneath `T` that is
//! wrong; those are the places to audit.

use std::sync::Mutex;

/// A `T` behind a mutex: callable from any thread, one call at a time, dropped on whichever
/// thread drops it.
///
/// `T` must be `Send`. An `Rc` in the state does not compile:
///
/// ```compile_fail
/// use std::rc::Rc;
/// use empower_rs_utils::ffi::SharedSequential;
///
/// let _ = SharedSequential::new(|| Rc::new(1));
/// ```
///
/// while the same state counted atomically does:
///
/// ```
/// use std::sync::Arc;
/// use empower_rs_utils::ffi::SharedSequential;
///
/// let shared = SharedSequential::new(|| Arc::new(1));
/// assert_eq!(shared.execute(|state| **state), 1);
/// ```
pub struct SharedSequential<T: Send> {
    state: Mutex<T>,
}

impl<T: Send> SharedSequential<T> {
    /// Builds the state with `init`, on the calling thread: being `Send`, it may live anywhere.
    pub fn new(init: impl FnOnce() -> T) -> Self {
        Self { state: Mutex::new(init()) }
    }

    /// Runs `f` against the state on the calling thread, holding the lock for its duration.
    ///
    /// Neither `f` nor its result need be `Send`: nothing crosses a thread. A result cannot borrow
    /// from the state either, since `f`'s signature ties no lifetime between the two.
    ///
    /// A panic inside `f` propagates to the caller. It poisons the mutex in the std sense, and the
    /// next call recovers the state as `f` left it, as [`AbiThreadMarshaller`]'s `Direct` backend
    /// does: a panic is the caller's to report, not a reason to refuse every call after it.
    ///
    /// [`AbiThreadMarshaller`]: super::AbiThreadMarshaller
    pub fn execute<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut guard = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::Arc;
    use std::thread::{self, ThreadId};

    use super::*;

    #[test]
    fn when_used_from_many_threads_should_serialize_every_call() {
        // setup
        let shared = Arc::new(SharedSequential::new(|| 0u64));

        // act
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    for _ in 0..100 {
                        shared.execute(|count| *count += 1);
                    }
                })
            })
            .collect();
        for caller in callers {
            caller.join().expect("caller thread itself must not panic");
        }

        // assert
        assert_eq!(shared.execute(|count| *count), 800);
    }

    /// Records the thread it is dropped on.
    struct DropWitness {
        dropped_on: Arc<Mutex<Option<ThreadId>>>,
    }

    impl Drop for DropWitness {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(thread::current().id());
        }
    }

    #[test]
    fn when_dropped_on_another_thread_should_drop_its_state_there() {
        // setup
        let dropped_on = Arc::new(Mutex::new(None));
        let shared = SharedSequential::new({
            let dropped_on = Arc::clone(&dropped_on);
            move || DropWitness { dropped_on }
        });

        // act
        let dropper = thread::spawn(move || drop(shared));
        let dropper_id = dropper.thread().id();
        dropper.join().expect("dropping thread itself must not panic");

        // assert
        assert_eq!(*dropped_on.lock().unwrap(), Some(dropper_id));
    }

    #[test]
    fn when_a_call_panics_should_propagate_it_and_serve_the_next_call() {
        // setup
        let shared = SharedSequential::new(|| 7u32);

        // act
        let payload = panic::catch_unwind(AssertUnwindSafe(|| {
            shared.execute(|state| {
                *state += 1;
                panic!("simulated failure inside execute");
            })
        }))
        .expect_err("the panic inside execute must propagate");
        let afterwards = shared.execute(|state| *state);

        // assert
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"simulated failure inside execute"));
        assert_eq!(afterwards, 8, "the next call sees the state as the panicking call left it");
    }
}
