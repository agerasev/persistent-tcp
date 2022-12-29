use async_std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    task::sleep,
};
use futures::{pin_mut, ready, select_biased, Future, FutureExt};
use std::{
    fmt::Debug,
    io,
    mem::replace,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{Persistent, Socket, State, CONNECT_TIMEOUT, CONN_POLL_PERIOD};

impl Socket for TcpStream {}

async fn try_connect<A: ToSocketAddrs>(addrs: A) -> io::Result<TcpStream> {
    select_biased! {
        result = TcpStream::connect(addrs).fuse() => result,
        () = sleep(CONNECT_TIMEOUT).fuse() => {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Connect timed out",
            ))
        }
    }
}

async fn connect<A: ToSocketAddrs + Clone + Debug + Send>(addrs: A) -> TcpStream {
    loop {
        log::trace!("Connect loop");
        let addrs_ = addrs.clone();
        match try_connect(addrs_).await {
            Ok(stream) => {
                log::debug!("Socket connected");
                break stream;
            }
            Err(err) => {
                log::warn!("Error connecting to {:?}: {}", addrs, err)
            }
        }
        sleep(CONN_POLL_PERIOD).await;
    }
}

impl Persistent<TcpStream> {
    /// Create persistent socket without waiting for connection.
    pub fn new<A: ToSocketAddrs + Clone + Debug + Send + Sync + 'static>(addrs: A) -> Self
    where
        A::Iter: Send,
    {
        Self {
            connect: Box::new(move || Box::pin(connect(addrs.clone()))),
            state: State::Empty,
        }
    }
}

impl Read for Persistent<TcpStream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        log::trace!("Socket read");

        let stream = {
            let future = self.connected_socket();
            pin_mut!(future);
            ready!(future.poll(cx))
        };

        let result = ready!(Pin::new(stream).poll_read(cx, buf)).and_then(|len| {
            if len > 0 {
                Ok(len)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Remote host closed connection",
                ))
            }
        });
        match result {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(err) => {
                self.state = State::Empty;
                Poll::Ready(Err(err))
            }
        }
    }
}

impl Write for Persistent<TcpStream> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        log::trace!("Socket write");

        let stream = {
            let future = self.connected_socket();
            pin_mut!(future);
            ready!(future.poll(cx))
        };

        let result = ready!(Pin::new(stream).poll_write(cx, buf)).and_then(|len| {
            if len > 0 {
                Ok(len)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Remote host closed connection",
                ))
            }
        });
        match result {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(err) => {
                self.state = State::Empty;
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        log::trace!("Socket flush");
        if let State::Ready(mut stream) = replace(&mut self.state, State::Empty) {
            match ready!(Pin::new(&mut stream).poll_flush(cx)) {
                Ok(()) => {
                    self.state = State::Ready(stream);
                    Poll::Ready(Ok(()))
                }
                Err(err) => Poll::Ready(Err(err)),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        log::trace!("Socket close");
        if let State::Ready(mut stream) = replace(&mut self.state, State::Empty) {
            match ready!(Pin::new(&mut stream).poll_close(cx)) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(err) => Poll::Ready(Err(err)),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(all(test, feature = "test"))]
mod tests {
    use crate::Persistent;
    use async_std::{
        io::ReadExt,
        net::{TcpListener, TcpStream},
        task::spawn,
        test as async_test,
    };
    use futures::{join, AsyncWriteExt};
    use std::io::ErrorKind;

    #[async_test]
    async fn ping_pong() {
        let _ = env_logger::builder().is_test(true).try_init();

        const ADDR: &str = "127.0.0.1:57251";
        const ATTEMPTS: u32 = 256;

        let server = spawn(async {
            let listener = TcpListener::bind(ADDR).await.unwrap();
            listener.accept().await.unwrap().0.close().await.unwrap();
            for i in 0..ATTEMPTS {
                let (mut server, _) = listener.accept().await.unwrap();
                server.write_all(&i.to_be_bytes()).await.unwrap();
                server.flush().await.unwrap();
                let mut b = [0; 4];
                server.read_exact(&mut b).await.unwrap();
                assert_eq!(u32::from_be_bytes(b), ATTEMPTS - i);
                if i % 2 == 0 {
                    server.close().await.unwrap();
                }
            }
        });
        let client = spawn(async {
            let mut client = Persistent::<TcpStream>::new(ADDR);
            for i in 0..ATTEMPTS {
                let mut b = [0; 4];
                assert_eq!(
                    client.read_exact(&mut b).await.err().unwrap().kind(),
                    ErrorKind::ConnectionAborted
                );
                client.read_exact(&mut b).await.unwrap();
                assert_eq!(u32::from_be_bytes(b), i);
                client
                    .write_all(&(ATTEMPTS - i).to_be_bytes())
                    .await
                    .unwrap();
                client.flush().await.unwrap();
            }
            client.close().await.unwrap();
        });
        join!(server, client);
    }
}
