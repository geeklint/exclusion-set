use typeid_set::TypeIdSet;

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
fn can_only_insert_once() {
    model(|| {
        struct Marker;

        let set = TypeIdSet::default();
        assert!(set.try_insert(TypeId::of::<Marker>()));
        assert!(!set.try_insert(TypeId::of::<Marker>()));
    });
}

#[test]
fn can_insert_after_remove() {
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
fn only_one_thread_can_insert() {
    model(|| {
        struct Marker;

        let set: Arc<TypeIdSet> = Arc::default();
        let set2 = Arc::clone(&set);
        let other = thread::spawn(move || i32::from(set2.try_insert(TypeId::of::<Marker>())));
        let count = i32::from(set.try_insert(TypeId::of::<Marker>()));

        let count = count + other.join().unwrap();
        assert_eq!(count, 1);
    });
}

#[test]
fn many_inserts() {
    model(|| {
        struct Marker;
        struct Marker1;
        struct Marker2;
        struct Marker3;
        struct MarkerA;
        struct MarkerB;
        struct MarkerC;

        let set: Arc<TypeIdSet> = Arc::default();
        assert!(set.try_insert(TypeId::of::<Marker>()));
        let set2 = Arc::clone(&set);
        let other = thread::spawn(move || {
            assert!(set2.try_insert(TypeId::of::<Marker1>()));
            assert!(set2.try_insert(TypeId::of::<Marker2>()));
            assert!(set2.try_insert(TypeId::of::<Marker3>()));
        });
        assert!(set.try_insert(TypeId::of::<MarkerA>()));
        assert!(set.try_insert(TypeId::of::<MarkerB>()));
        assert!(set.try_insert(TypeId::of::<MarkerC>()));
        other.join().unwrap();
    });
}

#[test]
fn removing_something_absent_is_false() {
    model(|| {
        struct Present;
        struct Absent;

        let set = TypeIdSet::default();
        assert!(set.try_insert(TypeId::of::<Present>()));
        unsafe {
            assert!(!set.remove(TypeId::of::<Absent>()));
            assert!(set.remove(TypeId::of::<Present>()));
        }
    });
}

#[test]
fn removing_while_touching() {
    model(|| {
        struct Marker;

        let set: Arc<TypeIdSet> = Arc::default();
        let set2 = Arc::clone(&set);
        assert!(set.try_insert(TypeId::of::<Marker>()));
        let bg = thread::spawn(move || {
            while set2.touch(TypeId::of::<Marker>()) {
                thread::yield_now();
            }
        });
        unsafe { assert!(set.remove(TypeId::of::<Marker>())) };
        bg.join().unwrap();
    });
}
