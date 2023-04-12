//! A lock-free concurrent set that stores `TypeId`s.  The operations on this
//! set are O(n) where n is the number of distinct `TypeId`s that have ever been
//! inserted into the set.

#![no_std]
#![deny(clippy::all, clippy::pedantic)]

extern crate alloc;

use {
    alloc::boxed::Box,
    core::{any::TypeId, ptr, sync::atomic::Ordering},
};

use loom::{
    sync::atomic::{AtomicBool, AtomicPtr},
    thread::{self, Thread},
};

/// A set of `TypeId`s held in a linked list.
#[derive(Default)]
pub struct TypeIdSet {
    head: AtomicPtr<Node>,
}

struct Node {
    /// The TypeId this node was created for.
    value: TypeId,

    /// The current status of the associated `TypeId`; null if the `TypeId` is
    /// currently considered absent, `occupied` if the `TypeId` is currently
    /// considered present, or a valid pointer if the `TypeId` is currently
    /// considered present and one or more threads are waiting to insert it.
    status: AtomicPtr<WaitingThreadNode>,

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
    thread: Thread,

    /// A flag to indicate if this node has been removed from the list of
    /// waiting threads, and the thread should stop waiting.
    popped: AtomicBool,

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
            status: AtomicPtr::new(occupied()),
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
    pub fn wait_to_insert(&self, value: TypeId) {
        let Err(node) = self.try_insert_inner(value) else { return };
        let mut waiting_node = WaitingThreadNode {
            thread: thread::current(),
            popped: AtomicBool::new(false),
            next: ptr::null_mut(),
        };
        let Ok(existing) = node
            .status
            .fetch_update(Ordering::Release, Ordering::Acquire, |existing| {
                if existing.is_null() {
                    Some(occupied())
                } else {
                    waiting_node.next = existing;
                    Some(&mut waiting_node)
                }
            })
            else { unreachable!() };
        if existing.is_null() {
            return;
        }
        loop {
            if waiting_node.popped.load(Ordering::Acquire) {
                return;
            }
            thread::park();
        }
    }

    /// Mark a `TypeId` as absent from the set, or notify a waiting thread that
    /// it may proceed.
    ///
    /// # Safety
    /// The value must be in the set.
    pub unsafe fn remove(&self, value: TypeId) {
        let Ok(node) = self.find(value) else { unreachable!() };
        let Ok(first_waiting) = node
            .status
            .fetch_update(Ordering::Release, Ordering::Acquire, |waiting| {
                debug_assert_ne!(waiting, ptr::null_mut());
                if waiting == occupied() {
                    Some(ptr::null_mut())
                } else {
                    // Safety: `waiting` is either `occupied`, null, or valid.
                    // It's only null if this function's safety precondition is
                    // violated, and we just checked that it wasn't `occupied`,
                    // so it's safe to dereference here.
                    Some(unsafe { (*waiting).next })
                }
            })
            else { unreachable!() };
        debug_assert_ne!(first_waiting, ptr::null_mut());
        if first_waiting != occupied() {
            // Safety: `first_waiting` is either `occupied`, null, or valid. It's
            // only null if this function's safety precondition is violated, and
            // we just checked that it wasn't `occupied`, so it's safe to
            // dereference here.
            let WaitingThreadNode { thread, popped, .. } = unsafe { &*first_waiting };
            let thread = thread.clone();
            popped.store(true, Ordering::Release);
            thread.unpark();
        }
    }
}

#[cfg(test)]
mod tests {
    use core::any::TypeId;

    use super::*;

    use loom::model;

    /*
    fn model<F: FnOnce()>(f: F) {
        f()
    }
    */

    #[test]
    fn test1() {
        model(|| {
            struct Marker;

            let set = TypeIdSet::default();
            assert!(set.try_insert(TypeId::of::<Marker>()));
            assert!(!set.try_insert(TypeId::of::<Marker>()));
        });
    }

    #[test]
    fn test2() {
        model(|| {
            struct Marker;

            let set = TypeIdSet::default();
            assert!(set.try_insert(TypeId::of::<Marker>()));
            unsafe {
                set.remove(TypeId::of::<Marker>());
            }
            assert!(set.try_insert(TypeId::of::<Marker>()));
        });
    }

    #[test]
    fn test3() {
        model(|| {
            struct Marker;

            let set: &TypeIdSet = Box::leak(Box::default());
            let other = thread::spawn(|| i32::from(set.try_insert(TypeId::of::<Marker>())));
            let count = i32::from(set.try_insert(TypeId::of::<Marker>()));

            let count = count + other.join().unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn test4() {
        model(|| {
            struct Marker;

            let set: &TypeIdSet = Box::leak(Box::default());
            let other = thread::spawn(|| {
                set.wait_to_insert(TypeId::of::<Marker>());
                unsafe {
                    set.remove(TypeId::of::<Marker>());
                }
            });
            set.wait_to_insert(TypeId::of::<Marker>());
            unsafe {
                set.remove(TypeId::of::<Marker>());
            }
            other.join().unwrap();
        });
    }

    #[test]
    fn test5() {
        model(|| {
            struct Marker1;
            struct Marker2;

            let set: &TypeIdSet = Box::leak(Box::default());

            assert!(set.try_insert(TypeId::of::<Marker1>()));

            let other = thread::spawn(|| {
                set.wait_to_insert(TypeId::of::<Marker2>());
                unsafe {
                    set.remove(TypeId::of::<Marker2>());
                }
            });
            set.wait_to_insert(TypeId::of::<Marker2>());
            unsafe {
                set.remove(TypeId::of::<Marker2>());
            }
            other.join().unwrap();
        });
    }

    #[test]
    fn test6() {
        model(|| {
            struct Marker;
            struct Marker1;
            struct Marker2;
            struct Marker3;
            struct MarkerA;
            struct MarkerB;
            struct MarkerC;

            let set: &TypeIdSet = Box::leak(Box::default());
            assert!(set.try_insert(TypeId::of::<Marker>()));
            let other = thread::spawn(|| {
                assert!(set.try_insert(TypeId::of::<Marker1>()));
                assert!(set.try_insert(TypeId::of::<Marker2>()));
                assert!(set.try_insert(TypeId::of::<Marker3>()));
            });
            assert!(set.try_insert(TypeId::of::<MarkerA>()));
            assert!(set.try_insert(TypeId::of::<MarkerB>()));
            assert!(set.try_insert(TypeId::of::<MarkerC>()));
            other.join().unwrap();
        });
    }
}
