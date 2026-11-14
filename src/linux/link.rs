use crate::crypto::{Cipher, Counter, random_u64};
use crate::proto::{Envelope, Msg};
use crate::session::Silence;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const HELLO_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const NONCE_MEMORY: usize = 4;
const SEND_WARNING_INTERVAL: Duration = Duration::from_secs(5);
const ROAM_INTERVAL: Duration = Duration::from_secs(5);

/// The encrypted UDP session with the Mac: handshake, replay protection and liveness.
pub struct Link {
    socket: UdpSocket,
    cipher: Rc<Cipher>,
    peers: Vec<String>,
    counter: Counter,
    nonces: VecDeque<u64>,
    joining: Option<(SocketAddr, u64)>,
    last_hello: Option<Instant>,
    next_peer: usize,
    pub peer: Option<SocketAddr>,
    pub token: Option<u64>,
    peer_counter: u64,
    session_floor: u64,
    pub last_heard: Instant,
    silence: Silence,
    resolved: Rc<RefCell<HashMap<String, SocketAddr>>>,
    last_send_warning: Option<Instant>,
    roam_allowed: bool,
    last_roam: Option<Instant>,
    sent_since_reply: u32,
    pub flags: u8,
}

pub enum Incoming {
    Connected,
    Message(Msg),
}

impl Link {
    pub async fn new(cipher: Rc<Cipher>, peers: Vec<String>) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        if let Err(error) = socket.set_tos_v4(crate::proto::LOW_DELAY) {
            tracing::warn!("marking packets low-delay: {error}");
        }
        Ok(Self {
            socket,
            cipher,
            peers,
            counter: Counter::new(),
            nonces: VecDeque::new(),
            joining: None,
            last_hello: None,
            next_peer: 0,
            peer: None,
            token: None,
            peer_counter: 0,
            session_floor: 0,
            last_heard: Instant::now(),
            silence: Silence::default(),
            resolved: Rc::new(RefCell::new(HashMap::new())),
            last_send_warning: None,
            roam_allowed: false,
            last_roam: None,
            sent_since_reply: 0,
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
        self.nonces.clear();
        self.joining = None;
    }

    /// Counts the time since the previous call toward the Mac's silence, at most `bound`, and
    /// returns the whole gap. Time this daemon's loop spent blocked is left out, so a stall here
    /// is never blamed on the Mac: the heartbeats it would have answered were never sent.
    pub fn observe(&mut self, now: Instant, bound: Duration) -> Duration {
        self.silence.observe(now, bound)
    }

    /// How long the Mac has been silent while this loop was running to hear it.
    pub fn quiet(&self) -> Duration {
        self.silence.quiet()
    }

    fn heard(&mut self) {
        self.last_heard = Instant::now();
        self.silence.heard();
        self.sent_since_reply = 0;
    }

    /// Lets the link move to the first configured peer while nothing is captured: a new
    /// session replaces the Mac's current one, which would end control in the middle of use.
    pub fn allow_roaming(&mut self, allowed: bool) {
        self.roam_allowed = allowed;
        if !allowed {
            self.joining = self.joining.filter(|_| !self.connected());
        }
    }

    /// Greets one configured peer per interval, in order, so the wired address wins whenever it
    /// answers and the tailnet name is only a fallback; drops a session that went silent.
    pub async fn maintain(&mut self) {
        if self.connected() && self.quiet() > IDLE_TIMEOUT {
            let reason = format!("no reply for 3 s ({} packets sent since the last one)", self.sent_since_reply);
            self.disconnect(&reason);
        }
        if self.connected() {
            self.roam().await;
            return;
        }
        if self.last_hello.is_some_and(|sent| sent.elapsed() < HELLO_INTERVAL) {
            return;
        }
        self.last_hello = Some(Instant::now());
        if self.peers.is_empty() {
            return;
        }
        let packet = self.hello();
        let peer = self.peers[self.next_peer % self.peers.len()].clone();
        self.next_peer = (self.next_peer + 1) % self.peers.len();
        if let Some(address) = self.address_of(&peer)
            && let Err(error) = self.socket.send_to(&packet, address).await
        {
            tracing::debug!("hello to {address}: {error}");
        }
    }

    /// Linked through a fallback (the tailnet) while the first configured peer (the wired
    /// address) may be back: greets the first peer now and then, and a completed handshake
    /// with it moves the session over.
    async fn roam(&mut self) {
        let Some(first) = self.peers.first().cloned() else { return };
        let Some(preferred) = self.address_of(&first) else { return };
        if !self.roam_allowed || self.peer == Some(preferred) || self.last_roam.is_some_and(|at| at.elapsed() < ROAM_INTERVAL) {
            return;
        }
        self.last_roam = Some(Instant::now());
        let packet = self.hello();
        if let Err(error) = self.socket.send_to(&packet, preferred).await {
            tracing::debug!("hello to {preferred}: {error}");
        }
    }

    /// A sealed Hello with a fresh nonce, remembered so the Challenge answering it is accepted.
    fn hello(&mut self) -> Vec<u8> {
        let nonce = random_u64();
        if self.nonces.len() == NONCE_MEMORY {
            self.nonces.pop_front();
        }
        self.nonces.push_back(nonce);
        self.seal(0, Msg::Hello { nonce })
    }

    /// Whether a handshake packet from `from` may proceed: always when unlinked, and from a
    /// better address than the current one while roaming is allowed.
    fn may_handshake(&self, from: SocketAddr) -> bool {
        !self.connected() || (self.roam_allowed && self.peer != Some(from))
    }

    /// The address for a configured peer. Literal addresses are used as they are; names are
    /// resolved by a background task and used from the next greeting on, so a slow resolver
    /// never holds the daemon's loop.
    fn address_of(&self, peer: &str) -> Option<SocketAddr> {
        if let Ok(address) = peer.parse::<SocketAddr>() {
            return Some(address);
        }
        let cached = self.resolved.borrow().get(peer).copied();
        let resolved = self.resolved.clone();
        let name = peer.to_string();
        tokio::task::spawn_local(async move {
            let found = tokio::net::lookup_host(name.as_str()).await.map(|mut addresses| addresses.find(SocketAddr::is_ipv4));
            match found {
                Ok(Some(address)) => {
                    resolved.borrow_mut().insert(name, address);
                }
                Ok(None) => {}
                Err(error) => tracing::debug!("resolving {name}: {error}"),
            }
        });
        cached
    }

    pub async fn send(&mut self, msg: Msg) {
        let (Some(token), Some(peer)) = (self.token, self.peer) else { return };
        self.sent_since_reply += 1;
        let packet = self.seal(token, msg);
        if let Err(error) = self.socket.send_to(&packet, peer).await
            && self.last_send_warning.is_none_or(|at| at.elapsed() > SEND_WARNING_INTERVAL)
        {
            self.last_send_warning = Some(Instant::now());
            tracing::warn!("sending to the Mac at {peer} failed: {error}");
        }
    }

    fn seal(&mut self, token: u64, msg: Msg) -> Vec<u8> {
        let counter = self.counter.next();
        self.cipher.seal(&Envelope { token, counter, msg }.encode())
    }

    pub async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buffer).await
    }

    pub fn try_recv(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.try_recv_from(buffer)
    }

    /// Authenticates a datagram and applies the session rules; returns what the daemon must act on.
    /// The handshake is Hello, Challenge, Join, Welcome: the Mac only opens a session for a Join
    /// that answers its latest single-use challenge, so replayed handshake packets open nothing.
    pub async fn accept(&mut self, packet: &[u8], from: SocketAddr) -> Option<Incoming> {
        let envelope = Envelope::decode(&self.cipher.open(packet)?)?;
        match envelope.msg {
            Msg::Challenge { nonce, challenge } if self.may_handshake(from) && self.nonces.contains(&nonce) => {
                self.joining = Some((from, nonce));
                let join = self.seal(0, Msg::Join { nonce, challenge });
                if let Err(error) = self.socket.send_to(&join, from).await {
                    tracing::debug!("join to {from}: {error}");
                }
                None
            }
            Msg::Welcome { nonce, token } if self.may_handshake(from) && self.joining == Some((from, nonce)) => {
                if let Some(previous) = self.peer.filter(|_| self.connected()) {
                    tracing::info!("moving the link from {previous} to {from}");
                }
                self.token = Some(token);
                self.peer = Some(from);
                self.peer_counter = envelope.counter;
                self.session_floor = self.counter.peek();
                self.heard();
                self.nonces.clear();
                self.joining = None;
                tracing::info!("linked with the Mac at {from}");
                Some(Incoming::Connected)
            }
            Msg::Reset { token, counter }
                if self.connected()
                    && Some(from) == self.peer
                    && Some(token) == self.token
                    && counter >= self.session_floor =>
            {
                self.disconnect("the Mac no longer knows this session");
                None
            }
            Msg::Challenge { .. } | Msg::Welcome { .. } | Msg::Reset { .. } => None,
            msg => {
                if Some(envelope.token) != self.token || Some(from) != self.peer {
                    return None;
                }
                if envelope.counter <= self.peer_counter {
                    return None;
                }
                self.peer_counter = envelope.counter;
                self.heard();
                Some(Incoming::Message(msg))
            }
        }
    }
}
