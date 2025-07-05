//! Utility to park and unpark task.

extern crate alloc;

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::{self, *};
use core::task::{Poll, Waker};

const UNPARKED: usize = usize::MAX;

#[derive(Clone, Copy)]
enum State {
    Parking(usize),
    Unparked,
}

impl State {
    unsafe fn from(u: usize) -> State {
        match u {
            UNPARKED => State::Unparked,
            _ => State::Parking(u),
        }
    }
}

impl From<State> for usize {
    fn from(state: State) -> usize {
        match state {
            State::Unparked => UNPARKED,
            State::Parking(UNPARKED) => panic!("parker state overflow usize"),
            State::Parking(index) => index,
        }
    }
}

struct AtomicState(AtomicUsize);

impl Default for AtomicState {
    fn default() -> Self {
        AtomicState(AtomicUsize::new(State::Parking(0).into()))
    }
}

impl AtomicState {
    pub fn load(&self, ordering: Ordering) -> State {
        let u = self.0.load(ordering);
        unsafe { State::from(u) }
    }

    pub fn compare_exchange(
        &self,
        current: State,
        new: State,
        success: Ordering,
        failure: Ordering,
    ) -> Result<State, State> {
        match self.0.compare_exchange(current.into(), new.into(), success, failure) {
            Ok(_) => Ok(current),
            Err(updated) => unsafe { Err(State::from(updated)) },
        }
    }
}

trait MemoryOrdering {
    fn park_ordering() -> Ordering;
    fn unpark_ordering() -> Ordering;
}

/// No happens-before relationship between `unpark` and `park`.
pub enum WeakOrdering {}

impl MemoryOrdering for WeakOrdering {
    fn park_ordering() -> Ordering {
        Relaxed
    }

    fn unpark_ordering() -> Ordering {
        // Acquire load to observe waker.
        Acquire
    }
}

impl MemoryOrdering for StrongOrdering {
    fn park_ordering() -> Ordering {
        // Acquire load to observe all writes before `unpark`.
        Acquire
    }

    fn unpark_ordering() -> Ordering {
        // Acquire load to observe waker.
        // Release store to publish all writes before `unpark`.
        AcqRel
    }
}

/// Code before `unpark` happens before code after `park` that returns `Poll::Ready`.
pub enum StrongOrdering {}

#[allow(dead_code)]
pub(crate) type WeakParking = Parking<WeakOrdering>;
#[allow(dead_code)]
pub(crate) type StrongParking = Parking<StrongOrdering>;

pub(crate) struct Parking<T> {
    state: AtomicState,
    wakers: UnsafeCell<[Option<Waker>; 2]>,
    _marker: PhantomData<T>,
}

unsafe impl<T> Send for Parking<T> {}
unsafe impl<T> Sync for Parking<T> {}

#[allow(private_bounds)]
impl<T: MemoryOrdering> Parking<T> {
    pub fn new() -> Self {
        Self { state: AtomicState::default(), wakers: UnsafeCell::new(Default::default()), _marker: PhantomData }
    }

    /// # Panics
    /// Panic if unpark twice.
    ///
    /// # Safety
    /// Unsafe to unpark concurrently
    pub unsafe fn unpark(&self) -> Option<Waker> {
        let mut state = self.state.load(Relaxed);
        loop {
            match state {
                State::Unparked => unreachable!("unpark twice"),
                State::Parking(index) => {
                    match self.state.compare_exchange(state, State::Unparked, T::unpark_ordering(), Relaxed) {
                        Err(updated) => state = updated,
                        Ok(_) => {
                            let wakers = unsafe { &mut *self.wakers.get() };
                            return wakers[index].take();
                        },
                    }
                },
            }
        }
    }

    /// # Safety
    /// Unsafe to concurrent parks.
    pub unsafe fn park(&self, waker: &Waker) -> Poll<()> {
        let state = self.state.load(T::park_ordering());
        match state {
            State::Unparked => Poll::Ready(()),
            State::Parking(index) => {
                let index = (index + 1) & 0x01;
                let wakers = unsafe { &mut *self.wakers.get() };
                match unsafe { wakers.get_unchecked_mut(index) } {
                    existing_waker @ None => *existing_waker = Some(waker.clone()),
                    Some(existing_waker) => existing_waker.clone_from(waker),
                }
                // Release store to publish waker.
                match self.state.compare_exchange(state, State::Parking(index), Release, T::park_ordering()) {
                    Ok(_) => Poll::Pending,
                    Err(State::Unparked) => Poll::Ready(()),
                    Err(_) => unsafe { std::hint::unreachable_unchecked() },
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::sync::Arc;

    use asyncs::task;

    use super::{StrongParking, WeakParking};

    #[asyncs::test]
    async fn unpark_first() {
        let parking = WeakParking::new();

        unsafe {
            assert!(parking.unpark().is_none());
            assert!(parking.park(futures::task::noop_waker_ref()).is_ready());
        }
    }

    #[asyncs::test]
    async fn unpark_waker() {
        let parking = WeakParking::new();

        unsafe {
            assert!(parking.park(futures::task::noop_waker_ref()).is_pending());
            assert!(parking.unpark().is_some());
        }
    }

    #[asyncs::test]
    async fn parking_concurrent() {
        let shared = Arc::new(0usize);
        let parking = Arc::new(StrongParking::new());

        let handle = task::spawn({
            let shared = shared.clone();
            let parking = parking.clone();
            async move {
                future::poll_fn(|cx| unsafe { parking.park(cx.waker()) }).await;
                *shared
            }
        });

        #[allow(invalid_reference_casting)]
        let mutable = unsafe { &mut *(shared.as_ref() as *const usize as *mut usize) };
        *mutable = 5;
        if let Some(waker) = unsafe { parking.unpark() } {
            waker.wake();
        }

        assert_eq!(handle.await.unwrap(), 5);
    }

    #[asyncs::test]
    #[should_panic(expected = "unpark twice")]
    async fn unpark_twice() {
        let parking = WeakParking::new();

        unsafe {
            parking.unpark();
            parking.unpark();
        }
    }
}
