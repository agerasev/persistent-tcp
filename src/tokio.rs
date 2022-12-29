use futures::{pin_mut, ready};
use std::{
    fmt::Debug,
    future::Future,
    io,
    mem::replace,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpStream, ToSocketAddrs},
    select,
    time::sleep,
};

use crate::{Persistent, Socket, State, CONNECT_TIMEOUT, CONN_POLL_PERIOD};

impl Socket for TcpStream {}

async fn try_connect<A: ToSocketAddrs>(addrs: A) -> io::Result<TcpStream> {
    select! {
        biased;
        result = TcpStream::connect(addrs) => result,
        () = sleep(CONNECT_TIMEOUT) => {
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
    pub fn new<A: ToSocketAddrs + Clone + Debug + Send + 'static>(addrs: A) -> Self {
        Self {
            connect: Box::new(move || Box::pin(connect(addrs.clone()))),
            state: State::Empty,
        }
    }
}

impl AsyncRead for Persistent<TcpStream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        log::trace!("Socket read");

        let connected = self.connected();
        pin_mut!(connected);
        ready!(connected.poll(cx));

        let stream = if let State::Ready(socket) = &mut self.state {
            socket
        } else {
            unreachable!()
        };

        let len = buf.filled().len();
        let result = ready!(Pin::new(stream).poll_read(cx, buf)).and_then(|()| {
            if buf.filled().len() > len {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Remote host closed connection",
                ))
            }
        });
        match result {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => {
                self.state = State::Empty;
                Poll::Ready(Err(err))
            }
        }
    }
}

impl AsyncWrite for Persistent<TcpStream> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        log::trace!("Socket write");

        let connected = self.connected();
        pin_mut!(connected);
        ready!(connected.poll(cx));

        let stream = if let State::Ready(socket) = &mut self.state {
            socket
        } else {
            unreachable!()
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

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        log::trace!("Socket shutdown");
        if let State::Ready(mut stream) = replace(&mut self.state, State::Empty) {
            match ready!(Pin::new(&mut stream).poll_shutdown(cx)) {
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
    use std::io::ErrorKind;
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
            listener.accept().await.unwrap().0.shutdown().await.unwrap();
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
            let mut client = Persistent::<TcpStream>::new(ADDR);
            for i in 0..ATTEMPTS {
                assert_eq!(
                    client.read_u32().await.err().unwrap().kind(),
                    ErrorKind::ConnectionAborted
                );
                assert_eq!(client.read_u32().await.unwrap(), i);
                client.write_u32(ATTEMPTS - i).await.unwrap();
                client.flush().await.unwrap();
            }
            client.shutdown().await.unwrap();
        });
        try_join!(server, client).unwrap();
    }
}
