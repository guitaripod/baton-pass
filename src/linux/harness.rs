//! End-to-end link tests: the desktop's real `Link` against the Mac's real `Doorman`, over
//! localhost UDP through a relay that loses, duplicates, delays (and so reorders) and blacks
//! out datagrams, driven like the daemon drives them.

use super::link::{Incoming, Link};
use super::{FAILSAFE, HEARTBEAT_CAPTURED, TICK};
use crate::crypto::{Cipher, Counter};
use crate::proto::{Envelope, Msg};
use crate::session::{Admission, Doorman};
use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const KEY: [u8; 32] = [42; 32];

/// What the relay does to every datagram, in both directions.
#[derive(Clone, Copy, Default)]
struct Faults {
    loss: f64,
    duplicate: f64,
    max_delay: Duration,
    blackout: Option<(Duration, Duration)>,
}

/// A small seeded generator, so a failing run fails the same way again.
struct Dice(u64);

impl Dice {
    fn roll(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The Mac's side: admits datagrams with a `Doorman` and acks every heartbeat, like the daemon.
struct FakeMac {
    socket: UdpSocket,
    doorman: RefCell<Doorman>,
    counter: RefCell<Counter>,
    accepted: Cell<usize>,
}

impl FakeMac {
    async fn serve(self: Rc<Self>, cipher: Rc<Cipher>) {
        let mut buffer = [0u8; 2048];
        while let Ok((length, from)) = self.socket.recv_from(&mut buffer).await {
            let Some(envelope) = cipher.open(&buffer[..length]).and_then(|plain| Envelope::decode(&plain)) else { continue };
            let admission = self.doorman.borrow_mut().admit(envelope, from);
            let (to, token, msg) = match admission {
                Admission::Answer { to, msg } => (to, 0, msg),
                Admission::Joined { to, welcome } => (to, 0, welcome),
                Admission::Accept { token, msg } => {
                    self.accepted.set(self.accepted.get() + 1);
                    match msg {
                        Msg::Heartbeat { .. } => (from, token, Msg::Ack { flags: 0 }),
                        _ => continue,
                    }
                }
                Admission::Ignore => continue,
            };
            let counter = self.counter.borrow_mut().next();
            let _ = self.socket.send_to(&cipher.seal(&Envelope { token, counter, msg }.encode()), to).await;
        }
    }

    /// Forgets the session, as a restarted Mac daemon would.
    fn restart(&self) {
        *self.doorman.borrow_mut() = Doorman::new();
    }
}

/// Forwards between the desktop and the Mac, applying `faults`.
async fn relay(socket: Rc<UdpSocket>, mac: SocketAddr, faults: Faults, started: Instant, seed: u64) {
    let desktop: Rc<Cell<Option<SocketAddr>>> = Rc::new(Cell::new(None));
    let mut dice = Dice(seed);
    let mut buffer = [0u8; 2048];
    while let Ok((length, from)) = socket.recv_from(&mut buffer).await {
        let to = if from == mac {
            match desktop.get() {
                Some(desktop) => desktop,
                None => continue,
            }
        } else {
            desktop.set(Some(from));
            mac
        };
        let elapsed = started.elapsed();
        if faults.blackout.is_some_and(|(from, until)| elapsed >= from && elapsed < until) || dice.roll() < faults.loss {
            continue;
        }
        let copies = if dice.roll() < faults.duplicate { 2 } else { 1 };
        for _ in 0..copies {
            let delay = faults.max_delay.mul_f64(dice.roll());
            let (socket, packet) = (socket.clone(), buffer[..length].to_vec());
            tokio::task::spawn_local(async move {
                tokio::time::sleep(delay).await;
                let _ = socket.send_to(&packet, to).await;
            });
        }
    }
}

/// What the desktop saw over one run.
#[derive(Debug, Default)]
struct Run {
    linked_after: Option<Duration>,
    links: usize,
    drops: usize,
    acks: usize,
    heartbeats: usize,
    longest_quiet: Duration,
    failsafe_trips: usize,
}

/// Drives the desktop's `Link` the way the daemon does: a tick every `TICK` that counts
/// silence, maintains the link and heartbeats at the captured cadence, with replies read as
/// they arrive. Calls `during` once per tick with the elapsed time.
async fn drive(faults: Faults, length: Duration, seed: u64, during: impl Fn(Duration, &FakeMac)) -> (Run, Rc<FakeMac>) {
    let cipher = Rc::new(Cipher::from_key(KEY));
    let mac = Rc::new(FakeMac {
        socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        doorman: RefCell::new(Doorman::new()),
        counter: RefCell::new(Counter::new()),
        accepted: Cell::new(0),
    });
    let mac_address = mac.socket.local_addr().unwrap();
    tokio::task::spawn_local(mac.clone().serve(cipher.clone()));
    let relay_socket = Rc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_address = relay_socket.local_addr().unwrap();
    let started = Instant::now();
    tokio::task::spawn_local(relay(relay_socket, mac_address, faults, started, seed));

    let mut link = Link::new(cipher, vec![relay_address.to_string()]).await.unwrap();
    let mut run = Run::default();
    let mut ticker = tokio::time::interval(TICK);
    let mut last_heartbeat = Instant::now();
    let mut buffer = [0u8; 2048];
    let mut was_connected = false;
    while started.elapsed() < length {
        tokio::select! {
            received = link.recv(&mut buffer) => {
                let Ok((size, from)) = received else { continue };
                match link.accept(&buffer[..size], from).await {
                    Some(Incoming::Connected) => {
                        run.links += 1;
                        run.linked_after.get_or_insert(started.elapsed());
                    }
                    Some(Incoming::Message(Msg::Ack { .. })) => run.acks += 1,
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                link.observe(Instant::now(), TICK * 2);
                link.maintain().await;
                let connected = link.connected();
                if was_connected && !connected {
                    run.drops += 1;
                }
                was_connected = connected;
                if connected {
                    run.longest_quiet = run.longest_quiet.max(link.quiet());
                    if link.quiet() > FAILSAFE {
                        run.failsafe_trips += 1;
                    }
                    if last_heartbeat.elapsed() >= HEARTBEAT_CAPTURED {
                        last_heartbeat = Instant::now();
                        run.heartbeats += 1;
                        link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0 }).await;
                    }
                }
                during(started.elapsed(), &mac);
            }
        }
    }
    (run, mac)
}

fn local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, future)
}

#[test]
fn stays_linked_through_loss_duplicates_and_reordering() {
    let faults = Faults { loss: 0.2, duplicate: 0.2, max_delay: Duration::from_millis(30), ..Default::default() };
    let (run, mac) = local(drive(faults, Duration::from_secs(6), 0x9e37_79b9_7f4a_7c15, |_, _| {}));
    assert!(run.linked_after.is_some_and(|after| after < Duration::from_secs(3)), "{run:?}");
    assert_eq!((run.links, run.drops, run.failsafe_trips), (1, 0, 0), "{run:?}");
    assert!(run.acks * 3 > run.heartbeats, "{run:?}");
    assert!(mac.accepted.get() <= run.heartbeats, "a duplicate was accepted twice: {run:?}");
}

#[test]
fn rides_out_a_short_blackout() {
    let blackout = Some((Duration::from_millis(1500), Duration::from_millis(1900)));
    let faults = Faults { blackout, ..Default::default() };
    let (run, _) = local(drive(faults, Duration::from_secs(4), 7, |_, _| {}));
    assert_eq!((run.links, run.drops, run.failsafe_trips), (1, 0, 0), "{run:?}");
    assert!(run.longest_quiet >= Duration::from_millis(350), "the blackout did not happen: {run:?}");
}

#[test]
fn drops_after_a_long_blackout_and_links_again() {
    let blackout = Some((Duration::from_millis(1000), Duration::from_millis(4500)));
    let faults = Faults { blackout, ..Default::default() };
    let (run, _) = local(drive(faults, Duration::from_secs(6), 11, |_, _| {}));
    assert_eq!((run.links, run.drops), (2, 1), "{run:?}");
    assert!(run.failsafe_trips > 0, "{run:?}");
}

#[test]
fn a_restarted_mac_resets_the_session_and_it_links_again() {
    let restarted = Cell::new(false);
    let (run, _) = local(drive(Faults::default(), Duration::from_secs(3), 13, |elapsed, mac| {
        if elapsed > Duration::from_millis(1000) && !restarted.replace(true) {
            mac.restart();
        }
    }));
    assert_eq!((run.links, run.drops), (2, 1), "{run:?}");
    assert_eq!(run.failsafe_trips, 0, "{run:?}");
}
