//! Monoio Legacy Driver.

use std::{
    cell::UnsafeCell,
    io,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};

use super::{
    op::{CompletionMeta, Op, OpAble},
    poll::Poll as LegacyPoll,
    ready::{self, Ready},
    Driver, Inner, CURRENT,
};

#[allow(missing_docs, unreachable_pub, dead_code, unused_imports)]
#[cfg(windows)]
pub(super) mod iocp;

#[cfg(feature = "sync")]
mod waker;
#[cfg(feature = "sync")]
pub(crate) use waker::UnparkHandle;

pub(crate) struct LegacyInner {
    pub(crate) poller: LegacyPoll,

    #[cfg(feature = "sync")]
    shared_waker: std::sync::Arc<waker::EventWaker>,

    // Waker receiver
    #[cfg(feature = "sync")]
    waker_receiver: flume::Receiver<std::task::Waker>,
}

/// Driver with Poll-like syscall.
#[allow(unreachable_pub)]
pub struct LegacyDriver {
    inner: Rc<UnsafeCell<LegacyInner>>,

    // Used for drop
    #[cfg(feature = "sync")]
    thread_id: usize,
}

#[cfg(feature = "sync")]
const TOKEN_WAKEUP: mio::Token = mio::Token(1 << 31);

#[allow(dead_code)]
impl LegacyDriver {
    const DEFAULT_ENTRIES: u32 = 1024;

    pub(crate) fn new() -> io::Result<Self> {
        Self::new_with_entries(Self::DEFAULT_ENTRIES)
    }

    pub(crate) fn new_with_entries(entries: u32) -> io::Result<Self> {
        let poller = LegacyPoll::with_capacity(entries as usize)?;
        #[cfg(all(unix, feature = "sync"))]
        let shared_waker = std::sync::Arc::new(waker::EventWaker::new(mio::Waker::new(
            poller.poll.registry(),
            TOKEN_WAKEUP,
        )?));
        #[cfg(all(windows, feature = "sync"))]
        let shared_waker = std::sync::Arc::new(waker::EventWaker::new(iocp::Waker::new(
            &poll,
            TOKEN_WAKEUP,
        )?));
        #[cfg(feature = "sync")]
        let (waker_sender, waker_receiver) = flume::unbounded::<std::task::Waker>();
        #[cfg(feature = "sync")]
        let thread_id = crate::builder::BUILD_THREAD_ID.with(|id| *id);

        let inner = LegacyInner {
            poller,
            #[cfg(feature = "sync")]
            shared_waker,
            #[cfg(feature = "sync")]
            waker_receiver,
        };
        let driver = Self {
            inner: Rc::new(UnsafeCell::new(inner)),
            #[cfg(feature = "sync")]
            thread_id,
        };

        // Register unpark handle
        #[cfg(feature = "sync")]
        {
            let unpark = driver.unpark();
            super::thread::register_unpark_handle(thread_id, unpark.into());
            super::thread::register_waker_sender(thread_id, waker_sender);
        }

        Ok(driver)
    }

    fn inner_park(&self, mut timeout: Option<Duration>) -> io::Result<()> {
        let inner = unsafe { &mut *self.inner.get() };

        #[allow(unused_mut)]
        let mut need_wait = true;
        #[cfg(feature = "sync")]
        {
            // Process foreign wakers
            while let Ok(w) = inner.waker_receiver.try_recv() {
                w.wake();
                need_wait = false;
            }

            // Set status as not awake if we are going to sleep
            if need_wait {
                inner
                    .shared_waker
                    .awake
                    .store(false, std::sync::atomic::Ordering::Release);
            }

            // Process foreign wakers left
            while let Ok(w) = inner.waker_receiver.try_recv() {
                w.wake();
                need_wait = false;
            }
        }

        if !need_wait {
            timeout = Some(Duration::ZERO);
        }

        match inner.poller.poll_inside(timeout) {
            Ok(_) => {}
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        let events = &inner.poller.events;
        #[cfg(unix)]
        let iter = events.iter();
        #[cfg(windows)]
        let iter = events.events.iter();
        for event in iter {
            let token = event.token();

            #[cfg(feature = "sync")]
            if token != TOKEN_WAKEUP {
                LegacyPoll::dispatch(
                    &mut inner.poller.io_dispatch,
                    token.0,
                    Ready::from_mio(event),
                );
            }

            #[cfg(not(feature = "sync"))]
            LegacyPoll::dispatch(
                &mut inner.poller.io_dispatch,
                token.0,
                Ready::from_mio(event),
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn register(
        this: &Rc<UnsafeCell<LegacyInner>>,
        state: &mut iocp::SocketState,
        interest: mio::Interest,
    ) -> io::Result<usize> {
        let inner = unsafe { &mut *this.get() };
        inner.poller.register(state, interest)
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn deregister(
        this: &Rc<UnsafeCell<LegacyInner>>,
        token: usize,
        state: &mut iocp::SocketState,
    ) -> io::Result<()> {
        let inner = unsafe { &mut *this.get() };
        inner.poller.deregister(token, state)
    }

    #[cfg(unix)]
    #[inline]
    pub(crate) fn register(
        this: &Rc<UnsafeCell<LegacyInner>>,
        source: &mut impl mio::event::Source,
        interest: mio::Interest,
    ) -> io::Result<usize> {
        let inner = unsafe { &mut *this.get() };
        inner.poller.register(source, interest)
    }

    #[cfg(unix)]
    #[inline]
    pub(crate) fn deregister(
        this: &Rc<UnsafeCell<LegacyInner>>,
        token: usize,
        source: &mut impl mio::event::Source,
    ) -> io::Result<()> {
        let inner = unsafe { &mut *this.get() };
        inner.poller.deregister(source, token)
    }
}

impl LegacyInner {
    pub(crate) fn poll_op<T: OpAble>(
        this: &Rc<UnsafeCell<Self>>,
        data: &mut T,
        cx: &mut Context<'_>,
    ) -> Poll<CompletionMeta> {
        let inner = unsafe { &mut *this.get() };
        let (direction, index) = match data.legacy_interest() {
            Some(x) => x,
            None => {
                // if there is no index provided, it means the action does not rely on fd
                // readiness. do syscall right now.
                return Poll::Ready(CompletionMeta {
                    result: OpAble::legacy_call(data),
                    flags: 0,
                });
            }
        };

        // wait io ready and do syscall
        let mut scheduled_io = inner
            .poller
            .io_dispatch
            .get(index)
            .expect("scheduled_io lost");
        let ref_mut = scheduled_io.as_mut();

        let readiness = ready!(ref_mut.poll_readiness(cx, direction));

        // check if canceled
        if readiness.is_canceled() {
            // clear CANCELED part only
            ref_mut.clear_readiness(readiness & Ready::CANCELED);
            return Poll::Ready(CompletionMeta {
                result: Err(io::Error::from_raw_os_error(125)),
                flags: 0,
            });
        }

        match OpAble::legacy_call(data) {
            Ok(n) => Poll::Ready(CompletionMeta {
                result: Ok(n),
                flags: 0,
            }),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                ref_mut.clear_readiness(direction.mask());
                ref_mut.set_waker(cx, direction);
                Poll::Pending
            }
            Err(e) => Poll::Ready(CompletionMeta {
                result: Err(e),
                flags: 0,
            }),
        }
    }

    pub(crate) fn cancel_op(
        this: &Rc<UnsafeCell<LegacyInner>>,
        token: usize,
        direction: ready::Direction,
    ) {
        let inner = unsafe { &mut *this.get() };
        let ready = match direction {
            ready::Direction::Read => Ready::READ_CANCELED,
            ready::Direction::Write => Ready::WRITE_CANCELED,
            ready::Direction::ReadOrWrite => Ready::CANCELED,
        };
        LegacyPoll::dispatch(&mut inner.poller.io_dispatch, token, ready);
    }

    pub(crate) fn submit_with_data<T>(
        this: &Rc<UnsafeCell<LegacyInner>>,
        data: T,
    ) -> io::Result<Op<T>>
    where
        T: OpAble,
    {
        Ok(Op {
            driver: Inner::Legacy(this.clone()),
            // useless for legacy
            index: 0,
            data: Some(data),
        })
    }

    #[cfg(feature = "sync")]
    pub(crate) fn unpark(this: &Rc<UnsafeCell<LegacyInner>>) -> waker::UnparkHandle {
        let inner = unsafe { &*this.get() };
        let weak = std::sync::Arc::downgrade(&inner.shared_waker);
        waker::UnparkHandle(weak)
    }
}

impl Driver for LegacyDriver {
    fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        let inner = Inner::Legacy(self.inner.clone());
        CURRENT.set(&inner, f)
    }

    fn submit(&self) -> io::Result<()> {
        // wait with timeout = 0
        self.park_timeout(Duration::ZERO)
    }

    fn park(&self) -> io::Result<()> {
        self.inner_park(None)
    }

    fn park_timeout(&self, duration: Duration) -> io::Result<()> {
        self.inner_park(Some(duration))
    }

    #[cfg(feature = "sync")]
    type Unpark = waker::UnparkHandle;

    #[cfg(feature = "sync")]
    fn unpark(&self) -> Self::Unpark {
        LegacyInner::unpark(&self.inner)
    }
}

impl Drop for LegacyDriver {
    fn drop(&mut self) {
        // Deregister thread id
        #[cfg(feature = "sync")]
        {
            use crate::driver::thread::{unregister_unpark_handle, unregister_waker_sender};
            unregister_unpark_handle(self.thread_id);
            unregister_waker_sender(self.thread_id);
        }
    }
}
