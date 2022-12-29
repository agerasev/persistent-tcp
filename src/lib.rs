use futures::{Future, FutureExt};
use std::{
    mem::replace,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

#[cfg(feature = "async-std")]
mod async_std;
#[cfg(feature = "tokio")]
mod tokio;

const CONN_POLL_PERIOD: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(4000);

pub trait Socket: Send + Unpin + Sized {}

type Connect<S> = Pin<Box<dyn Future<Output = S> + Send>>;

enum State<S: Socket> {
    Empty,
    Connecting(Connect<S>),
    Ready(S),
}

/// Persistense wrapper for TCP stream.
///
/// Performs subsequent reconnect attemps when socket is disconnected or not yet connected.
///
/// *It does not guarantee that all data sent to the server will arrive.*
/// *To ensure that you need to use some higher-level protocol.*
pub struct Persistent<S: Socket> {
    connect: Box<dyn Fn() -> Connect<S> + Send>,
    state: State<S>,
}

impl<S: Socket> Unpin for Persistent<S> {}

impl<S: Socket> Persistent<S> {
    pub fn connected(&mut self) -> Connected<S> {
        Connected { owner: self }
    }
}

pub struct Connected<'a, S: Socket> {
    owner: &'a mut Persistent<S>,
}

impl<'a, S: Socket> Unpin for Connected<'a, S> {}

impl<'a, S: Socket> Future for Connected<'a, S> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut self.owner;
        let socket = {
            let state = replace(&mut this.state, State::Empty);
            if let State::Ready(socket) = state {
                socket
            } else {
                let mut connect = match state {
                    State::Ready(..) => unreachable!(),
                    State::Connecting(connect) => connect,
                    State::Empty => (this.connect)(),
                };
                match connect.poll_unpin(cx) {
                    Poll::Ready(socket) => socket,
                    Poll::Pending => {
                        this.state = State::Connecting(connect);
                        return Poll::Pending;
                    }
                }
            }
        };
        this.state = State::Ready(socket);
        Poll::Ready(())
    }
}
