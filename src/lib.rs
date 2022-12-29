use futures::task::AtomicWaker;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[cfg(feature = "async-std")]
mod async_std;
#[cfg(feature = "tokio")]
mod tokio;

const CONN_POLL_PERIOD: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(4000);

struct Internal<S> {
    addrs: Vec<SocketAddr>,
    stream: Mutex<Option<S>>,
    waker: AtomicWaker,
    down: AtomicBool,
    recount: AtomicUsize,
}

/// Persistense wrapper for TCP stream.
///
/// Performs subsequent reconnect attemps when socket is disconnected or not yet connected.
///
/// *It does not guarantee that all data sent to the server will arrive.*
/// *To ensure that you need to use some higher-level protocol.*
#[derive(Clone)]
pub struct Persistent<S> {
    shared: Arc<Internal<S>>,
}

impl<S> Internal<S> {
    fn is_down(&self) -> bool {
        self.down.load(Ordering::Acquire)
    }

    /// Number of successfull connection attempts.
    ///
    /// Change of this value may signal about potential data loss.
    pub fn connect_count(&self) -> usize {
        self.recount.load(Ordering::Acquire)
    }
}
