//! Remembers which raw pointers were handed across the C ABI, so a pointer a caller hands back can
//! be checked before it is dereferenced or freed.

use std::collections::HashSet;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard};

/// The set of `*mut T` currently handed out to a C caller.
///
/// Like a library's loan book: [`check_out`](Self::check_out) records a pointer as it leaves Rust,
/// [`check_in`](Self::check_in) records its return (after which it may be freed), and
/// [`is_checked_out`](Self::is_checked_out) answers whether a pointer the caller just passed in is
/// one of ours and still live. Checking before dereferencing turns a forged, stale or
/// double-freed pointer into a refusal instead of a SIGSEGV.
///
/// Only addresses are stored, never a `T`, so the tag is `fn() -> T` rather than `T`: the
/// registry is `Send`/`Sync` whatever `T` is, and can sit in a `static` beside `!Send` values.
///
/// An address says nothing about *which* allocation it came from. Once a pointer is checked in
/// and freed, the allocator may hand the same address out again, and a caller still holding the
/// old copy will pass `is_checked_out` against the new one. This catches forgery and double free,
/// not every use-after-free.
pub struct PointerRegistry<T> {
    addresses: Mutex<HashSet<usize>>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Default for PointerRegistry<T> {
    fn default() -> Self { Self::new() }
}

impl<T> PointerRegistry<T> {
    pub fn new() -> Self {
        Self { addresses: Mutex::new(HashSet::new()), _marker: PhantomData }
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<usize>> {
        self.addresses.lock().expect("PointerRegistry mutex poisoned")
    }

    /// Records `raw_ptr` as handed out to the caller and returns it unchanged, so a return
    /// statement can wrap it: `REGISTRY.check_out(Box::into_raw(model))`.
    ///
    /// # Panics
    /// If `raw_ptr` is null, or is already checked out. Both are bugs on the Rust side of the
    /// boundary -- the caller has not been given anything yet.
    pub fn check_out(&self, raw_ptr: *mut T) -> *mut T {
        assert!(!raw_ptr.is_null(), "PointerRegistry::check_out: null pointer");
        let inserted = self.lock().insert(raw_ptr as usize);
        assert!(inserted, "PointerRegistry::check_out: {raw_ptr:p} is already checked out");
        raw_ptr
    }

    /// Records that the caller is finished with `raw_ptr`. `true` if it was checked out -- the
    /// caller's to free, now ours to free -- and `false` otherwise: never handed out, already
    /// checked in, or null (so C's `free(NULL)` idiom is a quiet no-op).
    pub fn check_in(&self, raw_ptr: *mut T) -> bool {
        self.lock().remove(&(raw_ptr as usize))
    }

    /// `true` if `raw_ptr` was returned by [`check_out`](Self::check_out) and has not since been
    /// [`check_in`](Self::check_in)'d. Always `false` for null, which is never checked out.
    pub fn is_checked_out(&self, raw_ptr: *const T) -> bool {
        self.lock().contains(&(raw_ptr as usize))
    }

    pub fn is_empty(&self) -> bool { self.lock().is_empty() }

    /// Forgets every checked-out pointer without freeing any of them.
    pub fn clear(&self) { self.lock().clear() }
}

impl<T> Debug for PointerRegistry<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let addresses = self.addresses.lock().map_err(|_| fmt::Error)?;
        f.debug_struct("PointerRegistry").field("addresses", &addresses).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    #[test]
    fn when_a_pointer_is_checked_out_should_report_it_checked_out_until_it_is_checked_in() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let mut value = 7u32;
        let ptr: *mut u32 = &mut value;

        // act
        let returned = registry.check_out(ptr);
        let while_out = registry.is_checked_out(ptr);
        let checked_in = registry.check_in(ptr);
        let after_in = registry.is_checked_out(ptr);

        // assert
        assert_eq!(returned, ptr, "check_out must hand back the pointer it was given");
        assert!(while_out);
        assert!(checked_in);
        assert!(!after_in);
        assert!(registry.is_empty());
    }

    #[test]
    fn when_a_pointer_was_never_checked_out_should_refuse_it() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let (mut ours, mut forged) = (1u32, 2u32);
        registry.check_out(&mut ours);

        // act
        let is_out = registry.is_checked_out(&forged);
        let checked_in = registry.check_in(&mut forged);

        // assert
        assert!(!is_out);
        assert!(!checked_in, "checking in a pointer we never handed out must not succeed");
        assert!(registry.is_checked_out(&ours), "refusing a forgery must not disturb the others");
    }

    #[test]
    fn when_a_pointer_is_checked_in_twice_should_refuse_the_second() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let mut value = 7u32;
        let ptr: *mut u32 = &mut value;
        registry.check_out(ptr);

        // act
        let first = registry.check_in(ptr);
        let second = registry.check_in(ptr);

        // assert
        assert!(first);
        assert!(!second, "a double free must be refused");
    }

    #[test]
    fn when_given_null_should_never_report_it_checked_out() {
        // setup
        let registry = PointerRegistry::<u32>::new();

        // act
        let is_out = registry.is_checked_out(std::ptr::null());
        let checked_in = registry.check_in(std::ptr::null_mut());

        // assert
        assert!(!is_out);
        assert!(!checked_in);
    }

    #[test]
    #[should_panic(expected = "null pointer")]
    fn when_null_is_checked_out_should_panic() {
        // setup
        let registry = PointerRegistry::<u32>::new();

        // act
        registry.check_out(std::ptr::null_mut());
    }

    #[test]
    #[should_panic(expected = "already checked out")]
    fn when_a_pointer_is_checked_out_twice_should_panic() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let mut value = 7u32;
        let ptr: *mut u32 = &mut value;
        registry.check_out(ptr);

        // act
        registry.check_out(ptr);
    }

    #[test]
    fn when_an_address_is_reused_after_check_in_should_check_it_out_again() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let mut value = 7u32;
        let ptr: *mut u32 = &mut value;
        registry.check_out(ptr);
        registry.check_in(ptr);

        // act
        registry.check_out(ptr);

        // assert
        assert!(registry.is_checked_out(ptr), "an allocator may legitimately reuse an address");
    }

    #[test]
    fn when_cleared_should_forget_every_pointer() {
        // setup
        let registry = PointerRegistry::<u32>::new();
        let (mut a, mut b) = (1u32, 2u32);
        registry.check_out(&mut a);
        registry.check_out(&mut b);

        // act
        registry.clear();

        // assert
        assert!(registry.is_empty());
        assert!(!registry.is_checked_out(&a));
        assert!(!registry.is_checked_out(&b));
    }

    #[test]
    fn when_the_pointee_is_not_send_should_still_be_send_and_sync() {
        // setup
        fn assert_send_sync<S: Send + Sync>() {}

        // act, assert: `Rc` is neither Send nor Sync, and only its address is ever stored
        assert_send_sync::<PointerRegistry<Rc<()>>>();
    }
}
