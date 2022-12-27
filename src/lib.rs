use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};

#[cfg(feature = "tokio")]
mod tokio;

const CONN_POLL_PERIOD: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

struct Internal<S: Sized> {
    addrs: Vec<SocketAddr>,
    stream: Mutex<Option<S>>,
    up: AtomicBool,
}

#[derive(Clone)]
pub struct PersistentTcpStream<S: Sized> {
    shared: Arc<Internal<S>>,
}
