use std::sync::atomic::Ordering;

use loom::{
    sync::atomic::{AtomicBool, AtomicPtr},
    thread::{self, Thread},
};

static SINGLE_OWNER: usize = 0xDEADBEEF;

fn single_owner() -> *mut WaitingThreadNode {
    ((&SINGLE_OWNER) as *const usize).cast_mut().cast()
}

struct WaitingThreadNode {
    thread: Thread,
    popped: AtomicBool,
    next: *mut WaitingThreadNode,
}

struct Node<T> {
    value: T,
    waiting: AtomicPtr<WaitingThreadNode>,
    next: *const Node<T>,
}

#[derive(Default)]
pub struct Set<T> {
    head: AtomicPtr<Node<T>>,
}

impl<T: Eq> Set<T> {
    fn find(&self, value: &T) -> Result<&Node<T>, *const Node<T>> {
        let original_head = self.head.load(Ordering::Acquire).cast_const();
        let mut current_node = original_head;
        // Safety: current_node is loaded from self.head or node.next, both of
        // which only ever store null or valid pointers created by
        // Box::into_raw, so it's safe to call .as_ref on it here
        while let Some(node) = unsafe { current_node.as_ref() } {
            if &node.value == value {
                return Ok(node);
            }
            current_node = node.next;
        }
        Err(original_head)
    }

    pub fn try_insert(&self, value: T) -> bool {
        self.try_insert_inner(value).is_ok()
    }

    fn try_insert_inner(&self, value: T) -> Result<(), &Node<T>> {
        let next = match self.find(&value) {
            Ok(node) => {
                return node
                    .waiting
                    .compare_exchange(
                        std::ptr::null_mut(),
                        single_owner(),
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
            waiting: AtomicPtr::new(single_owner()),
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
        match self
            .head
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
                        found_and_set = node
                            .waiting
                            .compare_exchange(
                                std::ptr::null_mut(),
                                single_owner(),
                                Ordering::Acquire,
                                Ordering::Relaxed,
                            )
                            .map(|_| ())
                            .map_err(|_| node);
                        return None;
                    }
                    current_next = node.next;
                }
            }) {
            Ok(_) => Ok(()),
            Err(_) => {
                // Safety: in the error case, we have not stored the box anywhere else
                // so we can free it here
                let _ = unsafe { Box::from_raw(new_node) };
                found_and_set
            }
        }
    }

    pub fn wait_to_insert(&self, value: T) {
        let node = match self.try_insert_inner(value) {
            Ok(()) => return,
            Err(n) => n,
        };
        let mut waiting_node = WaitingThreadNode {
            thread: thread::current(),
            popped: AtomicBool::new(false),
            next: std::ptr::null_mut(),
        };
        let existing = node
            .waiting
            .fetch_update(Ordering::Release, Ordering::Acquire, |existing| {
                if existing.is_null() {
                    Some(single_owner())
                } else {
                    waiting_node.next = existing;
                    Some(&mut waiting_node)
                }
            })
            .unwrap();
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

    /// # Safety
    /// The value must be in the set.
    pub unsafe fn remove(&self, value: &T) {
        let node = self.find(value).unwrap();
        let first_waiting = node
            .waiting
            .fetch_update(Ordering::Release, Ordering::Acquire, |waiting| {
                debug_assert_ne!(waiting, std::ptr::null_mut());
                if waiting == single_owner() {
                    Some(std::ptr::null_mut())
                } else {
                    // Safety: `waiting` is either single_owner, null, or valid.
                    // It's only null if this function's safety precondition is
                    // violated, and we just checked that it wasn't single_owner,
                    // so it's safe to dereference here.
                    Some(unsafe { (*waiting).next })
                }
            })
            .unwrap();
        debug_assert_ne!(first_waiting, std::ptr::null_mut());
        if first_waiting != single_owner() {
            // Safety: `first_waiting` is either single_owner, null, or valid. It's
            // only null if this function's safety precondition is violated, and
            // we just checked that it wasn't single_owner, so it's safe to
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
            let set = Set::default();

            assert!(set.try_insert(7));
            assert!(!set.try_insert(7));
        });
    }

    #[test]
    fn test2() {
        model(|| {
            let set = Set::default();

            assert!(set.try_insert(7));
            unsafe {
                set.remove(&7);
            }
            assert!(set.try_insert(7));
        });
    }

    #[test]
    fn test3() {
        model(|| {
            let set: &Set<i32> = Box::leak(Box::default());

            let other = thread::spawn(|| if set.try_insert(7) { 1 } else { 0 });
            let count = if set.try_insert(7) { 1 } else { 0 };

            let count = count + other.join().unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn test4() {
        model(|| {
            let set: &Set<i32> = Box::leak(Box::default());

            let other = thread::spawn(|| {
                set.wait_to_insert(7);
                unsafe {
                    set.remove(&7);
                }
            });
            set.wait_to_insert(7);
            unsafe {
                set.remove(&7);
            }

            other.join().unwrap();
        });
    }

    #[test]
    fn test5() {
        model(|| {
            let set: &Set<i32> = Box::leak(Box::default());

            assert!(set.try_insert(99));

            let other = thread::spawn(|| {
                set.wait_to_insert(7);
                unsafe {
                    set.remove(&7);
                }
            });
            set.wait_to_insert(7);
            unsafe {
                set.remove(&7);
            }

            other.join().unwrap();
        });
    }

    #[test]
    fn test6() {
        model(|| {
            let set: &Set<i32> = Box::leak(Box::default());

            assert!(set.try_insert(99));

            let other = thread::spawn(|| {
                assert!(set.try_insert(1));
                assert!(set.try_insert(3));
                assert!(set.try_insert(5));
            });
            assert!(set.try_insert(2));
            assert!(set.try_insert(4));
            assert!(set.try_insert(6));

            other.join().unwrap();
        });
    }
}
