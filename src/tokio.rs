use std::{
    io,
    ops::Deref,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, TcpStream, ToSocketAddrs},
    runtime, select,
    time::sleep,
};

use crate::{Internal, PersistentTcpStream, CONNECT_TIMEOUT, CONN_POLL_PERIOD};

impl PersistentTcpStream<TcpStream> {
    pub async fn connect<A: ToSocketAddrs>(addrs: A) -> io::Result<Self> {
        let shared = Arc::new(Internal {
            addrs: lookup_host(addrs).await?.collect(),
            stream: Mutex::new(None),
            up: AtomicBool::new(true),
        });
        runtime::Handle::current().spawn(shared.clone().connect_loop());
        Ok(Self { shared })
    }
}

impl Internal<TcpStream> {
    async fn connect_loop(self: Arc<Self>) {
        while self.up.load(Ordering::Acquire) {
            if self.stream.lock().unwrap().is_none() {
                if !self.up.load(Ordering::Acquire) {
                    break;
                }
                match select! {
                    biased;
                    result = TcpStream::connect(self.addrs.deref()) => result,
                    () = sleep(CONNECT_TIMEOUT) => {
                        log::error!("Connecting to {:?} timed out", self.addrs.deref());
                        continue;
                    }
                } {
                    Ok(stream) => assert!(self.stream.lock().unwrap().replace(stream).is_none()),
                    Err(err) => {
                        log::error!("Error connecting to {:?}: {}", self.addrs.deref(), err)
                    }
                }
            }
            sleep(CONN_POLL_PERIOD).await;
        }
    }
}

impl AsyncRead for PersistentTcpStream<TcpStream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => {
                let len = buf.filled().len();
                match Pin::new(&mut stream).poll_read(cx, buf) {
                    Poll::Ready(result) => Poll::Ready(match result {
                        Ok(()) => {
                            if buf.filled().len() > len {
                                guard.replace(stream);
                                Ok(())
                            } else {
                                Err(io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    "Remote host closed connection",
                                ))
                            }
                        }
                        Err(err) => Err(err),
                    }),
                    Poll::Pending => {
                        guard.replace(stream);
                        Poll::Pending
                    }
                }
            }
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No connection established at the time",
            ))),
        }
    }
}

impl AsyncWrite for PersistentTcpStream<TcpStream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_write(cx, buf) {
                Poll::Ready(result) => Poll::Ready(match result {
                    Ok(len) => {
                        if len > 0 {
                            guard.replace(stream);
                            Ok(len)
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "Remote host closed connection",
                            ))
                        }
                    }
                    Err(err) => Err(err),
                }),
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No connection established at the time",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_flush(cx) {
                Poll::Ready(result) => Poll::Ready(match result {
                    Ok(()) => {
                        guard.replace(stream);
                        Ok(())
                    }
                    Err(err) => Err(err),
                }),
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No connection established at the time",
            ))),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.shared.stream.lock().unwrap();
        match guard.take() {
            Some(mut stream) => match Pin::new(&mut stream).poll_shutdown(cx) {
                Poll::Ready(result) => {
                    self.shared.up.store(false, Ordering::Release);
                    Poll::Ready(match result {
                        Ok(()) => Ok(()),
                        Err(err) => {
                            log::error!("Shutdown failed: {}", err);
                            Err(err)
                        }
                    })
                }
                Poll::Pending => {
                    guard.replace(stream);
                    Poll::Pending
                }
            },
            None => {
                self.shared.up.store(false, Ordering::Release);
                Poll::Ready(Ok(()))
            }
        }
    }
}
