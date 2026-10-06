//! Machinery for exposing Rust state through a C ABI.
//!
//! * [`handles`] -- owned Rust values behind integer handles a C caller can hold. No threading.
//! * [`marshal`] -- keeps state on one thread however the C caller threads its calls.
//! * [`shared_sequential`] -- lets any thread use `Send` state, one call at a time.
//! * [`pointer_registry`] -- remembers which raw pointers were handed out, to reject forged ones.
//!
//! A consumer normally composes a store with one of the last two: its per-context state type holds
//! a [`HandleStore`] (plus whatever else it needs), owned by an [`AbiThreadMarshaller`]; or, when
//! everything in it is `Send`, a [`SendHandleStore`] owned by a [`SharedSequential`], which any
//! thread may call and drop.

pub mod handles;
pub mod marshal;
pub mod pointer_registry;
pub mod shared_sequential;

pub use handles::{Erase, Erased, Handle, HandleStore, HandleStoreOf, SendHandleStore, WrongKind};
pub use marshal::{AbiThreadMarshaller, SendMutPtr, SendPtr, ThreadFingerprint, ThreadStrategy, WrongThreadError};
pub use pointer_registry::PointerRegistry;
pub use shared_sequential::SharedSequential;
