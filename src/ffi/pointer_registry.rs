use std::collections::HashSet;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard};

pub struct PointerRegistry<T> {
    set: Mutex<HashSet<usize>>,
    _marker: PhantomData<T>,
}

/// Type to aide in managing raw pointers passed across the FFI boundary
impl<T> PointerRegistry<T> {
    pub fn new() -> Self {
        Self {
            set: Mutex::new(HashSet::new()),
            _marker: PhantomData,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<usize>> {
        self.set.lock().expect("PointerRegistry mutex poisoned")
    }

    // 'checked-out' pointers are values returned by surro across the FFI boundary.
    // like checking out a book from a library, the idea here is that the returned pointer is unmanaged.
    pub fn check_out(&self, raw_ptr: *mut T) -> *mut T {
        // explicit null check
        let addr = raw_ptr as usize;
        let mut g = self.lock();
        let inserted = g.insert(addr);
        assert!(inserted, "check_out: pointer already checked out");
        return raw_ptr
    }

    // checked-in pointers are values that the FFI caller is explicitly stating it
    // will no longer use (like returning a book to a library),
    // and can thus be freed.
    pub fn check_in(&self, raw_ptr: *mut T) -> bool {
        if raw_ptr.is_null() {
            return false;
        }
        let addr = raw_ptr as usize;
        let mut locked = self.lock();
        let removed = locked.remove(&addr);

        removed
    }

    // checks to see if the pointer is currently chcked out
    // (that is, that the specific value you pass here was provided by [check_out]
    // and has not yet been [check_in]'d.)
    // useful method for avoiding SIGSEGV: judicious use here will prevent you
    // from dereferencing user provided nonsense.
    pub fn is_checked_out(&self, raw_ptr: *const T) -> bool {
        if raw_ptr.is_null() { return false; }
        let addr = raw_ptr as usize;
        return self.lock().contains(&addr);
    }

    pub fn is_empty(&self) -> bool { self.lock().is_empty() }
    pub fn clear(&self) { self.lock().clear() }
}

impl<T> Debug for PointerRegistry<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let set = self.set.lock().map_err(|_| fmt::Error)?;
        f.debug_struct("PointerRegistry")
            .field("pointers", &set)
            .finish()
    }
}
