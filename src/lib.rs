//! A lock-free concurrent set.  The operations on this set are O(n) where n is
//! the number of distinct values that have ever been inserted into the set.
//!
//! The intended use-case is to provide a way to ensure exclusivity over
//! critical sections of code on a per-value basis, where the number of distinct
//! values is small but dynamic.
//!
//! This data structure uses atomic singly-linked lists in two forms to enable
//! its operations.  One list has a node for every distinct value that has
//! ever been inserted.  The other type of list exists within each of those
//! nodes, and manages a queue of threads waiting in the `wait_to_insert` method
//! for another thread to call `remove`.
//!
//! An atomic singly-linked list is relatively straightforward to insert to:
//! Allocate a new node, and then in loop, update the 'next' pointer of the node
//! to the most recent value of the 'head' pointer, and then attempt a
//! compare-exchange, replacing the old 'head' with the pointer to the new node.
//!
//! Things get more complicated as soon as you additionally consider removing
//! items from the list.  Anything that dereferences a node pointer now runs the
//! risk of attempting to dereference a value which has been removed between the
//! load that returned the pointer and the dereference of the pointer.  Note
//! that removal itself requires a dereference of the head pointer, to determine
//! the value of `head.next`.  This data structure avoids this issue in slightly
//! different ways for the two different types of list.
//!
//! The main list of nodes for each value avoids the issue by never removing
//! nodes except in `Drop`.  The exclusive access guarentee of Drop ensures that
//! no other thread could attempt to access the list while it is being freed.
//!
//! The list of waiting threads instead avoids the issue by specifying, for each
//! list of waiting threads, which in the context of this set, means for each
//! unique value, that at most one thread at a time may dereference a pointer.
//! It exposes this contract as the safety requirement of the unsafe `remove`
//! method.  This requirement is easy to fulfil for applications where a value
//! is only removed from the set by a logical "owner" which knows that it
//! previously inserted a value.
//!
//! # Example
//!
//! The following code inserts some values into the set, then removes one of
//! them, and then spawns a second thread that waits to insert into the set.
//!
#![cfg_attr(
    feature = "std",
    doc = "
```
# use std::sync::Arc;
# use typeid_set::Set;
# unsafe {
let set: Arc<Set> = Arc::default();
set.try_insert(1);
set.try_insert(2);
set.try_insert(3);
set.remove(1);
let set2 = set.clone();
# let handle =
std::thread::spawn(move || {
    set2.wait_to_insert(2);
});
# set.remove(2); // avoid a deadlock in the example
# handle.join();
# }
```
"
)]
//!
//! After this code has been run, we can expect the data structure to look like
//! this:
//!
//! <div style="background-color: white">
#![doc=include_str!("lib-example.svg")]
//! </div>

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(clippy::all, clippy::pedantic)]

extern crate alloc;

use {
    alloc::boxed::Box,
    core::{ptr, sync::atomic::Ordering},
};

#[cfg(loom)]
use loom::{sync::atomic, thread};

#[cfg(not(loom))]
use core::sync::atomic;

#[cfg(all(not(loom), feature = "std"))]
use std::thread;

/// A set of values held in a linked list.
pub struct Set<T> {
    /// This pointer is either null (if the set has never been inserted to) or a
    /// pointer to the first Node in the set.
    head: atomic::AtomicPtr<Node<T>>,
    /// This pointer is either null (if the set has never been inserted to) or a
    /// pointer to one of the nodes in the set.  This enables an optimization
    /// where the last removed item can be cheaper to re-insert by skipping
    /// navigating the whole linked-list.
    last_removed: atomic::AtomicPtr<Node<T>>,
}

struct Node<T> {
    /// The value this node was created for.
    value: T,

    /// The current status of the associated value; null if the value is
    /// currently considered absent, `occupied` if the value is currently
    /// considered present, or a valid pointer if the value is currently
    /// considered present and one or more threads are waiting to insert it.
    status: atomic::AtomicPtr<WaitingThreadNode>,

    /// The next node, or `null` if this is the end of the list.
    next: *const Node<T>,
}

/// This otherwise invalid pointer is used as a marker value that a value is
/// currently considered "in" the set.
fn occupied() -> *mut WaitingThreadNode {
    static RESERVED_MEMORY: usize = usize::from_ne_bytes([0xA5; core::mem::size_of::<usize>()]);
    core::ptr::addr_of!(RESERVED_MEMORY).cast_mut().cast()
}

/// This type is used for a stack-based linked list of waiting threads, so that
/// when a value is removed from the set, a thread which is waiting to insert
/// that value can be notified that it may proceed.
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

impl<T> Default for Set<T> {
    fn default() -> Self {
        Self {
            head: atomic::AtomicPtr::new(ptr::null_mut()),
            last_removed: atomic::AtomicPtr::new(ptr::null_mut()),
        }
    }
}

impl<T> Set<T> {
    /// Create a new, empty, `Set`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: atomic::AtomicPtr::new(ptr::null_mut()),
            last_removed: atomic::AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Search linearly through the list for a node with the given value. If it
    /// was not found, return the value of the head pointer when we started
    /// searching (it is assured a node with that value cannot be found by
    /// traversing from that pointer onward).
    fn find(&self, value: &T) -> Result<&Node<T>, *const Node<T>>
    where
        T: Eq,
    {
        let starting_node = self.last_removed.load(Ordering::Acquire).cast_const();
        let mut current_node = starting_node;
        // Safety: current_node is loaded from self.last_dropped or node.next, both of
        // which only ever store null or valid pointers created by
        // Box::into_raw, so it's safe to call .as_ref on it here
        while let Some(node) = unsafe { current_node.as_ref() } {
            if node.value == *value {
                return Ok(node);
            }
            current_node = node.next;
        }
        let original_head = self.head.load(Ordering::Acquire).cast_const();
        let mut current_node = original_head;
        while current_node != starting_node {
            // Safety: current_node is loaded from self.head or node.next, both of
            // which only ever store null or valid pointers created by
            // Box::into_raw, and it won't be null since it must not be the end of
            // the chain if it wasn't equal to starting_node
            let node = unsafe { &*current_node };
            if node.value == *value {
                return Ok(node);
            }
            current_node = node.next;
        }
        Err(original_head)
    }

    /// Try to insert a value into the set.  Returns `true` if the value was
    /// inserted or `false` if the value was already considered present.
    #[must_use]
    pub fn try_insert(&self, value: T) -> bool
    where
        T: Eq,
    {
        self.try_insert_inner(value).is_ok()
    }

    /// Try to insert a value into the set.  Returns Ok if the value was
    /// inserted, or the occupied node if the value was already considered
    /// present.
    fn try_insert_inner(&self, value: T) -> Result<(), &Node<T>>
    where
        T: Eq,
    {
        let next = match self.find(&value) {
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
                let _: Box<Node<T>> = unsafe { Box::from_raw(new_node) };
                found_and_set
            })
    }

    /// If the value provided is not in the set, insert it.  Otherwise, block
    /// the current thread until another thread calls `remove` for the given
    /// value (if multiple threads are waiting, only one of them will
    /// return).
    #[cfg(feature = "std")]
    pub fn wait_to_insert(&self, value: T)
    where
        T: Eq,
    {
        let Err(node) = self.try_insert_inner(value) else { return };
        let mut waiting_node = WaitingThreadNode {
            thread: thread::current(),
            popped: atomic::AtomicBool::new(false),
            next: occupied(),
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

    /// Mark a value as absent from the set, or notify a waiting thread that
    /// it may proceed.
    ///
    /// Returns true if the value was present in the set.
    ///
    /// # Safety
    /// Must not be called concurrently from multiple threads with the same
    /// value.
    pub unsafe fn remove(&self, value: &T) -> bool
    where
        T: Eq,
    {
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
                status_guess = status;
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
        self.last_removed
            .store(<*const _>::cast_mut(node), Ordering::Release);
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

impl<T> Drop for Set<T> {
    fn drop(&mut self) {
        #[cfg(loom)]
        let mut node = self
            .head
            .with_mut(|p| core::mem::replace(p, ptr::null_mut()));
        #[cfg(not(loom))]
        let mut node = core::mem::replace(self.head.get_mut(), ptr::null_mut());
        while !node.is_null() {
            // Node pointers are either null or valid pointers created by
            // `Box::into_raw`, and we just checked that it was not null, so
            // it's safe to call `Box::from_raw` here.
            let boxed = unsafe { Box::from_raw(node) };
            node = boxed.next.cast_mut();
        }
    }
}
