//! Machinery for exposing Rust state through a C ABI.
//!
//! * [`handles`] -- owned Rust values behind integer handles a C caller can hold. No threading.
//! * [`marshal`] -- keeps state on one thread however the C caller threads its calls.
//! * [`pointer_registry`] -- remembers which raw pointers were handed out, to reject forged ones.
//!
//! A consumer normally composes the first two: its per-context state type holds a
//! [`HandleStore`] (plus whatever else it needs), and an [`AbiThreadMarshaller`] owns that state.

pub mod handles;
pub mod marshal;
pub mod pointer_registry;

pub use handles::{Handle, HandleStore};
pub use marshal::{AbiThreadMarshaller, SendMutPtr, SendPtr, ThreadFingerprint, ThreadStrategy, WrongThreadError};
pub use pointer_registry::PointerRegistry;
