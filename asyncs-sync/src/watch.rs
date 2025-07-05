//! Channel to publish and subscribe values.

use std::cell::{Cell, UnsafeCell};
use std::cmp::Ordering::*;
use std::fmt::{self, Formatter};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::Ordering::{self, *};
use std::sync::atomic::{AtomicPtr, AtomicUsize};
use std::sync::Arc;

use crate::Notify;

/// Error for [Sender::send] to express no receivers alive.
#[derive(Debug)]
pub struct SendError<T>(T);

impl<T> SendError<T> {
    pub fn into_value(self) -> T {
        self.0
    }
}

/// Error for [Receiver::changed()] to express that sender has been dropped.
#[derive(Debug)]
pub struct RecvError(());

#[repr(transparent)]
#[derive(Clone, Copy, Default, Debug, PartialEq, PartialOrd, Eq, Ord)]
struct Version(u64);

impl Version {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

struct Slot<T> {
    refs: AtomicUsize,

    frees: AtomicPtr<Slot<T>>,

    /// Safety: never changed after published and before reclaimed
    value: UnsafeCell<ManuallyDrop<T>>,
    version: Cell<Version>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            refs: AtomicUsize::new(0),
            frees: AtomicPtr::new(ptr::null_mut()),
            value: UnsafeCell::new(ManuallyDrop::new(unsafe { MaybeUninit::zeroed().assume_init() })),
            version: Cell::new(Version::default()),
        }
    }
}

impl<T> Slot<T> {
    fn store(&self, value: T) {
        debug_assert_eq!(self.refs.load(Relaxed), 0);
        unsafe {
            std::ptr::write(self.value.get(), ManuallyDrop::new(value));
        }
        self.refs.store(1, Relaxed);
    }

    /// Retains current or newer version.
    unsafe fn retain(&self, version: Version) -> Option<&Slot<T>> {
        let mut refs = self.refs.load(Relaxed);
        loop {
            if refs == 0 {
                return None;
            }
            match self.refs.compare_exchange(refs, refs + 1, Relaxed, Relaxed) {
                Ok(_) => {},
                Err(updated) => {
                    refs = updated;
                    continue;
                },
            }
            match self.version.get().cmp(&version) {
                Equal | Greater => return Some(self),
                Less => panic!(
                    "BUG: version is monotonic, expect version {:?}, got old version {:?}",
                    version,
                    self.version.get()
                ),
            }
        }
    }
}

#[repr(transparent)]
struct UnsafeSlot<T>(Slot<T>);

impl<T> UnsafeSlot<T> {
    pub fn retain(&self, version: Version) -> Option<&Slot<T>> {
        unsafe { self.0.retain(version) }
    }

    pub unsafe fn slot(&self) -> &Slot<T> {
        &self.0
    }
}

struct Row<T> {
    prev: Option<Box<Row<T>>>,
    slots: [Slot<T>; 16],
}

impl<T> Default for Row<T> {
    fn default() -> Self {
        Self { prev: None, slots: Default::default() }
    }
}

// We could also stamp version into the atomic to filter eagerly, but it will require `AtomicU128`.
struct Latest(AtomicUsize);

impl Latest {
    const CLOSED: usize = 0x01;
    const MASK: usize = !Self::CLOSED;

    pub fn new<T>(slot: &Slot<T>) -> Self {
        let raw = slot as *const _ as usize;
        Self(AtomicUsize::new(raw))
    }

    pub fn load<'a, T>(&self, ordering: Ordering) -> (&'a UnsafeSlot<T>, bool) {
        let raw = self.0.load(ordering);
        (Self::slot(raw & Self::MASK), raw & Self::CLOSED == Self::CLOSED)
    }

    fn slot<'a, T>(raw: usize) -> &'a UnsafeSlot<T> {
        unsafe { &*(raw as *const UnsafeSlot<T>) }
    }

    fn ptr<T>(slot: &Slot<T>) -> usize {
        slot as *const Slot<T> as usize
    }

    pub fn compare_exchange<'a, T>(
        &self,
        current: &'a Slot<T>,
        new: &Slot<T>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<&'a Slot<T>, &'a UnsafeSlot<T>> {
        match self.0.compare_exchange(Self::ptr(current), Self::ptr(new), success, failure) {
            Ok(_) => Ok(current),
            Err(updated) => Err(Self::slot(updated)),
        }
    }

    pub fn close(&self) {
        let u = self.0.load(Relaxed);
        self.0.store(u | Self::CLOSED, Relaxed);
    }
}

struct Shared<T> {
    rows: UnsafeCell<Box<Row<T>>>,
    frees: AtomicPtr<Slot<T>>,

    latest: Latest,

    closed: Notify,
    changes: Notify,

    senders: AtomicUsize,
    receivers: AtomicUsize,
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let slot = self.latest.load(Relaxed).0;
        self.release(unsafe { slot.slot() });
    }
}

impl<T> Shared<T> {
    fn new(version: Version, value: T) -> Self {
        let row = Box::<Row<_>>::default();
        let slot = &row.slots[0];
        slot.store(value);
        slot.version.set(version);
        let latest = Latest::new(slot);
        let shared = Self {
            rows: UnsafeCell::new(row),
            frees: AtomicPtr::new(ptr::null_mut()),
            latest,
            closed: Notify::new(),
            changes: Notify::new(),
            senders: AtomicUsize::new(1),
            receivers: AtomicUsize::new(1),
        };
        let row = unsafe { &*shared.rows.get() };
        shared.add_slots(&row.slots[1..]);
        shared
    }

    fn new_sender(self: &Arc<Self>) -> Sender<T> {
        self.senders.fetch_add(1, Relaxed);
        Sender { shared: self.clone() }
    }

    fn drop_sender(&self) {
        if self.senders.fetch_sub(1, Relaxed) != 1 {
            return;
        }
        self.latest.close();
        self.changes.notify_all();
    }

    fn new_receiver(self: &Arc<Self>, seen: Version) -> Receiver<T> {
        self.receivers.fetch_add(1, Relaxed);
        Receiver { seen, shared: self.clone() }
    }

    fn drop_receiver(&self) {
        if self.receivers.fetch_sub(1, Relaxed) == 1 {
            self.closed.notify_all();
        }
    }

    fn add_slots(&self, slots: &[Slot<T>]) {
        for i in 0..slots.len() - 1 {
            let curr = unsafe { slots.get_unchecked(i) };
            let next = unsafe { slots.get_unchecked(i + 1) };
            curr.frees.store(next as *const _ as *mut _, Relaxed);
        }
        let head = unsafe { slots.get_unchecked(0) };
        let tail = unsafe { slots.get_unchecked(slots.len() - 1) };
        self.free_slots(head, tail);
    }

    fn alloc_slot(&self) -> &Slot<T> {
        // Acquire load to see `slot.frees`.
        let mut head = self.frees.load(Acquire);
        loop {
            if head.is_null() {
                break;
            }
            let slot = unsafe { &*head };
            let next = slot.frees.load(Relaxed);
            match self.frees.compare_exchange(head, next, Relaxed, Acquire) {
                Ok(_) => {
                    slot.frees.store(ptr::null_mut(), Relaxed);
                    return slot;
                },
                Err(updated) => head = updated,
            }
        }
        let mut row = ManuallyDrop::new(Box::<Row<_>>::default());
        row.prev = Some(unsafe { ptr::read(self.rows.get() as *const _) });
        unsafe {
            ptr::write(self.rows.get(), ManuallyDrop::take(&mut row));
        }
        self.add_slots(&row.slots[1..]);
        unsafe { std::mem::transmute(row.slots.get_unchecked(0)) }
    }

    fn free_slots(&self, head: &Slot<T>, tail: &Slot<T>) {
        let mut frees = self.frees.load(Relaxed);
        loop {
            tail.frees.store(frees, Relaxed);
            // Release store to publish `slot.frees`.
            match self.frees.compare_exchange(frees, head as *const _ as *mut _, Release, Relaxed) {
                Ok(_) => break,
                Err(updated) => frees = updated,
            }
        }
    }

    fn free_slot(&self, slot: &Slot<T>) {
        self.free_slots(slot, slot);
    }

    fn release(&self, slot: &Slot<T>) {
        if slot.refs.fetch_sub(1, Relaxed) != 1 {
            return;
        }
        let value = unsafe { &mut *slot.value.get() };
        let value = unsafe { ManuallyDrop::take(value) };
        drop(value);
        self.free_slot(slot);
    }

    fn publish(&self, value: T) {
        let slot = self.alloc_slot();
        slot.store(value);
        let mut latest = self.latest(Version(0));
        loop {
            let version = latest.version().next();
            slot.version.set(version);
            // Release store to publish value and version
            // Acquire load to observe version
            match self.latest.compare_exchange(latest.slot, slot, Release, Acquire) {
                Ok(slot) => {
                    self.release(slot);
                    self.changes.notify_all();
                    break;
                },
                Err(updated) => match updated.retain(version) {
                    None => latest = self.latest(version),
                    Some(slot) => latest = Ref { slot, shared: self, closed: false, changed: true },
                },
            }
        }
    }

    fn latest(&self, seen: Version) -> Ref<'_, T> {
        loop {
            // Acquire load to observe version and value.
            let (slot, closed) = self.latest.load(Acquire);
            if let Some(slot) = slot.retain(seen) {
                return Ref { slot, shared: self, closed, changed: seen != slot.version.get() };
            }
        }
    }

    fn try_changed(&self, seen: Version) -> Result<Option<Ref<'_, T>>, RecvError> {
        let latest = self.latest(seen);
        if latest.has_changed() {
            Ok(Some(latest))
        } else if latest.closed {
            Err(RecvError(()))
        } else {
            Ok(None)
        }
    }
}

/// Constructs a lock free channel to publish and subscribe values.
///
/// ## Differences with [tokio]
/// * [tokio] holds only a single value, so no allocation.
/// * [Ref] holds no lock but reference to underlying value, which prevent it from reclamation, so
///   it is also crucial to drop it as soon as possible.
///
/// [tokio]: https://docs.rs/tokio
pub fn channel<T>(value: T) -> (Sender<T>, Receiver<T>) {
    let version = Version(1);
    let shared = Arc::new(Shared::new(version, value));
    let sender = Sender { shared: shared.clone() };
    let receiver = Receiver { seen: version, shared };
    (sender, receiver)
}

/// Send part of [Receiver].
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

unsafe impl<T> Send for Sender<T> {}
unsafe impl<T> Sync for Sender<T> {}

impl<T: fmt::Debug> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let latest = self.shared.latest(Version(0));
        f.debug_struct("Sender")
            .field("version", &latest.version())
            .field("value", latest.as_ref())
            .field("closed", &latest.closed)
            .finish()
    }
}

impl<T> Sender<T> {
    /// Sends value to [Receiver]s.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.shared.receivers.load(Relaxed) == 0 {
            return Err(SendError(value));
        }
        self.publish(value);
        Ok(())
    }

    /// Publishes value for existing [Receiver]s and possible future [Receiver]s from
    /// [Sender::subscribe].
    pub fn publish(&self, value: T) {
        self.shared.publish(value);
    }

    /// Subscribes to future changes.
    pub fn subscribe(&self) -> Receiver<T> {
        let latest = self.shared.latest(Version::default());
        self.shared.receivers.fetch_add(1, Relaxed);
        Receiver { seen: latest.version(), shared: self.shared.clone() }
    }

    /// Receiver count.
    pub fn receiver_count(&self) -> usize {
        self.shared.receivers.load(Relaxed)
    }

    /// Blocks until all receivers dropped.
    pub async fn closed(&self) {
        // Loop as `subscribe` takes `&self` but not `&mut self`.
        while !self.is_closed() {
            let notified = self.shared.closed.notified();
            if self.is_closed() {
                return;
            }
            notified.await
        }
    }

    pub fn is_closed(&self) -> bool {
        self.receiver_count() == 0
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.new_sender()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.drop_sender();
    }
}

/// Receive part of [Sender].
pub struct Receiver<T> {
    seen: Version,
    shared: Arc<Shared<T>>,
}

unsafe impl<T> Send for Receiver<T> {}
unsafe impl<T> Sync for Receiver<T> {}

impl<T: fmt::Debug> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let latest = self.borrow();
        f.debug_struct("Receiver")
            .field("seen", &self.seen)
            .field("version", &latest.version())
            .field("value", latest.as_ref())
            .field("closed", &latest.closed)
            .field("changed", &latest.changed)
            .finish()
    }
}

/// Reference to borrowed value.
///
/// Holds reference will prevent it from reclamation so drop it as soon as possible.
pub struct Ref<'a, T> {
    slot: &'a Slot<T>,
    shared: &'a Shared<T>,
    closed: bool,
    changed: bool,
}

unsafe impl<T> Send for Ref<'_, T> {}
unsafe impl<T> Sync for Ref<'_, T> {}

impl<T: fmt::Debug> fmt::Debug for Ref<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ref")
            .field("version", &self.version())
            .field("value", self.as_ref())
            .field("closed", &self.closed)
            .field("changed", &self.changed)
            .finish()
    }
}

impl<'a, T> Ref<'a, T> {
    fn version(&self) -> Version {
        self.slot.version.get()
    }

    /// Do we ever seen this before from last [Receiver::borrow_and_update] and [Receiver::changed()] ?
    pub fn has_changed(&self) -> bool {
        self.changed
    }
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.slot.value.get() }
    }
}

impl<T> AsRef<T> for Ref<'_, T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> Drop for Ref<'_, T> {
    fn drop(&mut self) {
        self.shared.release(self.slot);
    }
}

impl<T> Receiver<T> {
    /// Borrows the latest value but does not mark it as seen.
    pub fn borrow(&self) -> Ref<'_, T> {
        self.shared.latest(self.seen)
    }

    /// Borrows the latest value and marks it as seen.
    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let latest = self.shared.latest(self.seen);
        self.seen = latest.version();
        latest
    }

    /// Blocks and consumes new change since last [Receiver::borrow_and_update] or [Receiver::changed()].
    ///
    /// If multiple values are published in the meantime, it is likely that only later one got
    /// observed. It is guaranteed that the final value after all [Sender]s dropped is always
    /// observed.
    ///
    /// ## Errors
    /// * [RecvError] after all [Sender]s dropped and final value consumed
    pub async fn changed(&mut self) -> Result<Ref<'_, T>, RecvError> {
        loop {
            // This serves both luck path and recheck after `notified.await`.
            if let Some(changed) = self.shared.try_changed(self.seen)? {
                self.seen = changed.version();
                return Ok(changed);
            }
            let notified = self.shared.changes.notified();
            if let Some(changed) = self.shared.try_changed(self.seen)? {
                self.seen = changed.version();
                return Ok(changed);
            }
            notified.await;
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.new_receiver(self.seen)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.drop_receiver();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use asyncs::{select, task};

    use crate::{watch, Notify};

    #[asyncs::test]
    async fn channel_sequential() {
        // given: a watch channel
        let (sender, receiver) = watch::channel(5);

        // when: borrow without a send
        let latest = receiver.borrow();
        // then: have seen initial value
        assert_eq!(*latest, 5);
        assert!(!latest.has_changed());
        drop(latest);

        // when: send
        sender.send(6).unwrap();
        // then: receiver will observe that send
        let latest = receiver.borrow();
        assert_eq!(*latest, 6);
        assert!(latest.has_changed());
        drop(latest);

        // when: send after all receivers dropped.
        drop(receiver);
        assert_eq!(sender.send(7).unwrap_err().into_value(), 7);

        // then: send fails with no side effect.
        let receiver = sender.subscribe();
        let latest = receiver.borrow();
        assert_eq!(*latest, 6);
        assert!(!latest.has_changed());
        drop(latest);
        drop(receiver);

        // when: publish after all receivers dropped.
        sender.publish(7);
        // then: new receiver will observe that
        let receiver = sender.subscribe();
        let latest = receiver.borrow();
        assert_eq!(*latest, 7);
        assert!(!latest.has_changed());
    }

    #[asyncs::test]
    async fn receivers_dropped() {
        let (sender, receiver) = watch::channel(5);
        task::spawn(async move {
            drop(receiver);
        });
        select! {
            _ = sender.closed() => {},
        }

        let _receiver = sender.subscribe();
        select! {
            default => {},
            _ = sender.closed() => unreachable!(),
        }
    }

    #[asyncs::test]
    async fn senders_dropped() {
        let (sender, mut receiver) = watch::channel(());
        drop(sender.clone());
        select! {
            default => {},
            _ = receiver.changed() => unreachable!(),
        }

        drop(sender);
        select! {
            default => unreachable!(),
            Err(_) = receiver.changed() => {},
        }
    }

    #[asyncs::test]
    async fn changed() {
        let notify = Arc::new(Notify::new());
        let (sender, mut receiver) = watch::channel(0);
        let handle = task::spawn({
            let notify = notify.clone();
            async move {
                let mut values = vec![];
                while let Ok(latest) = receiver.changed().await {
                    values.push(*latest);
                    notify.notify_one();
                }
                values
            }
        });

        sender.send(1).unwrap();
        notify.notified().await;
        sender.send(2).unwrap();
        notify.notified().await;
        sender.send(3).unwrap();
        notify.notified().await;

        // Final value is guaranteed to be seen before error.
        sender.send(4).unwrap();
        drop(sender);
        let values = handle.await.unwrap();
        assert_eq!(values, vec![1, 2, 3, 4]);
    }

    #[asyncs::test]
    async fn ref_drop_release_value() {
        let shared = Arc::new(());

        let (sender, receiver) = watch::channel(shared.clone());
        assert_eq!(Arc::strong_count(&shared), 2);

        let borrowed1 = receiver.borrow();
        let borrowed2 = receiver.borrow();
        assert_eq!(Arc::strong_count(&shared), 2);
        sender.send(Arc::new(())).unwrap();
        assert_eq!(Arc::strong_count(&shared), 2);

        drop(borrowed1);
        assert_eq!(Arc::strong_count(&shared), 2);
        drop(borrowed2);
        assert_eq!(Arc::strong_count(&shared), 1);
    }
}
