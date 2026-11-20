use crate::crypto::{Cipher, Counter, random_u64};
use crate::proto::{Envelope, Medium, Msg};
use crate::session::Silence;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, SocketAddrV4};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const HELLO_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const NONCE_MEMORY: usize = 4;
const SEND_WARNING_INTERVAL: Duration = Duration::from_secs(5);
const ROAM_INTERVAL: Duration = Duration::from_secs(2);

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
    last_version_warning: Option<Instant>,
    roam_allowed: bool,
    last_roam: Option<Instant>,
    roam_next: usize,
    advertised: Vec<(SocketAddr, Medium)>,
    epoch: Instant,
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
            last_version_warning: None,
            roam_allowed: false,
            last_roam: None,
            roam_next: 0,
            advertised: Vec::new(),
            epoch: Instant::now(),
            sent_since_reply: 0,
            flags: 0,
        })
    }

    /// This side's clock for stamping input, in microseconds. The Mac measures how late each
    /// packet arrives against it.
    pub fn clock(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Takes the Mac's list of its own addresses, and returns whether it changed. Each becomes a
    /// path to try, ranked by its medium.
    pub fn learn(&mut self, list: &[(SocketAddrV4, Medium)]) -> bool {
        let learned: Vec<(SocketAddr, Medium)> = list.iter().map(|(address, medium)| (SocketAddr::V4(*address), *medium)).collect();
        if learned == self.advertised {
            return false;
        }
        self.advertised = learned;
        true
    }

    pub fn advertised(&self) -> &[(SocketAddr, Medium)] {
        &self.advertised
    }

    /// The medium the Mac reported for one of its addresses.
    pub fn medium_of(&self, address: SocketAddr) -> Option<Medium> {
        self.advertised.iter().find(|(known, _)| *known == address).map(|(_, medium)| *medium)
    }

    /// How strongly a path is preferred, lowest first: a Mac address on a cable, then the first
    /// configured peer, then the Mac's Wi-Fi, then the rest (the tailnet and other tunnels).
    /// What the Mac reports about an address outranks where it sits in the config.
    fn rank(&self, address: SocketAddr) -> u8 {
        match self.medium_of(address) {
            Some(Medium::Wired) => 0,
            Some(Medium::Wireless) => 2,
            Some(Medium::Other) => 3,
            None if self.peers.first().and_then(|peer| self.known_address(peer)) == Some(address) => 1,
            None => 3,
        }
    }

    /// Every path to the Mac, best first: the configured peers and the addresses it reported.
    fn candidates(&self) -> Vec<SocketAddr> {
        let mut list: Vec<SocketAddr> = Vec::new();
        let configured = self.peers.iter().filter_map(|peer| self.address_of(peer));
        for address in configured.chain(self.advertised.iter().map(|(address, _)| *address)) {
            if !list.contains(&address) {
                list.push(address);
            }
        }
        list.sort_by_key(|address| self.rank(*address));
        list
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

    /// Greets one path per interval, best first, so a cable wins whenever it answers and the
    /// tailnet is only a fallback; drops a session that went silent.
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
        let candidates = self.candidates();
        if candidates.is_empty() {
            return;
        }
        let address = candidates[self.next_peer % candidates.len()];
        self.next_peer = self.next_peer.wrapping_add(1);
        let packet = self.hello();
        if let Err(error) = self.socket.send_to(&packet, address).await {
            tracing::debug!("hello to {address}: {error}");
        }
    }

    /// Linked through a lesser path (Wi-Fi, the tailnet) while a better one may answer: greets
    /// the better paths in turn, one per interval so their handshakes never cross, and a
    /// completed handshake moves the session over.
    async fn roam(&mut self) {
        let Some(current) = self.peer else { return };
        if !self.roam_allowed || self.last_roam.is_some_and(|at| at.elapsed() < ROAM_INTERVAL) {
            return;
        }
        let current_rank = self.rank(current);
        let better: Vec<SocketAddr> = self.candidates().into_iter().filter(|address| self.rank(*address) < current_rank).collect();
        if better.is_empty() {
            return;
        }
        self.last_roam = Some(Instant::now());
        let target = better[self.roam_next % better.len()];
        self.roam_next = self.roam_next.wrapping_add(1);
        let packet = self.hello();
        if let Err(error) = self.socket.send_to(&packet, target).await {
            tracing::debug!("hello to {target}: {error}");
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
    /// better path than the current one while roaming is allowed.
    fn may_handshake(&self, from: SocketAddr) -> bool {
        !self.connected() || (self.roam_allowed && self.peer.is_some_and(|peer| peer != from && self.rank(from) < self.rank(peer)))
    }

    /// A configured peer's address as far as it is known now: literal, or resolved earlier.
    fn known_address(&self, peer: &str) -> Option<SocketAddr> {
        peer.parse::<SocketAddr>().ok().or_else(|| self.resolved.borrow().get(peer).copied())
    }

    /// The address for a configured peer. Literal addresses are used as they are; names are
    /// resolved by a background task and used from the next greeting on, so a slow resolver
    /// never holds the daemon's loop.
    fn address_of(&self, peer: &str) -> Option<SocketAddr> {
        if let Ok(address) = peer.parse::<SocketAddr>() {
            return Some(address);
        }
        let cached = self.known_address(peer);
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

    /// Says, now and then, that the Mac answers in another wire version: one machine was
    /// updated and the other was not, and they cannot talk until both run the same release.
    fn note_foreign_version(&mut self, packet: &[u8], from: SocketAddr) {
        let Some(version) = crate::crypto::foreign_version(packet) else { return };
        if self.last_version_warning.is_none_or(|at| at.elapsed() > SEND_WARNING_INTERVAL * 12) {
            self.last_version_warning = Some(Instant::now());
            tracing::warn!(
                "{from} speaks Baton Pass wire version {version} and this desktop speaks {}: update both machines to the same release",
                crate::crypto::WIRE_VERSION
            );
        }
    }

    /// What a path is, for logs and status: the medium the Mac reported for it, if any.
    pub fn describe(&self, address: SocketAddr) -> &'static str {
        match self.medium_of(address) {
            Some(Medium::Wired) => "the Mac's wired address",
            Some(Medium::Wireless) => "the Mac's Wi-Fi address",
            Some(Medium::Other) => "a tunnel or other link",
            None => "a configured peer",
        }
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
        let Some(plain) = self.cipher.open(packet) else {
            self.note_foreign_version(packet, from);
            return None;
        };
        let envelope = Envelope::decode(&plain)?;
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
                    tracing::info!("moving the link from {previous} ({}) to {from} ({})", self.describe(previous), self.describe(from));
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
