//! End-to-end link tests: the desktop's real `Link` against the Mac's real `Doorman`, over
//! localhost UDP through a relay that loses, duplicates, delays (and so reorders), blacks out
//! and stalls datagrams, driven like the daemon drives them.

use super::link::{Incoming, Link};
use super::{FAILSAFE, HEARTBEAT_CAPTURED, TICK};
use crate::crypto::{Cipher, Counter};
use crate::jitter::{KeyLease, Lateness, simulated_repeats};
use crate::proto::{Envelope, KeyState, Msg};
use crate::session::{Admission, Doorman};
use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const KEY: [u8; 32] = [42; 32];

/// What the relay does to every datagram, in both directions.
#[derive(Clone, Copy, Default)]
struct Faults {
    loss: f64,
    duplicate: f64,
    max_delay: Duration,
    blackout: Option<(Duration, Duration)>,
    stall: Option<(Duration, Duration)>,
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

/// The key repeat the fake Mac plays back: its owner's settings, 166 ms then every 16.7 ms.
const REPEAT_DELAY: u64 = 166_667;
const REPEAT_INTERVAL: u64 = 16_667;

/// The Mac daemon's keyboard path: every key state the desktop sends goes through the lease,
/// or straight to the "HID device" without it, and each state posted is recorded with the
/// Mac's time so the key repeat macOS would produce can be counted afterwards.
struct FakeKeyboard {
    leased: bool,
    epoch: Instant,
    lateness: Lateness,
    lease: KeyLease,
    posts: Vec<(u64, KeyState)>,
}

impl FakeKeyboard {
    fn new(leased: bool) -> Self {
        Self { leased, epoch: Instant::now(), lateness: Lateness::default(), lease: KeyLease::default(), posts: vec![(0, KeyState::default())] }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn post(&mut self, at: u64, state: KeyState) {
        if self.posts.last().map(|(_, last)| last) != Some(&state) {
            self.posts.push((at, state));
        }
    }

    fn receive(&mut self, keys: KeyState, at: u64) {
        let now = self.now();
        self.lateness.observe(at, now);
        let desktop_now = self.lateness.desktop_now(now).unwrap_or(at);
        let state = if self.leased { self.lease.confirm(keys, at, now, desktop_now, REPEAT_DELAY) } else { keys };
        self.post(now, state);
    }

    fn tick(&mut self) {
        let now = self.now();
        if self.leased
            && let Some(desktop_now) = self.lateness.desktop_now(now)
            && let Some(state) = self.lease.expire(now, desktop_now, REPEAT_DELAY)
        {
            self.post(now, state);
        }
    }
}

/// The Mac's side: admits datagrams with a `Doorman` and acks every heartbeat, like the daemon.
struct FakeMac {
    socket: UdpSocket,
    doorman: RefCell<Doorman>,
    counter: RefCell<Counter>,
    accepted: Cell<usize>,
    keyboard: Option<RefCell<FakeKeyboard>>,
}

impl FakeMac {
    async fn bind(keyboard: Option<FakeKeyboard>) -> Rc<Self> {
        Rc::new(FakeMac {
            socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            doorman: RefCell::new(Doorman::new()),
            counter: RefCell::new(Counter::new()),
            accepted: Cell::new(0),
            keyboard: keyboard.map(RefCell::new),
        })
    }

    async fn send(&self, cipher: &Cipher, to: SocketAddr, token: u64, msg: Msg) {
        let counter = self.counter.borrow_mut().next();
        let _ = self.socket.send_to(&cipher.seal(&Envelope { token, counter, msg }.encode()), to).await;
    }

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
                        Msg::Heartbeat { entered, keys, at, .. } => {
                            if let Some(keyboard) = &self.keyboard
                                && entered
                            {
                                keyboard.borrow_mut().receive(keys, at);
                            }
                            (from, token, Msg::Ack { flags: 0, stalls: 0, worst: 0 })
                        }
                        Msg::Keys { keys, at } => {
                            if let Some(keyboard) = &self.keyboard {
                                keyboard.borrow_mut().receive(keys, at);
                            }
                            continue;
                        }
                        _ => continue,
                    }
                }
                Admission::Ignore => continue,
            };
            self.send(&cipher, to, token, msg).await;
        }
    }

    /// Ticks the keyboard every 4 ms, like the Mac daemon's loop while it has control.
    async fn tick_keyboard(self: Rc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(4));
        loop {
            ticker.tick().await;
            if let Some(keyboard) = &self.keyboard {
                keyboard.borrow_mut().tick();
            }
        }
    }

    /// Forgets the session, as a restarted Mac daemon would.
    fn restart(&self) {
        *self.doorman.borrow_mut() = Doorman::new();
    }
}

/// Forwards between the desktop and the Mac, applying `faults`. A stall holds every datagram
/// sent while it lasts and releases them in order when it ends, the way the Mac's Wi-Fi does
/// while AWDL has its radio on another channel.
async fn relay(socket: Rc<UdpSocket>, mac: SocketAddr, faults: Faults, started: Instant, seed: u64) {
    let desktop: Rc<Cell<Option<SocketAddr>>> = Rc::new(Cell::new(None));
    let mut dice = Dice(seed);
    let (held, mut releases) = mpsc::unbounded_channel::<(Instant, SocketAddr, Vec<u8>)>();
    let forwarder = socket.clone();
    tokio::task::spawn_local(async move {
        while let Some((at, to, packet)) = releases.recv().await {
            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            let _ = forwarder.send_to(&packet, to).await;
        }
    });
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
        if let Some((period, stalled)) = faults.stall {
            let phase = Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64);
            let release = if phase < stalled { Instant::now() + (stalled - phase) } else { Instant::now() };
            let _ = held.send((release, to, buffer[..length].to_vec()));
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
    let (run, mac, _) = drive_paths(&[faults], length, seed, during).await;
    (run, mac)
}

/// Like `drive`, with one relay per entry of `paths`, configured as peers in that order (the
/// first standing for the wired address, the rest for fallbacks). Also returns the relay
/// addresses and the address the link ended on.
async fn drive_paths(
    paths: &[Faults],
    length: Duration,
    seed: u64,
    during: impl Fn(Duration, &FakeMac),
) -> (Run, Rc<FakeMac>, (Vec<SocketAddr>, Option<SocketAddr>)) {
    let cipher = Rc::new(Cipher::from_secret(KEY));
    let mac = FakeMac::bind(None).await;
    let mac_address = mac.socket.local_addr().unwrap();
    tokio::task::spawn_local(mac.clone().serve(cipher.clone()));
    let started = Instant::now();
    let mut relays = Vec::new();
    for (index, faults) in paths.iter().enumerate() {
        let relay_socket = Rc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        relays.push(relay_socket.local_addr().unwrap());
        tokio::task::spawn_local(relay(relay_socket, mac_address, *faults, started, seed + index as u64));
    }

    let mut link = Link::new(cipher, relays.iter().map(SocketAddr::to_string).collect()).await.unwrap();
    link.allow_roaming(true);
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
                        let at = link.clock();
                        link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0, at }).await;
                    }
                }
                during(started.elapsed(), &mac);
            }
        }
    }
    let ended_on = link.peer.filter(|_| link.connected());
    (run, mac, (relays, ended_on))
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
fn rides_out_a_one_second_blackout() {
    let blackout = Some((Duration::from_millis(1500), Duration::from_millis(2500)));
    let faults = Faults { blackout, ..Default::default() };
    let (run, _) = local(drive(faults, Duration::from_secs(4), 7, |_, _| {}));
    assert_eq!((run.links, run.drops, run.failsafe_trips), (1, 0, 0), "{run:?}");
    assert!(run.longest_quiet >= Duration::from_millis(900), "the blackout did not happen: {run:?}");
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

#[test]
fn moves_back_to_the_wired_path_once_it_answers() {
    let wired_down = Faults { blackout: Some((Duration::ZERO, Duration::from_secs(2))), ..Default::default() };
    let (run, _, (relays, ended_on)) = local(drive_paths(&[wired_down, Faults::default()], Duration::from_secs(9), 17, |_, _| {}));
    assert_eq!(ended_on, Some(relays[0]), "{run:?}");
    assert_eq!((run.links, run.drops, run.failsafe_trips), (2, 0, 0), "{run:?}");
}

/// Types on the desktop through a link that stalls like Wi-Fi under AWDL: 130 ms every
/// 524 ms, with `probe::tap`, which sends keys the way the daemon does. Returns the key states
/// the fake Mac posted and how long the run lasted on its clock.
async fn type_through_stalls(leased: bool, taps: usize, hold: Duration) -> (Vec<(u64, KeyState)>, u64) {
    let cipher = Rc::new(Cipher::from_secret(KEY));
    let mac = FakeMac::bind(Some(FakeKeyboard::new(leased))).await;
    let mac_address = mac.socket.local_addr().unwrap();
    tokio::task::spawn_local(mac.clone().serve(cipher.clone()));
    tokio::task::spawn_local(mac.clone().tick_keyboard());
    let relay_socket = Rc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_address = relay_socket.local_addr().unwrap();
    let stall = Some((Duration::from_millis(524), Duration::from_millis(130)));
    tokio::task::spawn_local(relay(relay_socket, mac_address, Faults { stall, ..Default::default() }, Instant::now(), 1));

    let mut link = Link::new(cipher, vec![relay_address.to_string()]).await.unwrap();
    let mut buffer = [0u8; 2048];
    while !link.connected() {
        link.observe(Instant::now(), TICK * 2);
        link.maintain().await;
        if let Ok(Ok((size, from))) = tokio::time::timeout(TICK, link.recv(&mut buffer)).await {
            link.accept(&buffer[..size], from).await;
        }
    }

    super::probe::tap(&mut link, 4, taps, hold).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let keyboard = mac.keyboard.as_ref().unwrap().borrow();
    (keyboard.posts.clone(), keyboard.now())
}

#[test]
fn taps_never_repeat_through_wifi_stalls() {
    let (raw, raw_end) = local(type_through_stalls(false, 20, Duration::from_millis(90)));
    let raw_repeats = simulated_repeats(&raw, 4, raw_end, REPEAT_DELAY, REPEAT_INTERVAL);
    assert_eq!(raw_repeats.len(), 20, "every tap reached the Mac once: {raw:?}");
    assert!(raw_repeats.iter().sum::<u64>() > 0, "the stalls never made a tap repeat, so this test proves nothing: {raw_repeats:?}");

    let (leased, leased_end) = local(type_through_stalls(true, 20, Duration::from_millis(90)));
    let repeats = simulated_repeats(&leased, 4, leased_end, REPEAT_DELAY, REPEAT_INTERVAL);
    assert_eq!(repeats.len(), 20, "every tap reached the Mac once: {leased:?}");
    assert!(repeats.iter().all(|count| *count == 0), "{repeats:?}");
}

#[test]
fn a_held_key_still_repeats_through_wifi_stalls() {
    let (posts, end) = local(type_through_stalls(true, 1, Duration::from_millis(1500)));
    let repeats: u64 = simulated_repeats(&posts, 4, end, REPEAT_DELAY, REPEAT_INTERVAL).iter().sum();
    assert!(repeats >= 25, "a 1.5 s hold should keep repeating between stalls: {repeats} repeats, {posts:?}");
    assert!(posts.last().is_some_and(|(_, state)| state.keys.is_empty()), "{posts:?}");
}
