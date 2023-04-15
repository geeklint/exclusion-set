//! A lock-free concurrent set that stores `TypeId`s.  The operations on this
//! set are O(n) where n is the number of distinct `TypeId`s that have ever been
//! inserted into the set.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(clippy::all, clippy::pedantic)]

extern crate alloc;

use {
    alloc::boxed::Box,
    core::{any::TypeId, ptr, sync::atomic::Ordering},
};

#[cfg(loom)]
use loom::{sync::atomic, thread};

#[cfg(not(loom))]
use core::sync::atomic;

#[cfg(all(not(loom), feature = "std"))]
use std::thread;

/// A set of `TypeId`s held in a linked list.
#[derive(Default)]
pub struct TypeIdSet {
    head: atomic::AtomicPtr<Node>,
}

struct Node {
    /// The TypeId this node was created for.
    value: TypeId,

    /// The current status of the associated `TypeId`; null if the `TypeId` is
    /// currently considered absent, `occupied` if the `TypeId` is currently
    /// considered present, or a valid pointer if the `TypeId` is currently
    /// considered present and one or more threads are waiting to insert it.
    status: atomic::AtomicPtr<WaitingThreadNode>,

    /// The next node, or `null` if this is the end of the list.
    next: *const Node,
}

/// This otherwise invalid pointer is used as a marker value that a `TypeId` is
/// currently considered "in" the set.
fn occupied() -> *mut WaitingThreadNode {
    static RESERVED_MEMORY: usize = usize::from_ne_bytes([0xA5; core::mem::size_of::<usize>()]);
    core::ptr::addr_of!(RESERVED_MEMORY).cast_mut().cast()
}

/// This type is used for a stack-based linked list of waiting threads, so that
/// when a `TypeId` is removed from the set, a thread which is waiting to insert
/// that `TypeId` can be notified that it may proceed.
struct WaitingThreadNode {
    /// The handle of the waiting thread.
    #[cfg(feature = "std")]
    thread: thread::Thread,

    /// A flag to indicate if this node has been removed from the list of
    /// waiting threads, and the thread should stop waiting.
    #[cfg(feature = "std")]
    popped: atomic::AtomicBool,

    /// The next node, or `occupied`, if there are no more waiting threads.
    next: *mut WaitingThreadNode,
}

impl TypeIdSet {
    /// Search linearly through the list for a node with the given `TypeId`. If it
    /// was not found, return the value of the head pointer when we started
    /// searching (it is assured a node with that `TypeId` cannot be found by
    /// traversing from that pointer onward).
    fn find(&self, value: TypeId) -> Result<&Node, *const Node> {
        let original_head = self.head.load(Ordering::Acquire).cast_const();
        let mut current_node = original_head;
        // Safety: current_node is loaded from self.head or node.next, both of
        // which only ever store null or valid pointers created by
        // Box::into_raw, so it's safe to call .as_ref on it here
        while let Some(node) = unsafe { current_node.as_ref() } {
            if node.value == value {
                return Ok(node);
            }
            current_node = node.next;
        }
        Err(original_head)
    }

    /// Try to insert a `TypeId` into the set.  Returns `true` if the `TypeId` was
    /// inserted or `false` if the `TypeId` was already considered present.
    #[must_use]
    pub fn try_insert(&self, value: TypeId) -> bool {
        self.try_insert_inner(value).is_ok()
    }

    /// Try to insert a `TypeId` into the set.  Returns Ok if the `TypeId` was
    /// inserted, or the occupied node if the `TypeId` was already considered
    /// present.
    fn try_insert_inner(&self, value: TypeId) -> Result<(), &Node> {
        let next = match self.find(value) {
            Ok(node) => {
                // The failure Ordering can be Relaxed here, because we don't
                // try to read any data associated with the value, we just care
                // if the operation succeeded or not.
                return node
                    .status
                    .compare_exchange(
                        ptr::null_mut(),
                        occupied(),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .map(|_| ())
                    .map_err(|_| node);
            }
            Err(original_head) => original_head,
        };
        let new_node = Box::into_raw(Box::new(Node {
            value,
            status: atomic::AtomicPtr::new(occupied()),
            next,
        }));
        // Safety: we just created the pointer from the box, so it's safe to
        // dereference here
        let Node {
            ref value,
            ref mut next,
            ..
        } = unsafe { &mut *new_node };
        let mut found_and_set = Ok(());
        self.head
            .fetch_update(Ordering::Release, Ordering::Acquire, |most_recent_head| {
                let most_recent_head = most_recent_head.cast_const();
                let mut current_next = most_recent_head;
                loop {
                    if current_next == *next {
                        *next = most_recent_head;
                        return Some(new_node);
                    }
                    // Safety: current_next is loaded from self.head or
                    // node.next, both of which only ever store null or valid
                    // pointers created by Box::into_raw, and only the last in
                    // the chain can be null, in which case we would have caught
                    // it with the previous condition, so it's safe to
                    // dereference here.
                    let node = unsafe { &*current_next };
                    if &node.value == value {
                        // The failure Ordering can be Relaxed here, because we
                        // don't try to read any data associated with the value,
                        // we just care if the operation succeeded or not.
                        found_and_set = node
                            .status
                            .compare_exchange(
                                ptr::null_mut(),
                                occupied(),
                                Ordering::Acquire,
                                Ordering::Relaxed,
                            )
                            .map(|_| ())
                            .map_err(|_| node);
                        return None;
                    }
                    current_next = node.next;
                }
            })
            .map(|_| ())
            .or_else(|_| {
                // Safety: in the error case, we have not stored the box anywhere else
                // so we can free it here
                let _ = unsafe { Box::from_raw(new_node) };
                found_and_set
            })
    }

    /// Block the current thread until we can consider ourselves to have
    /// inserted the `TypeId` provided.
    #[cfg(feature = "std")]
    pub fn wait_to_insert(&self, value: TypeId) {
        let Err(node) = self.try_insert_inner(value) else { return };
        let mut waiting_node = WaitingThreadNode {
            thread: thread::current(),
            popped: atomic::AtomicBool::new(false),
            next: ptr::null_mut(),
        };
        let mut status_guess = occupied();
        let mut set_status_to: *mut WaitingThreadNode = &mut waiting_node;
        while let Err(status) = node.status.compare_exchange_weak(
            status_guess,
            set_status_to,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            status_guess = status;
            if status.is_null() {
                set_status_to = occupied();
            } else {
                waiting_node.next = status;
                set_status_to = &mut waiting_node;
            }
        }
        if set_status_to == occupied() {
            // The status was null, so we didn't end up needing to wait.
            return;
        }
        loop {
            if waiting_node.popped.load(Ordering::Acquire) {
                break;
            }
            thread::park();
        }
        drop(waiting_node);
    }

    /// Mark a `TypeId` as absent from the set, or notify a waiting thread that
    /// it may proceed.
    ///
    /// Returns true if the value was present in the set.
    ///
    /// # Safety
    /// Must not be called concurrently from multiple threads with the same
    /// `TypeId` value.
    pub unsafe fn remove(&self, value: TypeId) -> bool {
        let Ok(node) = self.find(value) else { return false };
        let mut status_guess = occupied();
        let mut set_status_to = ptr::null_mut();
        while let Err(status) = node.status.compare_exchange_weak(
            status_guess,
            set_status_to,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            if status.is_null() {
                return false;
            } else if status == occupied() {
                set_status_to = ptr::null_mut();
                status_guess = occupied();
            } else {
                // Safety: `status` is either null, `occupied()`, or valid, and
                // we just checked that it wasn't null or `occupied`, so it's
                // safe to dereference here.  The pointer is still alive unless
                // this function is called concurrently, which is why it's an
                // unsafe function with that condition.
                set_status_to = unsafe { (*status).next };
                status_guess = status;
            }
        }
        // If we were successful, it's because our guess was correct, so
        // `status_guess` holds the previous value of `node.status`.
        #[cfg(feature = "std")]
        if status_guess != occupied() {
            // Safety: `status` is either null, `occupied`, or valid. If it was
            // null, we would have returned false up above, and we just checked
            // that it wasn't `occupied()`.  The pointer is still alive unless
            // this function is called concurrently, which is why it's an unsafe
            // function with that condition.
            let WaitingThreadNode { thread, popped, .. } = unsafe { &*status_guess };
            // Clone the thread handle here because it could be invalid as soon
            // as we store into `popped`.
            let thread = thread.clone();
            popped.store(true, Ordering::Release);
            thread.unpark();
        }
        true
    }
}

impl Drop for TypeIdSet {
    fn drop(&mut self) {
        #[cfg(loom)]
        let mut node = self
            .head
            .with_mut(|p| core::mem::replace(p, ptr::null_mut()));
        #[cfg(not(loom))]
        let mut node = core::mem::replace(self.head.get_mut(), ptr::null_mut());
        while !node.is_null() {
            // Node pointers are either null or valid pointers created by
            // `Box::into_raw`, so it's safe to call `Box::from_raw` here.
            let boxed = unsafe { Box::from_raw(node) };
            node = boxed.next.cast_mut();
        }
    }
}
