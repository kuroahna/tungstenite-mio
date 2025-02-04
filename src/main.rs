mod mio_channel;

use mio::event::Source;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;
use tungstenite::protocol::Role;
use tungstenite::WebSocket;

const SERVER: Token = Token(0);
const BROADCAST: Token = Token(SERVER.0 + 1);

trait TokenExt {
    fn next(&self) -> Self;
}

impl TokenExt for Token {
    fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

trait Stream: Read + Write + Source {}

impl Stream for TcpStream {}

struct EmptyStream;

impl Stream for EmptyStream {}

impl Read for EmptyStream {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for EmptyStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Source for EmptyStream {
    fn register(&mut self, _: &mio::Registry, _: Token, _: Interest) -> std::io::Result<()> {
        Ok(())
    }

    fn reregister(&mut self, _: &mio::Registry, _: Token, _: Interest) -> std::io::Result<()> {
        Ok(())
    }

    fn deregister(&mut self, _: &mio::Registry) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
enum WebSocketError {
    Io(std::io::Error),
    Handshake(
        tungstenite::HandshakeError<
            tungstenite::ServerHandshake<
                Box<dyn Stream>,
                tungstenite::handshake::server::NoCallback,
            >,
        >,
    ),
    WebSocket(tungstenite::Error),
}

impl Display for WebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebSocketError::Io(error) => write!(f, "IO error: {}", error),
            WebSocketError::Handshake(error) => write!(f, "handshake error: {}", error),
            WebSocketError::WebSocket(error) => write!(f, "websocket error: {}", error),
        }
    }
}

impl Error for WebSocketError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            WebSocketError::Io(error) => Some(error),
            WebSocketError::Handshake(error) => Some(error),
            WebSocketError::WebSocket(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for WebSocketError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl
    From<
        tungstenite::HandshakeError<
            tungstenite::ServerHandshake<
                Box<dyn Stream>,
                tungstenite::handshake::server::NoCallback,
            >,
        >,
    > for WebSocketError
{
    fn from(
        value: tungstenite::HandshakeError<
            tungstenite::ServerHandshake<
                Box<dyn Stream>,
                tungstenite::handshake::server::NoCallback,
            >,
        >,
    ) -> Self {
        Self::Handshake(value)
    }
}

impl From<tungstenite::Error> for WebSocketError {
    fn from(value: tungstenite::Error) -> Self {
        Self::WebSocket(value)
    }
}

enum WebSocketMessage {
    UpgradeWebSocket(Box<dyn Stream>),
    Poll,
    SendText(Rc<str>),
}

enum WebSocketState {
    Unconnected(UnconnectedState),
    Connected(ConnectedState),
    Closed(WebSocket<Box<dyn Stream>>),
}

impl WebSocketState {
    fn next_state(&mut self, message: WebSocketMessage) -> Result<(), WebSocketError> {
        match self {
            WebSocketState::Unconnected(state) => *self = state.next_state(message)?,
            WebSocketState::Connected(state) => {
                if let Some(state) = state.next_state(message)? {
                    *self = state;
                }
            }
            WebSocketState::Closed(_) => panic!("WebSocket is already closed"),
        }

        Ok(())
    }
}

struct UnconnectedState;

impl UnconnectedState {
    fn next_state(&mut self, message: WebSocketMessage) -> Result<WebSocketState, WebSocketError> {
        match message {
            WebSocketMessage::UpgradeWebSocket(stream) => Ok(WebSocketState::Connected(
                ConnectedState(tungstenite::accept(stream)?),
            )),
            WebSocketMessage::Poll => panic!("polled on an unconnected WebSocket"),
            WebSocketMessage::SendText(_) => panic!("text sent on an unconnected WebSocket"),
        }
    }
}

struct ConnectedState(WebSocket<Box<dyn Stream>>);

impl ConnectedState {
    fn next_state(
        &mut self,
        message: WebSocketMessage,
    ) -> Result<Option<WebSocketState>, WebSocketError> {
        match message {
            WebSocketMessage::UpgradeWebSocket(_) => {
                panic!("connection is already upgraded to a WebSocket")
            }
            WebSocketMessage::Poll => loop {
                let msg = match self.0.read() {
                    Ok(msg) => msg,
                    Err(e) => match e {
                        tungstenite::Error::Io(ref error) => match error.kind() {
                            io::ErrorKind::WouldBlock => return Ok(None),
                            io::ErrorKind::Interrupted => continue,
                            _ => return Err(From::from(e)),
                        },
                        _ => return Err(From::from(e)),
                    },
                };
                dbg!("{}", &msg);

                if let tungstenite::Message::Close(close) = msg {
                    // Even if the client failed to handle our close response,
                    // we'll simply just close the connection on our end
                    let _ = self.0.close(close).or_else(|e| match e {
                        tungstenite::Error::ConnectionClosed => Ok(()),
                        _ => Err(e),
                    });

                    let state = std::mem::replace(
                        self,
                        ConnectedState(WebSocket::from_raw_socket(
                            Box::new(EmptyStream),
                            Role::Server,
                            None,
                        )),
                    );
                    return Ok(Some(WebSocketState::Closed(state.0)));
                }
            },
            WebSocketMessage::SendText(message) => {
                self.0.send(tungstenite::Message::Text((*message).into()))?;
                Ok(None)
            }
        }
    }
}

fn main() {
    let (tx, mut rx) = mio_channel::sync_channel::<String>(10);
    let mut poll =
        Poll::new().unwrap_or_else(|e| panic!("failed to create poll instance: {:?}", e));
    let mut events = Events::with_capacity(128);

    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 6677);
    let mut server = TcpListener::bind(address)
        .unwrap_or_else(|e| panic!("failed to bind address `{}`: {:?}", address, e));

    poll.registry()
        .register(&mut server, SERVER, Interest::READABLE)
        .unwrap_or_else(|e| panic!("failed to register server to poll instance: {:?}", e));
    poll.registry()
        .register(&mut rx, BROADCAST, Interest::READABLE)
        .unwrap_or_else(|e| {
            panic!(
                "failed to register broadcast channel to poll instance: {:?}",
                e
            )
        });

    std::thread::spawn(move || {
        let mut token_to_tcpstreams = HashMap::new();
        let mut token_to_websockets: HashMap<Token, WebSocketState> = HashMap::new();
        let mut unique_token = Token(BROADCAST.0 + 1);

        loop {
            if let Err(e) = poll.poll(&mut events, None) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("failed to poll for events: {:?}", e);
            }

            for event in events.iter() {
                match event.token() {
                    SERVER => loop {
                        let (mut stream, address) = match server.accept() {
                            Ok((stream, address)) => (stream, address),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                break;
                            }
                            Err(e) => {
                                panic!("failed to accept connection: {:?}", e);
                            }
                        };

                        println!("Accepted connection from: `{}`", address);

                        unique_token = unique_token.next();
                        poll.registry()
                            .register(
                                &mut stream,
                                unique_token,
                                Interest::READABLE.add(Interest::WRITABLE),
                            )
                            .unwrap_or_else(|e| {
                                panic!(
                                    "failed to register incoming connection `{}` for events: {:?}",
                                    address, e
                                )
                            });

                        token_to_tcpstreams.insert(unique_token, stream);
                    },
                    BROADCAST => {
                        if let Ok(msg) = rx.try_recv() {
                            let msg: Rc<str> = msg.into();
                            for state in token_to_websockets.values_mut() {
                                state
                                    .next_state(WebSocketMessage::SendText(msg.clone()))
                                    .unwrap_or_else(|e| {
                                        panic!(
                                            "failed to send text `{}` to WebSocket: {:?}",
                                            msg, e
                                        )
                                    });
                            }
                        }
                    }
                    token => {
                        if event.is_readable() && event.is_writable() {
                            match token_to_tcpstreams.remove(&token) {
                                Some(stream) => {
                                    let mut state = WebSocketState::Unconnected(UnconnectedState);
                                    state
                                        .next_state(WebSocketMessage::UpgradeWebSocket(Box::new(
                                            stream,
                                        )))
                                        .unwrap_or_else(|e| {
                                            panic!(
                                                "failed to upgrade tcp stream to WebSocket: {:?}",
                                                e
                                            )
                                        });
                                    token_to_websockets.insert(token, state);
                                }
                                None => {
                                    let state = token_to_websockets
                                        .get_mut(&token)
                                        .expect("tcp stream should be upgraded to a WebSocket");
                                    state
                                        .next_state(WebSocketMessage::Poll)
                                        .unwrap_or_else(|e| {
                                            panic!("failed to poll WebSocket: {:?}", e)
                                        });
                                    if let WebSocketState::Closed(stream) = state {
                                        poll.registry()
                                            .deregister(stream.get_mut())
                                            .unwrap_or_else(|e| {
                                                panic!("failed to deregister stream: {:?}", e)
                                            });
                                        token_to_websockets
                                            .remove(&token)
                                            .expect("WebSocket should not have been removed yet");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    loop {
        std::thread::sleep(Duration::from_millis(500));
        tx.send("Hello world".to_owned())
            .expect("failed to broadcast message");
    }
}
