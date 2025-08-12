//! Considering that `CBCentralManager` is `!Send/!Sync` we want the central manager to
//! logically be owned by a single dispatch queue but `objc2` only has limited bindings
//! for `dispatch_queue_set_specific`, that only let you associate a destructor with a
//! queue - not data.
//!
//! This lets us wrap queue-specific state in a `DispatchSpecific<T>` type which is
//! then boxed and attached to a queue with `dispatch_queue_set_specific`. Since
//! the wrapped state can then only be safely referenced from a task running on the
//! associated queue (via [`DispatchSpecific::with`]) then we know access to
//! the `!Send/!Sync` state is safely serialized.

use anyhow;
use dispatch2::{DispatchObject, DispatchQueue};
use futures::channel::oneshot;
use std::marker::PhantomData;
use std::panic::{resume_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::{ffi::c_void, panic::catch_unwind};

extern "C" {
    fn dispatch_queue_set_specific(
        queue: *const c_void, key: *const c_void, context: *const c_void,
        destructor: Option<extern "C" fn(*const c_void)>,
    );

    fn dispatch_queue_get_specific(queue: *const c_void, key: *const c_void) -> *const c_void;

    fn dispatch_get_specific(key: *const c_void) -> *const c_void;

    fn dispatch_assert_queue(queue: *const c_void);
}

extern "C" fn dispatch_specific_destructor<T>(ptr: *const c_void) {
    if !ptr.is_null() {
        unsafe {
            // Convert back to Box and drop it
            let _boxed: Box<DispatchSpecific<T>> = Box::from_raw(ptr as *mut DispatchSpecific<T>);
            // Box is automatically dropped here
        }
    }
}

/// A wrapper for non-Send types that are logically owned by a specific dispatch queue
pub struct DispatchSpecific<T> {
    inner: T,
    _marker: PhantomData<*const ()>, // !Send + !Sync marker
}

impl<T> DispatchSpecific<T> {
    #[inline(never)]
    fn type_key_fn() {}
    #[inline(always)]
    fn key() -> *const c_void {
        Self::type_key_fn as *const () as *const c_void
    }

    /// Accesses queue-specific data from within a dispatched task
    ///
    /// This is the only safe way to access dispatch-specific data. The closure
    /// is called with a reference to the stored value.
    ///
    /// # Panic
    ///
    /// Will panic if [`Self::attach`] hasn't previously been called to attach
    /// queue-specific data corresponding to `T`
    pub fn with<R>(f: impl FnOnce(&T) -> R) -> R {
        // Get the raw pointer from dispatch-specific storage
        let ptr = unsafe { dispatch_get_specific(Self::key()) };

        if ptr.is_null() {
            panic!("DispatchQueue specific data not found - must call DispatchSpecific::<T>::attach() first");
        } else {
            // SAFETY:
            // - The key pointer is private, so if anything is returned we can
            //   assume it was set via Self::attach and is valid
            // - Access is serialized by the dispatch queue we're currently on
            let dispatch_specific = unsafe { &*(ptr as *const DispatchSpecific<T>) };
            f(&dispatch_specific.inner)
        }
    }

    /// Accesses (mutable) queue-specific data from within a dispatched task
    ///
    /// This is the only safe way to mutably access dispatch-specific data.
    ///
    /// # Panic
    ///
    /// Will panic if [`Self::attach`] hasn't previously been called to attach
    /// queue-specific data corresponding to `T`
    pub fn with_mut<R>(f: impl FnOnce(&mut T) -> R) -> R {
        // Get the raw pointer from dispatch-specific storage
        let ptr = unsafe { dispatch_get_specific(Self::key()) };

        if ptr.is_null() {
            panic!("DispatchQueue specific data not found - must call DispatchSpecific::<T>::attach() first");
        } else {
            // SAFETY:
            // - The key pointer is private, so if anything is returned we can
            //   assume it was set via Self::attach and is valid
            // - Access is serialized by the dispatch queue we're currently on
            let dispatch_specific = unsafe { &mut *(ptr as *mut DispatchSpecific<T>) };
            f(&mut dispatch_specific.inner)
        }
    }

    /// Instantiate some dispatch-queue-specific state, that is only accessible
    /// to tasks running on the queue, via [`Self::with`]
    ///
    /// The value will be dropped when the queue is released.
    ///
    /// Returns `true` if the state was successfully attached, or `false` if
    /// state was already attached to this queue for type `T`.
    ///
    /// # Safety
    ///
    /// This is only safe if the queue was constructed with [`dispatch2::DispatchQueueAttr::SERIAL`]
    /// (Or default `None` attributes) (unfortunately there's no API to explicitly assert this)
    pub unsafe fn attach<F>(queue: &DispatchQueue, init: F) -> bool
    where
        F: FnOnce() -> T,
    {
        // Use a static mutex to prevent race conditions when multiple threads
        // try to attach state to the same per-process dispatch queue
        static ATTACH_MUTEX: Mutex<()> = Mutex::new(());
        let _lock = ATTACH_MUTEX.lock().unwrap();

        // Check if state is already attached to this queue
        let existing_ptr = unsafe {
            dispatch_queue_get_specific(queue.as_raw().as_ptr() as *const c_void, Self::key())
        };

        if !existing_ptr.is_null() {
            // State is already attached
            return false;
        }

        let specific = Self {
            inner: init(),
            _marker: PhantomData,
        };

        // Box the value so we can store a pointer to it
        let boxed = Box::new(specific);
        let ptr = Box::into_raw(boxed) as *const c_void;

        // Store the pointer in queue-specific storage with destructor
        unsafe {
            dispatch_queue_set_specific(
                queue.as_raw().as_ptr() as *const c_void,
                Self::key(),
                ptr,
                Some(dispatch_specific_destructor::<T>),
            );
        }
        true
    }
}

/// Extension trait for [`dispatch2::DispatchQueue`]
pub trait DispatchExt {
    /// A safer alternative to [`DispatchQueue::exec_async`] that wraps the closure in
    /// [`catch_unwind`] to make sure that any panic in Rust code doesn't unwind across
    /// an FFI boundary and will instead be resumed back in Rust land.
    async fn safe_exec_async<R, E, F>(&self, work: F) -> Result<R, crate::Error>
    where
        F: FnOnce() -> Result<R, E> + Send + 'static,
        E: Into<crate::Error> + Send + 'static,
        R: Send + 'static;
}

impl DispatchExt for DispatchQueue {
    async fn safe_exec_async<R, E, F>(&self, work: F) -> Result<R, crate::Error>
    where
        F: FnOnce() -> Result<R, E> + Send + 'static,
        E: Into<crate::Error> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.exec_async(move || {
            // Note: we have to use catch_unwind otherwise we'd hit undefined behavior if we
            // let Rust unwind past an FFI boundary.
            let res = catch_unwind(AssertUnwindSafe(|| work()));
            let _ = tx.send(res);
        });

        match rx.await {
            Ok(Ok(result)) => result.map_err(|e| e.into()),
            Ok(Err(panic_payload)) => resume_unwind(panic_payload),
            Err(_) => Err(crate::Error::Other(anyhow::anyhow!(
                "Dispatch task sender was dropped"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    use super::*;
    use dispatch2::DispatchQueue;
    use futures::channel::oneshot;

    #[test]
    fn test_dispatch_specific_basic() {
        let queue = DispatchQueue::new("test.queue", None);

        // Initialize and attach some queue-specific data
        unsafe { DispatchSpecific::<i32>::attach(&queue, || 42) };

        // Access the data
        queue.exec_sync(|| {
            DispatchSpecific::<i32>::with(|value: &i32| assert_eq!(value, &42));
        });
    }

    #[tokio::test]
    #[should_panic]
    async fn test_dispatch_specific_unattached() {
        let queue = DispatchQueue::new("test.queue2", None);

        // Should panic if attempting to access the data without attaching first
        let _ = queue
            .safe_exec_async(|| -> std::result::Result<(), crate::Error> {
                DispatchSpecific::<i32>::with(|value: &i32| {
                    eprintln!("Shouldn't be reached");
                });
                Ok(())
            })
            .await;
    }

    #[test]
    fn test_dispatch_specific_mutable() {
        let queue = DispatchQueue::new("test.queue3", None);

        struct QueueState {
            data: Vec<i32>,
        }

        // Initialize and attach some queue-specific data
        unsafe {
            DispatchSpecific::<QueueState>::attach(&queue, || QueueState {
                data: vec![1, 2, 3],
            })
        };

        // Mutate the data
        queue.exec_sync(|| {
            DispatchSpecific::with_mut(|value: &mut QueueState| {
                value.data.push(4);
            });
        });

        // Check the mutation
        queue.exec_sync(|| {
            DispatchSpecific::with_mut(|value: &mut QueueState| {
                assert_eq!(value.data, vec![1, 2, 3, 4]);
            });
        });
    }

    #[test]
    fn test_dispatch_specific_double_attach() {
        let queue = DispatchQueue::new("test.queue4", None);

        // First attach should succeed
        let attached1 = unsafe { DispatchSpecific::<i32>::attach(&queue, || 42) };
        assert!(attached1);

        // Second attach should fail and return false
        let attached2 = unsafe { DispatchSpecific::<i32>::attach(&queue, || 100) };
        assert!(!attached2);

        // The original value should still be accessible
        queue.exec_sync(|| {
            DispatchSpecific::<i32>::with(|value: &i32| {
                assert_eq!(value, &42); // Should still be the original value
            });
        });
    }
}
