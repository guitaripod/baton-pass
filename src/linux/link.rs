use crate::crypto::{Cipher, Counter, random_u64};
use crate::proto::{Envelope, Msg};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const HELLO_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

/// The encrypted UDP session with the Mac: handshake, replay protection and liveness.
pub struct Link {
    socket: UdpSocket,
    cipher: Rc<Cipher>,
    peers: Vec<String>,
    counter: Counter,
    hello_nonce: u64,
    last_hello: Option<Instant>,
    next_peer: usize,
    pub peer: Option<SocketAddr>,
    pub token: Option<u64>,
    peer_counter: u64,
    pub last_heard: Instant,
    pub flags: u8,
}

pub enum Incoming {
    Connected,
    Message(Msg),
}

impl Link {
    pub async fn new(cipher: Rc<Cipher>, peers: Vec<String>) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            socket,
            cipher,
            peers,
            counter: Counter::new(),
            hello_nonce: random_u64(),
            last_hello: None,
            next_peer: 0,
            peer: None,
            token: None,
            peer_counter: 0,
            last_heard: Instant::now(),
            flags: 0,
        })
    }

    pub fn cipher(&self) -> Rc<Cipher> {
        self.cipher.clone()
    }

    pub fn connected(&self) -> bool {
        self.token.is_some()
    }

    pub fn disconnect(&mut self, reason: &str) {
        if self.token.take().is_some() {
            tracing::warn!("lost the Mac: {reason}");
        }
        self.flags = 0;
        self.next_peer = 0;
        self.last_hello = None;
    }

    /// Greets one configured peer per interval, in order, so the wired address wins whenever it
    /// answers and the tailnet name is only a fallback; drops a session that went silent.
    pub async fn maintain(&mut self) {
        if self.connected() && self.last_heard.elapsed() > IDLE_TIMEOUT {
            self.disconnect("no reply for 3 s");
        }
        if self.connected() || self.last_hello.is_some_and(|sent| sent.elapsed() < HELLO_INTERVAL) {
            return;
        }
        self.last_hello = Some(Instant::now());
        self.hello_nonce = random_u64();
        if self.peers.is_empty() {
            return;
        }
        let peer = self.peers[self.next_peer % self.peers.len()].clone();
        self.next_peer = (self.next_peer + 1) % self.peers.len();
        let packet = self.seal(0, Msg::Hello { nonce: self.hello_nonce });
        match tokio::net::lookup_host(&peer).await {
            Ok(mut addresses) => {
                if let Some(address) = addresses.find(SocketAddr::is_ipv4)
                    && let Err(error) = self.socket.send_to(&packet, address).await {
                        tracing::debug!("hello to {address}: {error}");
                    }
            }
            Err(error) => tracing::debug!("resolving {peer}: {error}"),
        }
    }

    pub async fn send(&mut self, msg: Msg) {
        let (Some(token), Some(peer)) = (self.token, self.peer) else { return };
        let packet = self.seal(token, msg);
        if let Err(error) = self.socket.send_to(&packet, peer).await {
            tracing::debug!("send to {peer}: {error}");
        }
    }

    fn seal(&mut self, token: u64, msg: Msg) -> Vec<u8> {
        let counter = self.counter.next();
        self.cipher.seal(&Envelope { token, counter, msg }.encode())
    }

    pub async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buffer).await
    }

    /// Authenticates a datagram and applies the session rules; returns what the daemon must act on.
    pub fn accept(&mut self, packet: &[u8], from: SocketAddr) -> Option<Incoming> {
        let envelope = Envelope::decode(&self.cipher.open(packet)?)?;
        match envelope.msg {
            Msg::Welcome { nonce, token } if nonce == self.hello_nonce && !self.connected() => {
                self.token = Some(token);
                self.peer = Some(from);
                self.peer_counter = envelope.counter;
                self.last_heard = Instant::now();
                tracing::info!("linked with the Mac at {from}");
                Some(Incoming::Connected)
            }
            Msg::Reset if Some(from) == self.peer && self.connected() => {
                self.disconnect("the Mac restarted its session");
                None
            }
            msg => {
                if Some(envelope.token) != self.token || Some(from) != self.peer {
                    return None;
                }
                if envelope.counter <= self.peer_counter {
                    return None;
                }
                self.peer_counter = envelope.counter;
                self.last_heard = Instant::now();
                Some(Incoming::Message(msg))
            }
        }
    }
}
