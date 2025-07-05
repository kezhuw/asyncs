use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{self, *};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use crate::parker::{Parking, WeakOrdering};

trait Node {
    fn link(&mut self) -> &mut Link<Self>;
}

struct Link<N: ?Sized> {
    next: Option<NonNull<N>>,
    prev: Option<NonNull<N>>,
}

impl<T> Default for Link<T> {
    fn default() -> Self {
        Self { next: None, prev: None }
    }
}

struct List<N: ?Sized> {
    head: Option<NonNull<N>>,
    tail: Option<NonNull<N>>,
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self { head: None, tail: None }
    }
}

impl<T: Node> List<T> {
    pub fn push_front(&mut self, node: &mut T) {
        let ptr = unsafe { NonNull::new_unchecked(node as *const T as *mut T) };
        if let Some(mut head) = self.head {
            unsafe {
                head.as_mut().link().prev = Some(ptr);
            }
        }
        let link = node.link();
        link.next = self.head;
        link.prev = None;
        self.head = Some(ptr);
        if self.tail.is_none() {
            self.tail = self.head;
        }
    }

    pub fn pop_back<'a>(&mut self) -> Option<&'a mut T> {
        let node = match self.tail {
            None => return None,
            Some(mut ptr) => unsafe { ptr.as_mut() },
        };
        self.tail = node.link().prev;
        match self.tail {
            None => self.head = None,
            Some(mut ptr) => unsafe { ptr.as_mut().link().next = None },
        }
        Some(node)
    }

    pub fn unlink(&mut self, node: &mut T) -> bool {
        let ptr = unsafe { NonNull::new_unchecked(node as *const T as *mut T) };
        let link = node.link();

        if let Some(mut next) = link.next {
            unsafe { next.as_mut().link().prev = link.prev };
        } else if self.tail == Some(ptr) {
            self.tail = link.prev;
        } else {
            return false;
        }

        if let Some(mut prev) = link.prev {
            unsafe { prev.as_mut().link().next = link.next };
        } else if self.head == Some(ptr) {
            self.head = link.next;
        } else {
            return false;
        }

        link.next = None;
        link.prev = None;

        true
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

struct GuardedList<'a, T> {
    empty: bool,
    guard: &'a mut T,
}

impl<'a, T: Node> GuardedList<'a, T> {
    pub fn new(list: List<T>, guard: &'a mut T) -> Self {
        let ptr = unsafe { NonNull::new_unchecked(guard as *mut T) };
        let link = guard.link();
        if list.is_empty() {
            link.next = Some(ptr);
            link.prev = Some(ptr);
        } else {
            link.next = list.head;
            link.prev = list.tail;
            unsafe {
                list.head.unwrap_unchecked().as_mut().link().prev = Some(ptr);
                list.tail.unwrap_unchecked().as_mut().link().next = Some(ptr);
            }
        }
        Self { empty: false, guard }
    }

    pub fn pop_back<'b>(&mut self) -> Option<&'b mut T> {
        let addr = self.guard as *mut _;
        let link = self.guard.link();
        let last = unsafe { link.prev.unwrap_unchecked().as_mut() };
        if ptr::addr_eq(addr, last) {
            self.empty = true;
            return None;
        }
        link.prev = last.link().prev;
        last.link().next = unsafe { Some(NonNull::new_unchecked(addr)) };
        Some(last)
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

struct WaiterList<'a> {
    list: GuardedList<'a, Waiter>,
    round: Round,
    notify: &'a Notify,
}

impl<'a> WaiterList<'a> {
    pub fn new(list: GuardedList<'a, Waiter>, round: Round, notify: &'a Notify) -> Self {
        Self { list, round, notify }
    }

    pub fn pop_back<'b>(&mut self, _lock: &mut std::sync::MutexGuard<'_, List<Waiter>>) -> Option<&'b mut Waiter> {
        self.list.pop_back()
    }
}

impl Drop for WaiterList<'_> {
    fn drop(&mut self) {
        if self.list.is_empty() {
            return;
        }
        let _lock = self.notify.list.lock().unwrap();
        while let Some(waiter) = self.list.pop_back() {
            waiter.notification.store(self.round.into_notification(NotificationKind::All), Release);
        }
    }
}

const STATUS_MASK: usize = 3usize;

const ROUND_UNIT: usize = STATUS_MASK + 1;
const ROUND_MASK: usize = !STATUS_MASK;

#[derive(Copy, Clone, Debug, PartialEq)]
struct Round(usize);

impl Round {
    const ZERO: Round = Self(0);

    pub fn new() -> Self {
        Self(ROUND_UNIT)
    }

    pub fn into_notification(self, kind: NotificationKind) -> Notification {
        Notification { kind, round: self }
    }

    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(ROUND_UNIT))
    }

    pub fn into(self) -> usize {
        self.0
    }

    pub fn from(i: usize) -> Self {
        Self(i & ROUND_MASK)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct State {
    round: Round,
    status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(usize)]
enum Status {
    Idle = 0,
    Waiting = 1,
    Notified = 2,
}

impl State {
    pub fn new() -> Self {
        Self { round: Round::new(), status: Status::Idle }
    }

    pub fn with_status(self, status: Status) -> Self {
        Self { round: self.round, status }
    }

    pub fn with_round(self, round: Round) -> Self {
        Self { round, status: self.status }
    }

    pub fn next_round(self) -> Self {
        self.with_round(self.round.next())
    }
}

struct AtomicState(AtomicUsize);

impl Default for AtomicState {
    fn default() -> Self {
        Self::new(State::new())
    }
}

impl AtomicState {
    pub fn new(state: State) -> Self {
        Self(AtomicUsize::new(state.into()))
    }

    pub fn store(&self, state: State, ordering: Ordering) {
        self.0.store(state.into(), ordering)
    }

    pub fn load(&self, ordering: Ordering) -> State {
        let u = self.0.load(ordering);
        State::from(u)
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
            Err(updated) => Err(State::from(updated)),
        }
    }
}

impl From<State> for usize {
    fn from(state: State) -> usize {
        state.round.into() | state.status as usize
    }
}

impl From<usize> for State {
    fn from(i: usize) -> Self {
        let status = i & STATUS_MASK;
        Self { round: Round::from(i), status: unsafe { std::mem::transmute::<usize, Status>(status) } }
    }
}

#[derive(Clone, Copy, PartialEq)]
#[repr(usize)]
enum NotificationKind {
    One = 0,
    All = 1,
}

#[derive(Clone, Copy)]
struct Notification {
    kind: NotificationKind,
    round: Round,
}

impl From<Notification> for usize {
    fn from(notification: Notification) -> usize {
        notification.round.into() | notification.kind as usize
    }
}

impl From<usize> for Notification {
    fn from(u: usize) -> Self {
        let kind = u & STATUS_MASK;
        Self { kind: unsafe { std::mem::transmute::<usize, NotificationKind>(kind) }, round: Round::from(u) }
    }
}

#[derive(Default)]
struct AtomicNotification(AtomicUsize);

impl AtomicNotification {
    pub fn clear(&mut self) {
        self.0.store(0, Relaxed)
    }

    pub fn take(&mut self) -> Option<Notification> {
        let notification = std::mem::take(self);
        notification.load(Relaxed)
    }

    pub fn load(&self, ordering: Ordering) -> Option<Notification> {
        match self.0.load(ordering) {
            0 => None,
            u => Some(Notification::from(u)),
        }
    }

    pub fn store(&self, notification: Notification, ordering: Ordering) {
        self.0.store(notification.into(), ordering)
    }
}

/// Notifies one task or all attached tasks to wakeup.
///
/// [notify_one] and [notified().await] behave similar to [Thread::unpark] and [thread::park]
/// except that [notified().await] will not be waked up spuriously. One could assume that there is
/// at most one permit associated with [Notify]. [notified().await] will block current task unless
/// or until the permit is available to consume. [notify_one] release the permit for [notified().await]
/// to acquire, it will wake up [Notified] in FIFO order if there are multiple [Notified]s blocking
/// for the permit. The order of [Notified]s are the order of [notified().await] or [Notified::enable()]
/// whichever first.
///
/// [notify_all], on the other hand, will wake up all attached [Notified]s and start a fresh new round
/// for [notify_one] with no permit. [Notify::notified()]s are attached by default, one could use
/// [Notified::detach] to detach from rounds of [Notify] until [Notified::enable] or future polling.
///
/// ## Differences with [tokio]
/// * [tokio::sync::Notify::notify_all()] does not clear permit from [notify_one].
/// * [tokio] does not have [Notified::detach()].
///
/// [thread::park]: std::thread::park
/// [Thread::unpark]: std::thread::Thread::unpark
/// [notified().await]: Notify::notified()
/// [notify_one]: Notify::notify_one()
/// [notify_all]: Notify::notify_all()
/// [tokio]: https://docs.rs/tokio
/// [tokio::sync::Notify::notify_all()]: https://docs.rs/tokio/latest/tokio/sync/struct.Notify.html#method.notify_one
#[derive(Default)]
pub struct Notify {
    // All link operations are guarded by this lock including GuardedList which actually is an
    // independent list.
    list: Mutex<List<Waiter>>,
    state: AtomicState,
}

unsafe impl Send for Notify {}
unsafe impl Sync for Notify {}

impl Notify {
    /// Constructs a new [Notify].
    pub fn new() -> Self {
        Self::default()
    }

    /// Constructs a attached [Notified] to consume permit from [Notify::notify_one].
    pub fn notified(&self) -> Notified<'_> {
        let round = self.round();
        Notified { notify: self, stage: Stage::default(), round, waiter: Waiter::default() }
    }

    /// Notifies one waiting task or stores a permit to consume in case of no waiting task.
    pub fn notify_one(&self) {
        let state = self.state.load(SeqCst);
        self.notify_one_in_round(state.round, state);
    }

    /// Notifies all attached [Notified]s and starts a fresh new round with no permit.
    pub fn notify_all(&self) {
        let mut state = self.state.load(SeqCst);
        loop {
            while state.status != Status::Waiting {
                match self.state.compare_exchange(state, state.next_round().with_status(Status::Idle), Release, Relaxed)
                {
                    Ok(_) => return,
                    Err(updated) => state = updated,
                }
            }
            let mut list = self.list.lock().unwrap();
            state = self.state.load(Relaxed);
            if state.status != Status::Waiting {
                drop(list);
                continue;
            }

            // Release store to publish changes.
            self.state.store(state.next_round().with_status(Status::Idle), Release);

            let mut guard = Waiter::default();
            let mut wakers = WakerList::new();
            let mut waiters =
                WaiterList::new(GuardedList::new(std::mem::take(&mut list), &mut guard), state.round, self);

            'list: loop {
                while !wakers.is_full() {
                    let Some(waiter) = waiters.pop_back(&mut list) else {
                        break 'list;
                    };
                    let waker = unsafe { waiter.parking.unpark() };
                    waiter.notification.store(state.round.into_notification(NotificationKind::All), Release);
                    if let Some(waker) = waker {
                        wakers.push(waker)
                    }
                }
                drop(list);
                wakers.wake();
                list = self.list.lock().unwrap();
            }
            drop(list);
            wakers.wake();
            return;
        }
    }

    fn remove(&self, waiter: &mut Waiter) {
        let notification = match waiter.notification.load(Acquire) {
            None => {
                let mut list = self.list.lock().unwrap();
                if list.unlink(waiter) && list.is_empty() {
                    let state = self.state.load(Relaxed);
                    if state.status == Status::Waiting {
                        self.state.store(state.with_status(Status::Idle), Relaxed);
                    }
                }
                drop(list);
                // Relaxed load as nothing is important in case of drop.
                let Some(notification) = waiter.notification.load(Relaxed) else {
                    return;
                };
                notification
            },
            Some(notification) => notification,
        };
        if notification.kind == NotificationKind::One {
            self.release_notification(notification.round);
        }
    }

    fn poll(&self, waiter: &mut Waiter, round: Round) -> Poll<Notification> {
        let mut state = self.state.load(SeqCst);
        let round = if round == Round::ZERO { state.round } else { round };
        loop {
            if state.round != round {
                return Poll::Ready(round.into_notification(NotificationKind::All));
            }
            if state.status != Status::Notified {
                break;
            }
            // Acquire load to observe changes in case of `notify_all`.
            match self.state.compare_exchange(state, state.with_status(Status::Idle), Acquire, Acquire) {
                Ok(_) => return Poll::Ready(state.round.into_notification(NotificationKind::One)),
                Err(updated) => state = updated,
            }
        }
        let mut list = self.list.lock().unwrap();
        state = self.state.load(SeqCst);
        loop {
            if state.round != round {
                drop(list);
                return Poll::Ready(round.into_notification(NotificationKind::All));
            }
            match state.status {
                Status::Waiting => break,
                Status::Idle => {
                    match self.state.compare_exchange(state, state.with_status(Status::Waiting), Relaxed, Relaxed) {
                        Ok(_) => break,
                        Err(updated) => state = updated,
                    }
                },
                Status::Notified => {
                    match self.state.compare_exchange(state, state.with_status(Status::Idle), Acquire, Relaxed) {
                        Ok(_) => {
                            drop(list);
                            return Poll::Ready(state.round.into_notification(NotificationKind::One));
                        },
                        Err(updated) => state = updated,
                    }
                },
            }
        }
        list.push_front(waiter);
        drop(list);
        Poll::Pending
    }

    fn notify_one_in_round(&self, round: Round, mut state: State) {
        loop {
            loop {
                // There are must be at least one `notify_all`, all waiters from this round must be
                // notified.
                if state.round != round {
                    return;
                }
                if state.status == Status::Waiting {
                    break;
                }
                // Release store to transfer happens-before relationship.
                match self.state.compare_exchange(state, state.with_status(Status::Notified), Release, Relaxed) {
                    Ok(_) => return,
                    Err(updated) => state = updated,
                }
            }
            let mut list = self.list.lock().unwrap();
            let state = self.state.load(Relaxed);
            if state.round != round {
                return;
            }
            if state.status != Status::Waiting {
                drop(list);
                continue;
            }
            let waiter = list.pop_back().unwrap();
            let waker = unsafe { waiter.parking.unpark() };
            waiter.notification.store(state.round.into_notification(NotificationKind::One), Release);
            if list.is_empty() {
                self.state.store(state.with_status(Status::Idle), Relaxed);
            }
            drop(list);
            if let Some(waker) = waker {
                waker.wake();
            }
            return;
        }
    }

    fn round(&self) -> Round {
        self.state.load(SeqCst).round
    }

    fn release_notification(&self, round: Round) {
        let state = self.state.load(SeqCst);
        self.notify_one_in_round(round, state);
    }
}

struct WakerList {
    next: usize,
    wakers: [MaybeUninit<Waker>; 32],
}

impl WakerList {
    pub fn new() -> Self {
        Self { next: 0, wakers: std::array::from_fn(|_| MaybeUninit::uninit()) }
    }

    pub fn is_full(&self) -> bool {
        self.next == self.wakers.len()
    }

    pub fn push(&mut self, waker: Waker) {
        debug_assert!(self.next < self.wakers.len());
        self.wakers[self.next].write(waker);
        self.next += 1;
    }

    pub fn wake(&mut self) {
        while self.next != 0 {
            self.next -= 1;
            let waker = unsafe { self.wakers[self.next].assume_init_read() };
            waker.wake();
        }
    }
}

impl Drop for WakerList {
    fn drop(&mut self) {
        while self.next != 0 {
            self.next -= 1;
            unsafe {
                self.wakers[self.next].assume_init_drop();
            }
        }
    }
}

struct Waiter {
    link: Link<Waiter>,
    parking: Parking<WeakOrdering>,

    /// Release store to release connection to `Waiter`.
    /// Acquire load to observe all changes.
    notification: AtomicNotification,
}

impl Default for Waiter {
    fn default() -> Self {
        Self { link: Link::default(), parking: Parking::new(), notification: AtomicNotification::default() }
    }
}

impl Node for Waiter {
    fn link(&mut self) -> &mut Link<Waiter> {
        &mut self.link
    }
}

#[repr(usize)]
#[derive(Default, Debug, Copy, Clone, PartialEq)]
enum Stage {
    #[default]
    Init = 0,
    Waiting = 1,
    Finished = 2,
}

/// Future created from [Notify::notified()].
pub struct Notified<'a> {
    notify: &'a Notify,

    stage: Stage,
    round: Round,

    waiter: Waiter,
}

unsafe impl Send for Notified<'_> {}
unsafe impl Sync for Notified<'_> {}

impl<'a> Notified<'a> {
    /// Enables to wait for a notification from [Notify::notify_one] or [Notify::notify_all].
    ///
    /// If there is permit from [Notify::notify_one], this will consume it temporarily for future
    /// polling. If this [Notified] is dropped without further polling, the permit will be handed
    /// over to [Notify] in case of no new [Notify::notify_all].
    ///
    /// [Notified::poll] will enable this also.
    pub fn enable(mut self: Pin<&mut Self>) {
        if self.stage != Stage::Init {
            return;
        }
        let round = self.round;
        if let Poll::Ready(notification) = self.notify.poll(&mut self.waiter, round) {
            self.stage = Stage::Finished;
            self.waiter.notification.store(notification, Relaxed);
        } else {
            self.stage = Stage::Waiting;
        }
    }

    /// Detaches from rounds of [Notify] so it will not be notified until [Notified::enable] or
    /// [Notified::poll].
    pub fn detach(mut self) -> Notified<'a> {
        self.round = Round::ZERO;
        self
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let round = self.round;
        match self.stage {
            Stage::Init => match self.notify.poll(&mut self.waiter, round) {
                Poll::Pending => self.stage = Stage::Waiting,
                Poll::Ready(_) => {
                    self.stage = Stage::Finished;
                    return Poll::Ready(());
                },
            },
            Stage::Waiting => match self.waiter.notification.load(Acquire) {
                None => {},
                Some(_) => {
                    self.waiter.notification.clear();
                    self.stage = Stage::Finished;
                    return Poll::Ready(());
                },
            },
            Stage::Finished => {
                // We could come from `enable`.
                self.waiter.notification.clear();
                return Poll::Ready(());
            },
        }
        debug_assert_eq!(self.stage, Stage::Waiting);
        if unsafe { self.waiter.parking.park(cx.waker()).is_ready() } {
            while self.waiter.notification.load(Acquire).is_none() {
                std::hint::spin_loop();
            }
            self.waiter.notification.clear();
            self.stage = Stage::Finished;
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        match self.stage {
            Stage::Init => {},
            Stage::Waiting => self.notify.remove(&mut self.waiter),
            Stage::Finished => {
                if let Some(Notification { round, kind: NotificationKind::One }) = self.waiter.notification.take() {
                    self.notify.release_notification(round);
                }
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use std::pin::{pin, Pin};

    use asyncs::select;

    use super::Notify;

    #[asyncs::test]
    async fn notify_one_simple() {
        let notify = Notify::new();

        // given: two notifieds polled in order
        let mut notified1 = notify.notified();
        let mut notified2 = notify.notified();
        select! {
            biased;
            default => {},
            _ = &mut notified1 => unreachable!(),
            _ = &mut notified2 => unreachable!(),
        }

        // when: notify_one
        notify.notify_one();

        // then: only the first polled got notified
        select! {
            biased;
            default => unreachable!(),
            _ = &mut notified2 => unreachable!(),
            _ = &mut notified1 => {}
        }

        // when: another notify_one
        notify.notify_one();
        // then: other got notified
        select! {
            default => unreachable!(),
            _ = &mut notified2 => {},
        }
    }

    #[asyncs::test]
    async fn notify_one_enabled() {
        let notify = Notify::new();
        let notified1 = notify.notified();
        let mut notified1 = pin!(notified1);
        let mut notified2 = notify.notified();

        // given: enabled notified
        notified1.as_mut().enable();
        select! {
            default => {},
            _ = &mut notified2 => unreachable!(),
        }

        // when: notify_one
        notify.notify_one();

        // then: enabled notified behaves same as polled notified
        notified1.await;

        select! {
            default => {},
            _ = &mut notified2 => unreachable!(),
        }
    }

    #[asyncs::test]
    async fn notify_one_permit_does_not_acculumate() {
        let notify = Notify::new();

        // given: two notifieds
        let notified1 = notify.notified();
        let notified2 = notify.notified();

        // when: notify_one twice
        notify.notify_one();
        notify.notify_one();

        // then: only one permit
        select! {
            default => unreachable!(),
            _ = notified1 => {},
        };
        select! {
            default => {},
            _ = notified2 => unreachable!(),
        };
    }

    #[asyncs::test]
    async fn notify_one_permit_consumed_by_poll() {
        let notify = Notify::new();
        let mut notified1 = notify.notified();
        let notified2 = notify.notified();

        // given: notify_one permit
        notify.notify_one();

        // when: poll and drop
        select! {
            default => unreachable!(),
            _ = &mut notified1 => {},
        };
        drop(notified1);

        // then: no permit resumed
        select! {
            default => {},
            _ = notified2 => unreachable!(),
        };
    }

    #[asyncs::test]
    async fn notify_one_permit_doesnot_consumed_by_enable() {
        let notify = Notify::new();
        let mut notified1 = notify.notified();
        let notified2 = notify.notified();

        // given: notify_one permit
        notify.notify_one();

        // when: enable and drop notified
        unsafe {
            Pin::new_unchecked(&mut notified1).enable();
        }
        drop(notified1);

        // then: notify_one permit resumed
        select! {
            default => unreachable!(),
            _ = notified2 => {},
        };
    }

    #[asyncs::test]
    async fn notify_one_permit_unconsumed_resumed_on_drop() {
        let notify = Notify::new();

        // given: enabled/polled notified
        let mut notified1 = notify.notified();
        select! {
            default => {},
            _ = &mut notified1 => unreachable!(),
        };

        // when: notify_one and drop with no further poll
        notify.notify_one();
        drop(notified1);

        // then: unconsumed notify_one will be resumed
        let notified2 = notify.notified();
        select! {
            default => unreachable!(),
            _ = notified2 => {},
        };
    }

    #[asyncs::test]
    async fn notify_one_permit_does_not_resumed_cross_round() {
        let notify = Notify::new();

        // given: enabled/polled notified
        let mut notified1 = notify.notified();
        select! {
            default => {},
            _ = &mut notified1 => unreachable!(),
        };

        // when: notify_one and drop after notify_all with no further poll
        notify.notify_one();
        notify.notify_all();
        drop(notified1);

        // then: unconsumed notify_one will not be resumed cross round
        let notified2 = notify.notified();
        select! {
            default => {},
            _ = notified2 => unreachable!(),
        };
    }

    #[asyncs::test]
    async fn notify_all_simple() {
        let notify = Notify::new();

        // given: not enabled notified
        let mut notified1 = notify.notified().detach();
        let mut notified2 = notify.notified().detach();
        let mut notified3 = notify.notified();

        // when: notify_all
        notify.notify_all();

        // then: only attached ones got notified
        select! {
            // So all notifieds got polled
            biased;
            default => unreachable!(),
            _ = &mut notified1 => unreachable!("not ready"),
            _ = &mut notified2 => unreachable!("not ready"),
            _ = &mut notified3 => {},
        };

        // given: polled notified
        // when: notify_all
        notify.notify_all();

        // then: notified
        select! {
            default => unreachable!(),
            _ = &mut notified1 => {},
        };

        select! {
            default => unreachable!(),
            _ = &mut notified2 => {},
        };
    }

    #[asyncs::test]
    async fn notify_all_enabled() {
        let notify = Notify::new();
        let notified = notify.notified();

        // given: enabled notified
        let mut notified = pin!(notified);
        notified.as_mut().enable();

        // when: notify_all
        notify.notify_all();

        // then: notified
        select! {
            default => unreachable!(),
            _ = notified => {},
        };
    }

    #[asyncs::test]
    async fn notify_all_ruin_permit() {
        let notify = Notify::new();

        // given: a detached Notified
        let notified = notify.notified().detach();

        // when: notify_one and then notify_all
        notify.notify_one();
        notify.notify_all();

        // then: permit got cleared
        select! {
            default => {},
            _ = notified => unreachable!(),
        }
    }

    #[asyncs::test]
    async fn notify_unlink() {
        let notify = Notify::new();

        let mut notified1 = notify.notified();
        let mut notified2 = notify.notified();

        select! {
            default => {},
            _ = &mut notified1 => unreachable!(),
            _ = &mut notified2 => unreachable!(),
        }

        let mut notified3 = notify.notified();
        unsafe { Pin::new_unchecked(&mut notified3).enable() };

        unsafe {
            std::ptr::drop_in_place(&mut notified1);
        }
        unsafe {
            std::ptr::drop_in_place(&mut notified2);
        }
        unsafe {
            std::ptr::drop_in_place(&mut notified3);
        }

        std::mem::forget(notified1);
        std::mem::forget(notified2);
        std::mem::forget(notified3);

        notify.notify_all();
    }
}
