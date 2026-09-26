//! Owned Rust values behind plain integer handles, for a C caller to hold.
//!
//! Pure data: nothing here knows about threads. What a store may hold is anything `'static`,
//! including `!Send` values, which is why a store is normally kept inside an
//! [`AbiThreadMarshaller`](super::AbiThreadMarshaller) rather than touched directly.

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;

/// A typed handle into a [`HandleStore`] -- pairs a plain `u64` id with a compile-time tag of
/// what type it's supposed to point at. The tag is `fn() -> T`, not `T` directly, so the handle
/// stays unconditionally `Copy`/`Send`/`Sync` regardless of `T` (e.g. a consuming crate's
/// `Handle<SomeSessionType>` must stay `Copy` even when `SomeSessionType` itself is `!Send`, as
/// rogare's own `OngoingTrainingSession` is) -- the handle is just a tagged `u64`, never the
/// value itself.
pub struct Handle<T> {
    id: u64,
    _marker: PhantomData<fn() -> T>,
}

// Manual impls, not `#[derive(..)]`: deriving Clone/Copy/Eq/Hash on a struct
// with a `PhantomData<T>` field adds a `T: Clone`/`T: Copy`/etc bound by
// default, which is exactly what the `fn() -> T` trick above is meant to avoid.
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self { *self }
}
impl<T> Copy for Handle<T> {}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}
impl<T> Eq for Handle<T> {}
impl<T> std::hash::Hash for Handle<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { self.id.hash(state) }
}
impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Handle<{}>({})", std::any::type_name::<T>(), self.id)
    }
}

impl<T> Handle<T> {
    /// Reconstructs a handle from a raw id -- the only way one of these crosses
    /// the C ABI is as a plain `u64`-newtype handle (e.g.
    /// `EmpowerOpsRogareModelHandle`); this is the trusted-but-verified seam
    /// where a misbehaving caller could hand back the wrong kind of id. See
    /// [`HandleStore::get`]'s downcast check for the runtime guard.
    pub fn from_raw(id: u64) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub fn into_raw(self) -> u64 {
        self.id
    }
}

pub struct HandleStore {
    next_id: u64,
    values: HashMap<u64, Box<dyn Any>>,
}

impl Default for HandleStore {
    fn default() -> Self { Self::new() }
}

impl HandleStore {
    pub fn new() -> Self {
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

    /// Stores `value` under a freshly allocated id and returns a typed handle
    /// to retrieve it later via `get`/`get_mut`/`take`.
    pub fn insert<T: Any>(&mut self, value: T) -> Handle<T> {
        let id = self.next_id();
        let prev = self.values.insert(id, Box::new(value));
        assert!(prev.is_none(), "duplicate id {id}? next_id allocation bug");
        Handle::from_raw(id)
    }

    /// `None` if `handle`'s id isn't currently tracked (id `0`, never issued,
    /// or already `take`n).
    ///
    /// # Panics
    /// Panics if `handle`'s id is tracked but doesn't hold a `T` -- this can
    /// only happen if a raw handle from a different resource kind (e.g. a
    /// string handle) was reconstructed against the wrong `T` and passed
    /// here; static typing catches every other case.
    pub fn get<T: Any>(&self, handle: Handle<T>) -> Option<&T> {
        if handle.id == 0 {
            return None;
        }
        let boxed = self.values.get(&handle.id)?;
        Some(boxed.downcast_ref::<T>().unwrap_or_else(|| wrong_type_panic::<T>(handle.id)))
    }

    /// Mutable counterpart to `get`; same `None`/panic semantics.
    pub fn get_mut<T: Any>(&mut self, handle: Handle<T>) -> Option<&mut T> {
        if handle.id == 0 {
            return None;
        }
        let boxed = self.values.get_mut(&handle.id)?;
        Some(boxed.downcast_mut::<T>().unwrap_or_else(|| wrong_type_panic::<T>(handle.id)))
    }

    /// Removes and returns the value under `handle`, releasing its slot.
    /// `None` if `handle`'s id isn't currently tracked. Same panic semantics as
    /// `get` on a type mismatch -- and the type is checked *before* anything is
    /// removed, so a mismatched handle leaves the value it hit in place.
    pub fn take<T: Any>(&mut self, handle: Handle<T>) -> Option<T> {
        if handle.id == 0 {
            return None;
        }
        if !self.values.get(&handle.id)?.is::<T>() {
            wrong_type_panic::<T>(handle.id);
        }
        let boxed = self.values.remove(&handle.id)?;
        Some(*boxed.downcast::<T>().unwrap_or_else(|_| {
            unreachable!("id {} held a {} a moment ago", handle.id, std::any::type_name::<T>())
        }))
    }
}

fn wrong_type_panic<T>(id: u64) -> ! {
    panic!(
        "HandleStore: id {id} is registered but does not hold a {} value \
         -- likely an id/handle from a different resource kind was passed here",
        std::any::type_name::<T>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_values_are_inserted_should_round_trip_through_get_get_mut_and_take() {
        // setup
        let mut store = HandleStore::new();
        let handle = store.insert(42u32);

        // act
        let first = store.get(handle).copied();
        *store.get_mut(handle).unwrap() += 1;
        let taken = store.take(handle);
        let after_take = store.get(handle).copied();

        // assert
        assert_eq!(first, Some(42));
        assert_eq!(taken, Some(43));
        assert_eq!(after_take, None);
    }

    #[test]
    fn when_given_id_zero_or_an_unknown_id_should_return_none() {
        // setup
        let mut store = HandleStore::new();
        let _ = store.insert(1u32); // just to move next_id off zero

        // act
        let zero = store.get(Handle::<u32>::from_raw(0)).copied();
        let unknown = store.get(Handle::<u32>::from_raw(999)).copied();

        // assert
        assert_eq!(zero, None);
        assert_eq!(unknown, None);
    }

    #[test]
    #[should_panic(expected = "does not hold a")]
    fn when_get_is_given_a_handle_of_the_wrong_type_should_panic() {
        // setup
        let mut store = HandleStore::new();
        let handle = store.insert(42u32);
        let mismatched: Handle<String> = Handle::from_raw(handle.into_raw());

        // act
        store.get(mismatched);
    }

    #[test]
    fn when_take_is_given_a_handle_of_the_wrong_type_should_panic_and_leave_the_value_in_place() {
        // setup
        let mut store = HandleStore::new();
        let handle = store.insert(42u32);
        let mismatched: Handle<String> = Handle::from_raw(handle.into_raw());

        // act
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.take(mismatched);
        })).is_err();

        // assert
        assert!(panicked, "a mismatched take must still panic");
        assert_eq!(store.get(handle).copied(), Some(42), "the value it hit must survive");
    }
}
