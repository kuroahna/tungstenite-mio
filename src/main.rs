mod mio_channel;

use mio::event::Source;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::Display;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
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
            WebSocketError::WebSocket(error) => write!(f, "WebSocket error: {}", error),
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
    MessagesAvailable,
    CanWrite,
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
            WebSocketMessage::UpgradeWebSocket(stream) => {
                Ok(WebSocketState::Connected(ConnectedState {
                    websocket: tungstenite::accept(stream)?,
                    messages: VecDeque::new(),
                    write: WriteState::Unwritable,
                }))
            }
            WebSocketMessage::MessagesAvailable => {
                panic!("messages available on an unconnected WebSocket")
            }
            WebSocketMessage::CanWrite => panic!("writable event on an unconnected WebSocket"),
            WebSocketMessage::SendText(_) => panic!("text sent on an unconnected WebSocket"),
        }
    }
}

enum WriteState {
    Unwritable,
    Writable,
}

struct ConnectedState {
    websocket: WebSocket<Box<dyn Stream>>,
    messages: VecDeque<Rc<str>>,
    write: WriteState,
}

impl ConnectedState {
    fn next_state(
        &mut self,
        message: WebSocketMessage,
    ) -> Result<Option<WebSocketState>, WebSocketError> {
        match message {
            WebSocketMessage::UpgradeWebSocket(_) => {
                panic!("connection is already upgraded to a WebSocket")
            }
            WebSocketMessage::MessagesAvailable => loop {
                let msg = match self.websocket.read() {
                    Ok(msg) => msg,
                    Err(e) => match e {
                        tungstenite::Error::ConnectionClosed => {
                            let state = std::mem::replace(
                                self,
                                ConnectedState {
                                    websocket: WebSocket::from_raw_socket(
                                        Box::new(EmptyStream),
                                        Role::Server,
                                        None,
                                    ),
                                    messages: VecDeque::new(),
                                    write: WriteState::Unwritable,
                                },
                            );
                            return Ok(Some(WebSocketState::Closed(state.websocket)));
                        }
                        tungstenite::Error::Io(ref error) => match error.kind() {
                            io::ErrorKind::WouldBlock => return Ok(None),
                            io::ErrorKind::Interrupted => continue,
                            _ => return Err(From::from(e)),
                        },
                        _ => return Err(From::from(e)),
                    },
                };
                dbg!("{}", &msg);
            },
            WebSocketMessage::CanWrite => {
                if let WriteState::Unwritable = self.write {
                    self.write = WriteState::Writable;
                }

                // On write events, we send one message at a time because mio
                // will send another write event after each successful flush.
                // This will allow us to drain our internal buffer if there are
                // any remaining messages left. It also allows the caller to
                // respond to each message since it is done one at a time
                self.send_message()
            }
            WebSocketMessage::SendText(message) => {
                self.messages.push_back(message);

                if let WriteState::Unwritable = self.write {
                    return Ok(None);
                }

                self.send_message()
            }
        }
    }

    fn send_message(&mut self) -> Result<Option<WebSocketState>, WebSocketError> {
        if let Some(msg) = self.messages.pop_front() {
            if let Err(e) = self
                .websocket
                .send(tungstenite::Message::Text((*msg).into()))
            {
                match e {
                    tungstenite::Error::ConnectionClosed => {
                        let state = std::mem::replace(
                            self,
                            ConnectedState {
                                websocket: WebSocket::from_raw_socket(
                                    Box::new(EmptyStream),
                                    Role::Server,
                                    None,
                                ),
                                messages: VecDeque::new(),
                                write: WriteState::Unwritable,
                            },
                        );
                        return Ok(Some(WebSocketState::Closed(state.websocket)));
                    }
                    tungstenite::Error::Io(ref err) => match err.kind() {
                        // On write error, tungstenite will store the frame in
                        // its internal buffer and send it on a subsequent call
                        // to write or flush. Hence, we do not need to push the
                        // message back into our buffer here
                        io::ErrorKind::WouldBlock => self.write = WriteState::Unwritable,
                        io::ErrorKind::Interrupted => {}
                        _ => return Err(From::from(e)),
                    },
                    _ => return Err(From::from(e)),
                }
            }
        }

        Ok(None)
    }
}

fn main() {
    let (tx, mut rx) = mio_channel::sync_channel::<String>(10);
    let mut poll =
        Poll::new().unwrap_or_else(|e| panic!("failed to create poll instance: {:?}", e));
    let mut events = Events::with_capacity(128);

    let address = SocketAddr::from(([127, 0, 0, 1], 6677));
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
        let mut unique_token = Token(BROADCAST.0);

        loop {
            if let Err(e) = poll.poll(&mut events, None) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("failed to poll for events: {:?}", e);
            }

            for event in events.iter() {
                match event.token() {
                    SERVER => {
                        if !event.is_readable() {
                            continue;
                        }

                        loop {
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
                        }
                    }
                    BROADCAST => {
                        if !event.is_readable() {
                            continue;
                        }

                        if let Ok(msg) = rx.try_recv() {
                            let msg: Rc<str> = msg.into();
                            let mut closed_connection_tokens = Vec::new();
                            for (token, state) in token_to_websockets.iter_mut() {
                                state
                                    .next_state(WebSocketMessage::SendText(msg.clone()))
                                    .unwrap_or_else(|e| {
                                        panic!(
                                            "failed to send text `{}` to WebSocket: {:?}",
                                            msg, e
                                        )
                                    });
                                if let WebSocketState::Closed(_) = state {
                                    closed_connection_tokens.push(*token);
                                }
                            }

                            for token in closed_connection_tokens {
                                let state = token_to_websockets
                                    .remove(&token)
                                    .expect("WebSocket should not have been removed yet");
                                let WebSocketState::Closed(mut stream) = state else {
                                    panic!("all WebSocket connections should be closed");
                                };
                                poll.registry()
                                    .deregister(stream.get_mut())
                                    .unwrap_or_else(|e| {
                                        panic!("failed to deregister stream: {:?}", e)
                                    });
                            }
                        }
                    }
                    token => {
                        if event.is_readable() {
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
                                    // There is no guarantee that another readiness event will be
                                    // delivered until the readiness event has been drained
                                    state
                                        .next_state(WebSocketMessage::MessagesAvailable)
                                        .unwrap_or_else(|e| {
                                            panic!("failed to read messages on WebSocket: {:?}", e)
                                        });
                                    if let WebSocketState::Closed(mut stream) = state {
                                        poll.registry()
                                            .deregister(stream.get_mut())
                                            .unwrap_or_else(|e| {
                                                panic!("failed to deregister stream: {:?}", e)
                                            });
                                    } else {
                                        token_to_websockets.insert(token, state);
                                    }
                                }
                                None => {
                                    let state = token_to_websockets
                                        .get_mut(&token)
                                        .expect("tcp stream should be upgraded to a WebSocket");
                                    state
                                        .next_state(WebSocketMessage::MessagesAvailable)
                                        .unwrap_or_else(|e| {
                                            panic!("failed to read messages on WebSocket: {:?}", e)
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

                        if event.is_writable() {
                            if let Some(state) = token_to_websockets.get_mut(&token) {
                                state
                                    .next_state(WebSocketMessage::CanWrite)
                                    .unwrap_or_else(|e| {
                                        panic!(
                                            "failed to handle writable event on WebSocket: {:?}",
                                            e
                                        )
                                    });
                                if let WebSocketState::Closed(stream) = state {
                                    poll.registry().deregister(stream.get_mut()).unwrap_or_else(
                                        |e| panic!("failed to deregister stream: {:?}", e),
                                    );
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
    });

    loop {
        std::thread::sleep(Duration::from_millis(500));
        tx.send("Hello world".to_owned())
            .expect("failed to broadcast message");
    }
}
