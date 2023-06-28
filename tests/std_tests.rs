#![cfg(feature = "std")]

use exclusion_set::Set;

extern crate std;

use core::any::TypeId;

use std::sync::Arc;

#[cfg(loom)]
use loom::{model, thread};

#[cfg(not(loom))]
fn model<F: FnOnce()>(f: F) {
    f();
}

#[cfg(not(loom))]
use std::thread;

#[test]
fn two_threads_wait() {
    model(|| {
        struct Marker;

        let set: Arc<Set<TypeId>> = Arc::default();
        let set2 = Arc::clone(&set);
        let other = thread::spawn(move || {
            set2.wait_to_insert(TypeId::of::<Marker>());
            unsafe {
                assert!(set2.remove(&TypeId::of::<Marker>()));
            }
        });
        set.wait_to_insert(TypeId::of::<Marker>());
        unsafe {
            assert!(set.remove(&TypeId::of::<Marker>()));
        }
        other.join().unwrap();
    });
}

#[test]
#[ignore = "very slow"]
fn three_threads_wait() {
    model(|| {
        struct Marker;

        let set0: Arc<Set<TypeId>> = Arc::default();
        let set1 = Arc::clone(&set0);
        let set2 = Arc::clone(&set0);
        let thread1 = thread::spawn(move || {
            set1.wait_to_insert(TypeId::of::<Marker>());
            unsafe {
                assert!(set1.remove(&TypeId::of::<Marker>()));
            }
        });
        let thread2 = thread::spawn(move || {
            set2.wait_to_insert(TypeId::of::<Marker>());
            unsafe {
                assert!(set2.remove(&TypeId::of::<Marker>()));
            }
        });
        set0.wait_to_insert(TypeId::of::<Marker>());
        unsafe {
            assert!(set0.remove(&TypeId::of::<Marker>()));
        }
        thread1.join().unwrap();
        thread2.join().unwrap();
    });
}

#[test]
fn two_threads_wait_after_an_insert() {
    model(|| {
        struct Marker1;
        struct Marker2;

        let set: Arc<Set<TypeId>> = Arc::default();
        assert!(set.try_insert(TypeId::of::<Marker1>()));
        let set2 = Arc::clone(&set);

        let other = thread::spawn(move || {
            set2.wait_to_insert(TypeId::of::<Marker2>());
            unsafe {
                assert!(set2.remove(&TypeId::of::<Marker2>()));
            }
        });
        set.wait_to_insert(TypeId::of::<Marker2>());
        unsafe {
            assert!(set.remove(&TypeId::of::<Marker2>()));
        }
        other.join().unwrap();
    });
}

#[test]
#[ignore = "very slow"]
fn alternating_waits() {
    model(|| {
        struct Marker;

        let typeid_value = TypeId::of::<Marker>();
        let set: Arc<Set<TypeId>> = Arc::default();
        assert!(set.try_insert(TypeId::of::<Marker>()));
        let set2 = Arc::clone(&set);
        let other = thread::spawn(move || {
            set2.wait_to_insert(typeid_value);
            unsafe {
                assert!(set2.remove(&typeid_value));
            }
            set2.wait_to_insert(typeid_value);
            unsafe {
                assert!(set2.remove(&typeid_value));
            }
        });
        unsafe {
            assert!(set.remove(&typeid_value));
        }
        set.wait_to_insert(typeid_value);
        unsafe {
            assert!(set.remove(&typeid_value));
        }
        set.wait_to_insert(typeid_value);
        unsafe {
            assert!(set.remove(&typeid_value));
        }
        other.join().unwrap();
    });
}
