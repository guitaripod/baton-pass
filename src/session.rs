#[cfg(any(target_os = "macos", test))]
use crate::crypto::random_u64;
#[cfg(any(target_os = "macos", test))]
use crate::proto::{Envelope, Msg};
#[cfg(any(target_os = "macos", test))]
use std::net::SocketAddr;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
const CHALLENGE_LIFETIME: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "macos", test))]
const RESET_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(any(target_os = "macos", test))]
/// The Mac's side of the session rules, free of sockets and input injection so that both
/// machines' tests can drive it. The handshake is Hello, Challenge, Join, Welcome: a session
/// opens only for a Join that answers the latest single-use challenge, from the address it was
/// issued to, so replayed handshake packets open nothing. Within a session, every message must
/// carry the session token, come from the session's address and have a higher counter than the
/// last one accepted.
pub struct Doorman {
    token: Option<u64>,
    peer: Option<SocketAddr>,
    challenge: Option<(u64, SocketAddr, Instant)>,
    counter: u64,
    last_reset: Option<Instant>,
}

#[cfg(any(target_os = "macos", test))]
/// What the Mac must do with one authenticated datagram.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// Answer outside any session: a Challenge, or a Reset for a token it does not know or one
    /// arriving from another address.
    Answer { to: SocketAddr, msg: Msg },
    /// A new session replaced any previous one; answer with this Welcome.
    Joined { to: SocketAddr, welcome: Msg },
    /// The next message of the current session.
    Accept { token: u64, msg: Msg },
    Ignore,
}

#[cfg(any(target_os = "macos", test))]
impl Doorman {
    pub fn new() -> Self {
        Self { token: None, peer: None, challenge: None, counter: 0, last_reset: None }
    }

    pub fn token(&self) -> Option<u64> {
        self.token
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    pub fn admit(&mut self, envelope: Envelope, from: SocketAddr) -> Admission {
        match envelope.msg {
            Msg::Hello { nonce } => {
                let challenge = random_u64();
                self.challenge = Some((challenge, from, Instant::now()));
                return Admission::Answer { to: from, msg: Msg::Challenge { nonce, challenge } };
            }
            Msg::Join { nonce, challenge } => {
                let answered = self.challenge.is_some_and(|(issued, to, at)| {
                    issued == challenge && to == from && at.elapsed() < CHALLENGE_LIFETIME
                });
                if !answered {
                    return Admission::Ignore;
                }
                self.challenge = None;
                let token = random_u64();
                self.token = Some(token);
                self.peer = Some(from);
                self.counter = envelope.counter;
                return Admission::Joined { to: from, welcome: Msg::Welcome { nonce, token } };
            }
            _ => {}
        }
        if Some(envelope.token) != self.token || Some(from) != self.peer {
            let quiet = self.last_reset.is_none_or(|at| at.elapsed() > RESET_INTERVAL);
            if envelope.token != 0 && quiet {
                self.last_reset = Some(Instant::now());
                return Admission::Answer { to: from, msg: Msg::Reset { token: envelope.token, counter: envelope.counter } };
            }
            return Admission::Ignore;
        }
        if envelope.counter <= self.counter {
            return Admission::Ignore;
        }
        self.counter = envelope.counter;
        Admission::Accept { token: envelope.token, msg: envelope.msg }
    }
}

/// How long the other side has been silent, counting only time this side's loop was running
/// to hear it. A loop that was blocked counts at most `bound` per observation, so its own
/// stall is never blamed on the peer, whose packets it simply had not read yet.
#[derive(Debug, Default)]
pub struct Silence {
    quiet: Duration,
    last_observed: Option<Instant>,
}

impl Silence {
    /// Adds the time since the previous observation, at most `bound`, and returns the full gap
    /// so the caller can report a stall.
    pub fn observe(&mut self, now: Instant, bound: Duration) -> Duration {
        let gap = self.last_observed.replace(now).map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        self.quiet += gap.min(bound);
        gap
    }

    pub fn heard(&mut self) {
        self.quiet = Duration::ZERO;
    }

    pub fn quiet(&self) -> Duration {
        self.quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn envelope(token: u64, counter: u64, msg: Msg) -> Envelope {
        Envelope { token, counter, msg }
    }

    fn joined(doorman: &mut Doorman, from: SocketAddr) -> u64 {
        let Admission::Answer { msg: Msg::Challenge { challenge, .. }, .. } =
            doorman.admit(envelope(0, 1, Msg::Hello { nonce: 7 }), from)
        else {
            panic!("no challenge")
        };
        match doorman.admit(envelope(0, 2, Msg::Join { nonce: 7, challenge }), from) {
            Admission::Joined { welcome: Msg::Welcome { token, .. }, .. } => token,
            other => panic!("not joined: {other:?}"),
        }
    }

    #[test]
    fn a_join_needs_the_latest_challenge_from_the_same_address() {
        let mut doorman = Doorman::new();
        let Admission::Answer { msg: Msg::Challenge { challenge, .. }, .. } =
            doorman.admit(envelope(0, 1, Msg::Hello { nonce: 1 }), addr(1))
        else {
            panic!("no challenge")
        };
        assert_eq!(doorman.admit(envelope(0, 2, Msg::Join { nonce: 1, challenge }), addr(2)), Admission::Ignore);
        assert_eq!(doorman.admit(envelope(0, 2, Msg::Join { nonce: 1, challenge: challenge ^ 1 }), addr(1)), Admission::Ignore);
        assert!(matches!(doorman.admit(envelope(0, 2, Msg::Join { nonce: 1, challenge }), addr(1)), Admission::Joined { .. }));
        assert_eq!(doorman.admit(envelope(0, 3, Msg::Join { nonce: 1, challenge }), addr(1)), Admission::Ignore);
    }

    #[test]
    fn session_messages_need_token_address_and_a_rising_counter() {
        let mut doorman = Doorman::new();
        let token = joined(&mut doorman, addr(1));
        assert_eq!((doorman.token(), doorman.peer()), (Some(token), Some(addr(1))));
        let heartbeat = || Msg::Heartbeat { entered: false, keys: Default::default(), buttons: 0 };
        assert!(matches!(doorman.admit(envelope(token, 5, heartbeat()), addr(1)), Admission::Accept { .. }));
        assert_eq!(doorman.admit(envelope(token, 5, heartbeat()), addr(1)), Admission::Ignore);
        assert_eq!(doorman.admit(envelope(token, 4, heartbeat()), addr(1)), Admission::Ignore);
        assert!(matches!(doorman.admit(envelope(token, 9, heartbeat()), addr(1)), Admission::Accept { .. }));
        assert_eq!(doorman.admit(envelope(token, 10, heartbeat()), addr(2)), Admission::Answer {
            to: addr(2),
            msg: Msg::Reset { token, counter: 10 }
        });
        assert_eq!(doorman.admit(envelope(token ^ 1, 11, heartbeat()), addr(1)), Admission::Ignore);
    }

    #[test]
    fn silence_leaves_out_time_the_loop_was_blocked() {
        let mut silence = Silence::default();
        let start = Instant::now();
        let bound = Duration::from_millis(50);
        silence.observe(start, bound);
        silence.observe(start + Duration::from_millis(25), bound);
        let gap = silence.observe(start + Duration::from_millis(925), bound);
        assert_eq!(gap, Duration::from_millis(900));
        assert_eq!(silence.quiet(), Duration::from_millis(75));
        silence.heard();
        assert_eq!(silence.quiet(), Duration::ZERO);
    }
}
