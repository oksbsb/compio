use std::{
    cell::RefCell,
    future::{ready, Future},
    io,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use async_task::{Runnable, Task};
use compio_buf::IntoInner;
use compio_driver::{
    op::Asyncify, AsRawFd, Key, OpCode, Proactor, ProactorBuilder, PushEntry, RawFd,
};
use compio_log::{debug, instrument};
use crossbeam_queue::SegQueue;
use futures_util::{future::Either, FutureExt};
use smallvec::SmallVec;

pub(crate) mod op;
#[cfg(feature = "time")]
pub(crate) mod time;

#[cfg(feature = "time")]
use crate::runtime::time::{TimerFuture, TimerRuntime};
use crate::{
    runtime::op::{OpFuture, OpRuntime},
    BufResult,
};

static RUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct RuntimeInner {
    id: usize,
    driver: RefCell<Proactor>,
    runnables: Arc<SegQueue<Runnable>>,
    op_runtime: RefCell<OpRuntime>,
    #[cfg(feature = "time")]
    timer_runtime: RefCell<TimerRuntime>,
}

impl RuntimeInner {
    pub fn new(builder: &ProactorBuilder) -> io::Result<Self> {
        Ok(Self {
            id: RUNTIME_COUNTER.fetch_add(1, Ordering::AcqRel),
            driver: RefCell::new(builder.build()?),
            runnables: Arc::new(SegQueue::new()),
            op_runtime: RefCell::default(),
            #[cfg(feature = "time")]
            timer_runtime: RefCell::new(TimerRuntime::new()),
        })
    }

    pub fn id(&self) -> usize {
        self.id
    }

    #[cfg(all(windows, feature = "event"))]
    pub unsafe fn handle_for(&self, user_data: usize) -> io::Result<compio_driver::NotifyHandle> {
        self.driver.borrow().handle_for(user_data)
    }

    // Safety: the return runnable should be scheduled.
    unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> Task<F::Output> {
        let runnables = self.runnables.clone();
        let handle = self
            .driver
            .borrow()
            .handle()
            .expect("cannot create notify handle of the proactor");
        let schedule = move |runnable| {
            runnables.push(runnable);
            handle.notify().ok();
        };
        let (runnable, task) = async_task::spawn_unchecked(future, schedule);
        runnable.schedule();
        task
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let mut result = None;
        unsafe { self.spawn_unchecked(async { result = Some(future.await) }) }.detach();
        loop {
            loop {
                let next_task = self.runnables.pop();
                if let Some(task) = next_task {
                    task.run();
                } else {
                    break;
                }
            }
            if let Some(result) = result.take() {
                return result;
            }
            self.poll();
        }
    }

    pub fn spawn<F: Future + 'static>(&self, future: F) -> Task<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    pub fn spawn_blocking<T: Send + Unpin + 'static>(
        &self,
        f: impl (FnOnce() -> T) + Send + Sync + Unpin + 'static,
    ) -> impl Future<Output = T> {
        let op = Asyncify::new(move || {
            let res = f();
            BufResult(Ok(0), res)
        });
        self.submit(op).map(|BufResult(_, op)| op.into_inner())
    }

    pub fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.driver.borrow_mut().attach(fd)
    }

    pub fn submit_raw<T: OpCode + 'static>(&self, op: T) -> PushEntry<Key<T>, BufResult<usize, T>> {
        self.driver.borrow_mut().push(op)
    }

    pub fn submit<T: OpCode + 'static>(&self, op: T) -> impl Future<Output = BufResult<usize, T>> {
        match self.submit_raw(op) {
            PushEntry::Pending(user_data) => {
                // Clear previous waker if exists.
                self.op_runtime.borrow_mut().remove(*user_data);
                Either::Left(OpFuture::new(user_data))
            }
            PushEntry::Ready(res) => Either::Right(ready(res)),
        }
    }

    #[cfg(feature = "time")]
    pub fn create_timer(&self, delay: std::time::Duration) -> impl Future<Output = ()> {
        let mut timer_runtime = self.timer_runtime.borrow_mut();
        if let Some(key) = timer_runtime.insert(delay) {
            Either::Left(TimerFuture::new(key))
        } else {
            Either::Right(std::future::ready(()))
        }
    }

    pub fn cancel_op<T>(&self, user_data: Key<T>) {
        self.op_runtime.borrow_mut().remove(*user_data);
        self.driver.borrow_mut().cancel(*user_data);
    }

    #[cfg(feature = "time")]
    pub fn cancel_timer(&self, key: usize) {
        self.timer_runtime.borrow_mut().cancel(key);
    }

    pub fn poll_task<T: OpCode>(
        &self,
        cx: &mut Context,
        user_data: Key<T>,
    ) -> Poll<BufResult<usize, T>> {
        instrument!(compio_log::Level::DEBUG, "poll_task", ?user_data,);
        let mut op_runtime = self.op_runtime.borrow_mut();
        let mut driver = self.driver.borrow_mut();
        if driver.has_result(*user_data) {
            debug!("has result");
            op_runtime.remove(*user_data);
            Poll::Ready(driver.pop::<T>(user_data))
        } else {
            debug!("update waker");
            op_runtime.update_waker(*user_data, cx.waker().clone());
            Poll::Pending
        }
    }

    #[cfg(feature = "time")]
    pub fn poll_timer(&self, cx: &mut Context, key: usize) -> Poll<()> {
        instrument!(compio_log::Level::DEBUG, "poll_timer", ?cx, ?key);
        let mut timer_runtime = self.timer_runtime.borrow_mut();
        if timer_runtime.contains(key) {
            debug!("pending");
            timer_runtime.update_waker(key, cx.waker().clone());
            Poll::Pending
        } else {
            debug!("ready");
            Poll::Ready(())
        }
    }

    fn poll(&self) {
        instrument!(compio_log::Level::DEBUG, "poll");
        #[cfg(not(feature = "time"))]
        let timeout = None;
        #[cfg(feature = "time")]
        let timeout = self.timer_runtime.borrow().min_timeout();
        debug!("timeout: {:?}", timeout);

        let mut entries = SmallVec::<[usize; 1024]>::new();
        let mut driver = self.driver.borrow_mut();
        match driver.poll(timeout, &mut entries) {
            Ok(_) => {
                debug!("poll driver ok, entries: {}", entries.len());
                for entry in entries {
                    self.op_runtime.borrow_mut().wake(entry);
                }
            }
            Err(e) => match e.kind() {
                io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {
                    debug!("expected error: {e}");
                }
                _ => panic!("{:?}", e),
            },
        }
        #[cfg(feature = "time")]
        self.timer_runtime.borrow_mut().wake();
    }
}

impl AsRawFd for RuntimeInner {
    fn as_raw_fd(&self) -> RawFd {
        self.driver.borrow().as_raw_fd()
    }
}

struct RuntimeContext {
    depth: usize,
    ptr: Weak<RuntimeInner>,
}

impl RuntimeContext {
    pub fn new() -> Self {
        Self {
            depth: 0,
            ptr: Weak::new(),
        }
    }

    pub fn inc_depth(&mut self) -> usize {
        let depth = self.depth;
        self.depth += 1;
        depth
    }

    pub fn dec_depth(&mut self) -> usize {
        self.depth -= 1;
        self.depth
    }

    pub fn set_runtime(&mut self, ptr: Weak<RuntimeInner>) -> Weak<RuntimeInner> {
        std::mem::replace(&mut self.ptr, ptr)
    }

    pub fn upgrade_runtime(&self) -> Option<Runtime> {
        self.ptr.upgrade().map(|inner| Runtime { inner })
    }
}

thread_local! {
    static CURRENT_RUNTIME: RefCell<RuntimeContext> = RefCell::new(RuntimeContext::new());
}

/// The async runtime of compio. It is a thread local runtime, and cannot be
/// sent to other threads.
#[derive(Clone)]
pub struct Runtime {
    inner: Rc<RuntimeInner>,
}

impl Runtime {
    /// Create [`Runtime`] with default config.
    pub fn new() -> io::Result<Self> {
        Self::builder().build()
    }

    /// Create a builder for [`Runtime`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Get the current running [`Runtime`].
    pub fn try_current() -> Option<Self> {
        CURRENT_RUNTIME.with_borrow(|r| r.upgrade_runtime())
    }

    /// Get the current running [`Runtime`].
    ///
    /// ## Panics
    ///
    /// This method will panic if there are no running [`Runtime`].
    pub fn current() -> Self {
        Self::try_current().expect("not in a compio runtime")
    }

    pub(crate) fn inner(&self) -> &RuntimeInner {
        &self.inner
    }

    /// Enter the runtime context. This runtime will be set as the `current`
    /// one.
    ///
    /// ## Panics
    ///
    /// When calling `Runtime::enter` multiple times, the returned guards
    /// **must** be dropped in the reverse order that they were acquired.
    /// Failure to do so will result in a panic and possible memory leaks.
    ///
    /// Do **not** do the following, this shows a scenario that will result in a
    /// panic and possible memory leak.
    ///
    /// ```should_panic
    /// use compio_runtime::Runtime;
    ///
    /// let rt1 = Runtime::new().unwrap();
    /// let rt2 = Runtime::new().unwrap();
    ///
    /// let enter1 = rt1.enter();
    /// let enter2 = rt2.enter();
    ///
    /// drop(enter1);
    /// drop(enter2);
    /// ```
    pub fn enter(&self) -> EnterGuard {
        EnterGuard::new(self)
    }

    /// Block on the future till it completes.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let guard = self.enter();
        guard.block_on(future)
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> Task<F::Output> {
        self.inner.spawn(future)
    }

    /// Spawns a blocking task in a new thread, and wait for it.
    ///
    /// The task will not be cancelled even if the future is dropped.
    pub fn spawn_blocking<T: Send + Unpin + 'static>(
        &self,
        f: impl (FnOnce() -> T) + Send + Sync + Unpin + 'static,
    ) -> impl Future<Output = T> {
        self.inner.spawn_blocking(f)
    }

    /// Attach a raw file descriptor/handle/socket to the runtime.
    ///
    /// You only need this when authoring your own high-level APIs. High-level
    /// resources in this crate are attached automatically.
    pub fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.inner.attach(fd)
    }

    /// Submit an operation to the runtime.
    ///
    /// You only need this when authoring your own [`OpCode`].
    pub fn submit<T: OpCode + 'static>(&self, op: T) -> impl Future<Output = BufResult<usize, T>> {
        self.inner.submit(op)
    }
}

impl AsRawFd for Runtime {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(feature = "criterion")]
impl criterion::async_executor::AsyncExecutor for Runtime {
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.block_on(future)
    }
}

#[cfg(feature = "criterion")]
impl criterion::async_executor::AsyncExecutor for &Runtime {
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        (**self).block_on(future)
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    proactor_builder: ProactorBuilder,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self {
            proactor_builder: ProactorBuilder::new(),
        }
    }

    /// Replace proactor builder.
    pub fn with_proactor(&mut self, builder: ProactorBuilder) -> &mut Self {
        self.proactor_builder = builder;
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self) -> io::Result<Runtime> {
        Ok(Runtime {
            inner: Rc::new(RuntimeInner::new(&self.proactor_builder)?),
        })
    }
}

/// Runtime context guard.
///
/// When the guard is dropped, exit the corresponding runtime context.
#[must_use]
pub struct EnterGuard<'a> {
    runtime: &'a Runtime,
    old_ptr: Weak<RuntimeInner>,
    depth: usize,
}

impl<'a> EnterGuard<'a> {
    fn new(runtime: &'a Runtime) -> Self {
        let (old_ptr, depth) = CURRENT_RUNTIME.with_borrow_mut(|ctx| {
            (
                ctx.set_runtime(Rc::downgrade(&runtime.inner)),
                ctx.inc_depth(),
            )
        });
        Self {
            runtime,
            old_ptr,
            depth,
        }
    }

    /// Block on the future in the runtime backed of this guard.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.inner.block_on(future)
    }
}

#[cold]
fn panic_incorrect_drop_order() {
    if !std::thread::panicking() {
        panic!(
            "`EnterGuard` values dropped out of order. Guards returned by `Runtime::enter()` must \
             be dropped in the reverse order as they were acquired."
        )
    }
}

impl Drop for EnterGuard<'_> {
    fn drop(&mut self) {
        let depth = CURRENT_RUNTIME.with_borrow_mut(|ctx| {
            ctx.set_runtime(std::mem::take(&mut self.old_ptr));
            ctx.dec_depth()
        });
        if depth != self.depth {
            panic_incorrect_drop_order()
        }
    }
}

/// Spawns a new asynchronous task, returning a [`Task`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion.
///
/// ```
/// # compio_runtime::Runtime::new().unwrap().block_on(async {
/// let task = compio_runtime::spawn(async {
///     println!("Hello from a spawned task!");
///     42
/// });
///
/// assert_eq!(task.await, 42);
/// # })
/// ```
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::current`].
pub fn spawn<F: Future + 'static>(future: F) -> Task<F::Output> {
    Runtime::current().spawn(future)
}

/// Spawns a blocking task in a new thread, and wait for it.
///
/// The task will not be cancelled even if the future is dropped.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::current`].
pub fn spawn_blocking<T: Send + Unpin + 'static>(
    f: impl (FnOnce() -> T) + Send + Sync + Unpin + 'static,
) -> impl Future<Output = T> {
    Runtime::current().spawn_blocking(f)
}
