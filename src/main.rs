use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    future::Future,
    marker::PhantomData,
    mem::MaybeUninit,
    os::raw::c_int,
    path::Path,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    time::{Duration, Instant}, borrow::BorrowMut,
};

use errno::{errno, Errno};
use libc::epoll_event;
use libc::itimerspec;
use libc::timespec;

macro_rules! syscall_may_fail {
    ($f: expr) => {{
        let res = unsafe { $f };
        if res == -1 {
            Err(Error::SystemCall(errno()))
        } else {
            Ok(res)
        }
    }};
}

async fn my_future() {
    println!("entering");

    delay(Duration::from_secs(3)).await;
    println!("3secs");

    delay(Duration::from_secs(2)).await;
    println!("5secs");
}

fn main() {
    Runtime::new().block_on(my_future());
}

#[derive(Debug)]
enum Error {
    SystemCall(Errno),
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct TaskID(u64);

struct Runtime {
    next_task_id: TaskID,
    wake_queue: VecDeque<TaskID>,
    _phantom: PhantomData<NonNull<()>>, // impl !Send & !Sync
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = RefCell::new(None);
    static EPOLL: RefCell<Option<Epoll>> = RefCell::new(None);
}

impl Runtime {
    fn new() -> Self {
        Self {
            next_task_id: TaskID(0),
            wake_queue: VecDeque::new(),
            _phantom: PhantomData,
        }
    }

    fn next_task_id(&mut self) -> TaskID {
        let next = self.next_task_id;
        self.next_task_id = TaskID(self.next_task_id.0 + 1);
        next
    }

    fn block_on<O>(mut self, mut future: impl Future<Output = O>) -> O {
        let kwaker = KWaker::new(self.next_task_id());
        let waker = unsafe { Waker::from_raw(kwaker.as_waker()) };
        let mut context = Context::from_waker(&waker);

        RUNTIME.with(|f| *f.borrow_mut() = Some(self));
        EPOLL.with(|f| *f.borrow_mut() = Some(Epoll::new()));

        loop {
            // TODO: support concurrent tasks
            let future = unsafe { Pin::new_unchecked(&mut future) };
            match future.poll(&mut context) {
                Poll::Ready(r) => return r,
                Poll::Pending => {}
            }

            EPOLL.with(|x| x.borrow_mut().as_mut().unwrap().tick());
        }
    }

    fn enqueue_wakeup(&mut self, task: TaskID) {
        self.wake_queue.push_back(task);
    }
}

struct KWaker {
    id: TaskID,
}

impl KWaker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        KWaker::clone,
        KWaker::wake,
        KWaker::wake_by_ref,
        KWaker::drop,
    );

    fn new(id: TaskID) -> Self {
        Self { id }
    }

    fn as_waker(&self) -> RawWaker {
        RawWaker::new(self as *const KWaker as *const (), &KWaker::VTABLE)
    }

    fn notify(&self) {
        RUNTIME.with(|runtime| {
            runtime
                .borrow_mut()
                .as_mut()
                .expect("runtime is not available")
                .enqueue_wakeup(self.id);
        })
    }

    unsafe fn clone(me: *const ()) -> RawWaker {
        let me = &*(me as *const KWaker);
        me.as_waker()
    }

    unsafe fn wake(me: *const ()) {
        let me = std::ptr::read(me as *const KWaker);
        me.notify();
    }

    unsafe fn wake_by_ref(me: *const ()) {
        let me = &*(me as *const KWaker);
        me.notify();
    }

    unsafe fn drop(me: *const ()) {
        drop(std::ptr::read(me as *const KWaker));
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct FileDescriptorBorrowed(c_int);

#[derive(Debug, PartialEq, Eq, Hash)]
struct FileDescriptorOwned(c_int);
impl Drop for FileDescriptorOwned {
    fn drop(&mut self) {
        syscall_may_fail!(libc::close(self.0)).expect("failed to close file descriptor");
    }
}
impl FileDescriptorOwned {
    fn to_borrowed(&self) -> FileDescriptorBorrowed {
        FileDescriptorBorrowed(self.0)
    }
}

struct Epoll {
    fd: FileDescriptorOwned,
    watchlist: HashMap<FileDescriptorBorrowed, Waker>,
}

impl Epoll {
    fn new() -> Self {
        let fd = syscall_may_fail!(libc::epoll_create(1)).expect("failed to create epoll instance");

        Self {
            fd: FileDescriptorOwned(fd),
            watchlist: HashMap::new(),
        }
    }

    fn watch(&mut self, fd: FileDescriptorBorrowed, w: Waker) {
        syscall_may_fail!(
            libc::epoll_ctl(
                self.fd.0,
                libc::EPOLL_CTL_ADD,
                fd.0,
                &mut libc::epoll_event {
                    events: libc::EPOLLIN as u32,

                    // accoding to sys/epoll.h, this field is union including file descriptor
                    u64: fd.0 as u64,
                },
            )
        ).expect("failed to put file descriptor to epoll instance");

        self.watchlist.insert(fd, w);
    }

    fn tick(&mut self) {
        const EVENTS_PER_ONCE: usize = 4;

        let mut events: [MaybeUninit<epoll_event>; EVENTS_PER_ONCE] =
            unsafe { MaybeUninit::uninit().assume_init() };

        let num_events_received = syscall_may_fail!(libc::epoll_wait(
            self.fd.0,
            events.as_mut_ptr() as *mut epoll_event,
            events.len() as i32,
            -1
        ))
        .expect("error from epoll_wait");

        for i in 0..num_events_received {
            let event = unsafe { events[i as usize].assume_init() };

            // accoding to sys/epoll.h, this field is union including file descriptor
            let fd = FileDescriptorBorrowed(event.u64 as i32);
            let waker = self
                .watchlist
                .get(&fd)
                .expect("received event which epoll doesn't know");

            waker.wake_by_ref();
        }
    }
}

struct TimerFd {
    fd: FileDescriptorOwned,
}

fn duration_to_timespec(d: Duration) -> timespec {
    timespec {
        tv_sec: d.as_secs() as i64,
        tv_nsec: d.subsec_nanos() as i64,
    }
}

impl TimerFd {
    fn new() -> Self {
        let fd = syscall_may_fail!(libc::timerfd_create(libc::CLOCK_BOOTTIME, 0))
            .expect("failed to create timerfd");

        Self {
            fd: FileDescriptorOwned(fd),
        }
    }

    fn disarm(&mut self) {
        syscall_may_fail!(libc::timerfd_settime(
            self.fd.0,
            0,
            &itimerspec {
                it_interval: duration_to_timespec(Duration::ZERO),
                it_value: duration_to_timespec(Duration::ZERO),
            },
            std::ptr::null_mut(),
        ))
        .expect("failed to disarm timerfd");
    }

    fn arm(&mut self, delay: impl Into<Option<Duration>>, interval: impl Into<Option<Duration>>) {
        let (delay, interval) = (delay.into(), interval.into());

        if delay.is_none() && interval.is_none() {
            panic!("you tried to disarm?");
        }

        syscall_may_fail!(libc::timerfd_settime(
            self.fd.0,
            0,
            &itimerspec {
                it_interval: duration_to_timespec(interval.unwrap_or(Duration::ZERO)),
                it_value: duration_to_timespec(delay.unwrap_or(Duration::ZERO)),
            },
            std::ptr::null_mut(),
        ))
        .expect("failed to arm timerfd");
    }
}

fn delay(d: Duration) -> DelayFuture {
    DelayFuture::new(d)
}

struct DelayFuture {
    delay: Duration,
    began_at: Option<Instant>,
    timerfd: Option<TimerFd>,
}

impl DelayFuture {
    fn new(d: Duration) -> Self {
        Self {
            delay: d,
            began_at: None,
            timerfd: None,
        }
    }
}

impl Future for DelayFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.began_at {
            Some(t) if t.elapsed() < self.delay => return Poll::Pending,
            Some(_) => return Poll::Ready(()),
            None => {}
        }

        let mut timerfd = TimerFd::new();

        self.began_at = Some(Instant::now());
        timerfd.arm(self.delay, None);

        EPOLL.with(|runtime| {
            runtime
                .borrow_mut()
                .as_mut()
                .expect("runtime is not available")
                .watch(timerfd.fd.to_borrowed(), cx.waker().clone());
        });

        self.timerfd = Some(timerfd);

        Poll::Pending
    }
}
