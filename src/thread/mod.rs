#![allow(warnings, missing_docs)]

use std::prelude::v1::*;

use std::cell::RefCell;
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use {Async, Poll, Stream};
use executor::{self, spawn, Spawn, NotifyHandle, Notify};
use future::{Future, Executor, ExecuteError};
use stream::FuturesUnordered;

mod sink;
mod stream;

pub use self::sink::BlockingSink;
pub use self::stream::BlockingStream;

pub fn block_until<F: Future>(f: F) -> Result<F::Item, F::Error> {
    let mut future = spawn(f);
    let guard = RunningThread::new();
    loop {
        match future.poll_future_notify(&guard.thread, 0)? {
            Async::NotReady => guard.thread.park(),
            Async::Ready(e) => return Ok(e),
        }
    }
}

pub fn block_on_all<F>(f: F)
    where F: FnOnce(&Spawner)
{
    let mut el = EventLoop::new();
    f(&el.spawner());
    el.block_on_all();
}

/// Convenience struct combining an executor::Guard with the curretn thread..
pub struct RunningThread {
    guard: executor::Guard,
    thread: Arc<ThreadUnpark>
}

impl RunningThread {
    pub fn new() -> RunningThread {
        let thread = Arc::new(ThreadUnpark::new(thread::current()));
        let guard = executor::enter();
        RunningThread { thread: thread, guard: guard }
    }

    pub fn park_thread(&self) {
        self.thread.park()
    }

    pub fn park_thread_timeout(&self, dur: Duration) {
        self.thread.park_timeout(dur);
    }
}

impl fmt::Debug for RunningThread {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "RunningThread")
    }
}

// An object for cooperatively executing multiple tasks on a single thread.
// Useful for working with non-`Send` futures.
//
// NB: this is not `Send`
pub struct EventLoop {
    inner: Rc<Inner>,
    non_daemons: usize,
    futures: Spawn<FuturesUnordered<SpawnedFuture>>,
}

struct Inner {
    new_futures: RefCell<Vec<(Box<Future<Item=(), Error=()>>, bool)>>,
}

impl EventLoop {
    pub fn new() -> EventLoop {
        EventLoop {
            non_daemons: 0,
            futures: spawn(FuturesUnordered::new()),
            inner: Rc::new(Inner {
                new_futures: RefCell::new(Vec::new()),
            })
        }
    }

    pub fn spawner(&self) -> Spawner {
        Spawner { inner: Rc::downgrade(&self.inner) }
    }

    pub fn block_on_all(&mut self) {
        let guard = RunningThread::new();
        loop {
            self.iterate(&guard);
            if self.non_daemons == 0 {
                break
            }
            guard.thread.park();
        }
    }

    pub fn block_until<F: Future>(&mut self, f: F) -> Result<F::Item, F::Error> {
        let guard = RunningThread::new();
        let mut future = spawn(f);
        loop {
            let abort_condition = future.poll_future_notify(&guard.thread, 0)?;
            if let Async::Ready(e) = abort_condition {
                return Ok(e)
            }
            self.iterate(&guard);
            guard.thread.park();
        }
    }

    pub fn iterate(&mut self, guard: &RunningThread) {
        self.poll_all(&|me| me.futures.poll_stream_notify(&guard.thread, 0));
    }

    fn poll_all(&mut self, f: &Fn(&mut EventLoop) -> Poll<Option<bool>, bool>) {
        loop {
            // Make progress on all spawned futures as long as we can
            loop {
                match f(self) {
                    // If a daemon exits, then we ignore it, but if a non-daemon
                    // exits then we update our counter of non-daemons
                    Ok(Async::Ready(Some(daemon))) |
                    Err(daemon) => {
                        if !daemon {
                            self.non_daemons -= 1;
                        }
                    }
                    Ok(Async::NotReady) |
                    Ok(Async::Ready(None)) => break,
                }
            }

            // Now that we've made as much progress as we can, check our list of
            // spawned futures to see if anything was spawned
            let mut futures = self.inner.new_futures.borrow_mut();
            if futures.len() == 0 {
                break
            }
            for (future, daemon) in futures.drain(..) {
                if !daemon {
                    self.non_daemons += 1;
                }
                self.futures.get_mut().push(SpawnedFuture {
                    daemon: daemon,
                    inner: future,
                });
            }
        }
    }
}

impl Future for EventLoop {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        self.poll_all(&|me| me.futures.get_mut().poll());
        Ok(Async::NotReady)
    }
}

impl fmt::Debug for EventLoop {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventLoop").finish()
    }
}

struct SpawnedFuture {
    daemon: bool,
    // TODO: wrap in `Spawn`
    inner: Box<Future<Item = (), Error = ()>>,
}

impl Future for SpawnedFuture {
    type Item = bool;
    type Error = bool;

    fn poll(&mut self) -> Poll<bool, bool> {
        match self.inner.poll() {
            Ok(e) => Ok(e.map(|()| self.daemon)),
            Err(()) => Err(self.daemon),
        }
    }
}

#[derive(Clone)]
pub struct Spawner {
    inner: Weak<Inner>,
}

impl Spawner {
    pub fn spawn<F>(&self, task: F)
        where F: Future<Item = (), Error = ()> + 'static
    {
        self._spawn(Box::new(task), false);
    }

    pub fn spawn_daemon<F>(&self, task: F)
        where F: Future<Item = (), Error = ()> + 'static
    {
        self._spawn(Box::new(task), true);
    }

    fn _spawn(&self,
              future: Box<Future<Item = (), Error = ()>>,
              daemon: bool) {
        let inner = match self.inner.upgrade() {
            Some(i) => i,
            None => return,
        };
        inner.new_futures.borrow_mut().push((future, daemon));
    }
}

impl<F> Executor<F> for Spawner
    where F: Future<Item = (), Error = ()> + 'static
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        self.spawn(future);
        Ok(())
    }
}

impl fmt::Debug for Spawner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Spawner").finish()
    }
}

struct ThreadUnpark {
    thread: thread::Thread,
    ready: AtomicBool,
}

impl ThreadUnpark {
    fn new(thread: thread::Thread) -> ThreadUnpark {
        ThreadUnpark {
            thread: thread,
            ready: AtomicBool::new(false),
        }
    }

    fn park(&self) {
        if !self.ready.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }

    fn park_timeout(&self, dur: Duration) {
        if !self.ready.swap(false, Ordering::SeqCst) {
            thread::park_timeout(dur);
        }
    }
}

impl Notify for ThreadUnpark {
    fn notify(&self, _unpark_id: usize) {
        self.ready.store(true, Ordering::SeqCst);
        self.thread.unpark()
    }
}
