use futures::task::AtomicWaker;
use std::{
    io,
    ops::Deref,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, TcpStream, ToSocketAddrs},
    runtime, select,
    time::sleep,
};

use crate::{Internal, Persistent, CONNECT_TIMEOUT, CONN_POLL_PERIOD};

impl Persistent<TcpStream> {
    pub async fn connect<A: ToSocketAddrs>(addrs: A) -> io::Result<Self> {
        let shared = Arc::new(Internal {
            addrs: lookup_host(addrs).await?.collect(),
            stream: Mutex::new(None),
            waker: AtomicWaker::new(),
            down: AtomicBool::new(false),
            recount: AtomicUsize::new(0),
        });
        runtime::Handle::current().spawn(Internal::connect_loop(Arc::downgrade(&shared)));
        Ok(Self { shared })
    }
}

impl Internal<TcpStream> {
    async fn connect_once(&self) -> io::Result<TcpStream> {
        TcpStream::connect(self.addrs.deref()).await
    }

    async fn connect_loop(weak: Weak<Self>) {
        loop {
            log::trace!("Connect loop");
            let this = match weak.upgrade() {
                Some(strong) => strong,
                None => break,
            };
            if this.is_down() {
                break;
            }
            if this.stream.lock().unwrap().is_none() {
                if this.is_down() {
                    break;
                }
                match select! {
                    biased;
                    result = this.connect_once() => result,
                    () = sleep(CONNECT_TIMEOUT) => {
                        log::warn!("Connecting to {:?} timed out", this.addrs.deref());
                        continue;
                    }
                } {
                    Ok(stream) => {
                        assert!(this.stream.lock().unwrap().replace(stream).is_none());
                        this.waker.wake();
                        log::info!("Socket connected");
                    }
                    Err(err) => {
                        log::warn!("Error connecting to {:?}: {}", this.addrs.deref(), err)
                    }
                }
            }
            sleep(CONN_POLL_PERIOD).await;
        }
        log::info!("Connect loop stopped");
    }
}

impl AsyncRead for Persistent<TcpStream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        log::trace!("Socket read");
        self.shared.waker.register(cx.waker());
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => {
                let len = buf.filled().len();
                match Pin::new(&mut stream).poll_read(cx, buf) {
                    Poll::Ready(result) => match result.and_then(|()| {
                        if buf.filled().len() > len {
                            Ok(())
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "Remote host closed connection",
                            ))
                        }
                    }) {
                        Ok(()) => {
                            guard.replace(stream);
                            Poll::Ready(Ok(()))
                        }
                        Err(err) => {
                            log::warn!("Socket read error: {}", err);
                            Poll::Pending
                        }
                    },
                    Poll::Pending => {
                        guard.replace(stream);
                        Poll::Pending
                    }
                }
            }
            None => {
                log::warn!("No connection established at the time");
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for Persistent<TcpStream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        log::trace!("Socket write");
        self.shared.waker.register(cx.waker());
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_write(cx, buf) {
                Poll::Ready(result) => match result.and_then(|len| {
                    if len > 0 {
                        Ok(len)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "Remote host closed connection",
                        ))
                    }
                }) {
                    Ok(len) => {
                        guard.replace(stream);
                        Poll::Ready(Ok(len))
                    }
                    Err(err) => {
                        log::warn!("Socket write error: {}", err);
                        Poll::Pending
                    }
                },
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => {
                log::warn!("No connection established at the time");
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        log::trace!("Socket flush");
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_flush(cx) {
                Poll::Ready(result) => Poll::Ready(match result {
                    Ok(()) => {
                        guard.replace(stream);
                        Ok(())
                    }
                    Err(err) => {
                        log::warn!("Socket flush error: {}", err);
                        Ok(())
                    }
                }),
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => {
                log::warn!("No connection established at the time");
                Poll::Ready(Ok(()))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        log::trace!("Socket shutdown");
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_shutdown(cx) {
                Poll::Ready(result) => {
                    self.shared.down.store(true, Ordering::Release);
                    Poll::Ready(match result {
                        Ok(()) => Ok(()),
                        Err(err) => {
                            log::error!("Shutdown failed: {}", err);
                            Ok(())
                        }
                    })
                }
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => {
                self.shared.down.store(true, Ordering::Release);
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(all(test, feature = "test"))]
mod tests {
    use crate::Persistent;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        runtime, test as async_test, try_join,
    };

    #[async_test]
    async fn ping_pong() {
        let _ = env_logger::builder().is_test(true).try_init();

        const ADDR: &str = "127.0.0.1:57249";
        const ATTEMPTS: u32 = 64;

        let rt = runtime::Handle::current();
        let server = rt.spawn(async {
            let listener = TcpListener::bind(ADDR).await.unwrap();
            for i in 0..ATTEMPTS {
                let (mut server, _) = listener.accept().await.unwrap();
                server.write_u32(i).await.unwrap();
                server.flush().await.unwrap();
                assert_eq!(server.read_u32().await.unwrap(), ATTEMPTS - i);
                if i % 2 == 0 {
                    server.shutdown().await.unwrap();
                }
            }
        });
        let client = rt.spawn(async {
            let mut client = Persistent::<TcpStream>::connect(ADDR).await.unwrap();
            for i in 0..ATTEMPTS {
                assert_eq!(client.read_u32().await.unwrap(), i);
                client.write_u32(ATTEMPTS - i).await.unwrap();
                client.flush().await.unwrap();
            }
            client.shutdown().await.unwrap();
        });
        try_join!(server, client).unwrap();
    }
}
