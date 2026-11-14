mod display;
mod linear;
pub mod pasteboard;
mod vhid;

use crate::config::{Confine, Config, Side};
use crate::crypto::{Cipher, Counter, random_u64};
use crate::proto::{Envelope, FLAG_ENTERED, KeyState, MAX_CLIPBOARD, Msg};
use anyhow::{Context, Result, bail};
use display::{Point, Rect};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::signal::unix::{SignalKind, signal};

const POLL_ENTERED: Duration = Duration::from_millis(4);
const POLL_IDLE: Duration = Duration::from_millis(100);
const SILENCE_LIMIT: Duration = Duration::from_millis(500);
const LEAVE_LIMIT: Duration = Duration::from_secs(3);
const DISPLAY_REFRESH: Duration = Duration::from_secs(1);
const EXIT_RESEND: Duration = Duration::from_millis(30);
const PUSH_MEMORY: Duration = Duration::from_millis(60);
const CALIBRATION_COUNTS: i8 = 120;
const CALIBRATION_REPORTS: i32 = 2;
const RATIO_SAMPLES: usize = 7;
const RATIO_SHIFT: f64 = 0.12;
const NUDGE_SETTLE: Duration = Duration::from_millis(20);
const SLOW_HANDLER: Duration = Duration::from_millis(50);
const LATE_TICK: Duration = Duration::from_millis(150);
const PARKING_LIMIT: Duration = Duration::from_millis(500);
const CHALLENGE_LIFETIME: Duration = Duration::from_secs(5);
const EXIT_ARM_DISTANCE: f64 = 24.0;

/// Counts posted since `start` was read, for re-measuring the count-to-point ratio during use.
struct Window {
    start: Point,
    opened: Instant,
    posted: (f64, f64),
}

/// Session state that both the datagram loop and the clipboard connections check.
struct Gate {
    token: Option<u64>,
    peer: Option<SocketAddr>,
    tcp_counter: u64,
    counter: Counter,
    clipboard_busy: bool,
}

struct Receiver {
    socket: Rc<UdpSocket>,
    cipher: Rc<Cipher>,
    gate: Rc<RefCell<Gate>>,
    confine: Confine,
    udp_counter: u64,
    quiet: Duration,
    quiet_released: bool,
    last_tick: Option<Instant>,
    watcher: display::Watcher,
    last_reset: Instant,
    challenge: Option<(u64, SocketAddr, Instant)>,
    entered: bool,
    exiting: Option<(f32, Instant)>,
    exit_side: Side,
    exit_enabled: bool,
    exit_armed: bool,
    speed: f32,
    keys: KeyState,
    buttons: u32,
    remainder: (f32, f32),
    wheel: (i32, i32),
    pushed: Option<(f32, f32, Instant)>,
    points_per_count: Option<f64>,
    last_calibration: Option<Instant>,
    pointer_was_ready: bool,
    recalibrate: bool,
    window: Option<Window>,
    ratio_samples: Vec<f64>,
    last_nudge: Option<Instant>,
    parking: Option<(Point, Instant)>,
    displays: Vec<Rect>,
    displays_at: Option<Instant>,
}

pub async fn run(config: Config) -> Result<()> {
    let cipher = Rc::new(Cipher::load(&Config::key_path())?);
    vhid::start();
    let socket = Rc::new(UdpSocket::bind(&config.listen).await.with_context(|| format!("binding {}", config.listen))?);
    let listener = TcpListener::bind(&config.listen).await?;
    let gate = Rc::new(RefCell::new(Gate {
        token: None,
        peer: None,
        tcp_counter: 0,
        counter: Counter::new(),
        clipboard_busy: false,
    }));
    let mut receiver = Receiver {
        socket,
        cipher: cipher.clone(),
        gate: gate.clone(),
        confine: config.confine,
        udp_counter: 0,
        quiet: Duration::ZERO,
        quiet_released: false,
        last_tick: None,
        watcher: display::Watcher::start(DISPLAY_REFRESH),
        last_reset: Instant::now() - Duration::from_secs(1),
        challenge: None,
        entered: false,
        exiting: None,
        exit_side: Side::Right,
        exit_enabled: true,
        exit_armed: false,
        speed: 1.0,
        keys: KeyState::default(),
        buttons: 0,
        remainder: (0.0, 0.0),
        wheel: (0, 0),
        pushed: None,
        points_per_count: None,
        last_calibration: None,
        pointer_was_ready: false,
        recalibrate: false,
        window: None,
        ratio_samples: Vec::new(),
        last_nudge: None,
        parking: None,
        displays: Vec::new(),
        displays_at: None,
    };

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut buffer = vec![0u8; 2048];
    tracing::info!("ferry {} listening on {}", env!("CARGO_PKG_VERSION"), config.listen);
    let mut next_tick = tokio::time::Instant::now();
    loop {
        let poll = receiver.poll_interval();
        next_tick = next_tick.min(tokio::time::Instant::now() + poll);
        tokio::select! {
            received = receiver.socket.recv_from(&mut buffer) => match received {
                Ok((length, from)) => {
                    let started = Instant::now();
                    receiver.on_datagram(&buffer[..length], from).await;
                    report_slow("handling a datagram", started);
                }
                Err(error) => tracing::debug!("udp receive: {error}"),
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, from)) => {
                    let mut state = gate.borrow_mut();
                    let allowed = state.peer.map(|peer| peer.ip()) == Some(from.ip()) && !state.clipboard_busy;
                    if allowed {
                        state.clipboard_busy = true;
                        tokio::task::spawn_local(serve_clipboard(stream, cipher.clone(), gate.clone()));
                    }
                }
                Err(error) => tracing::debug!("tcp accept: {error}"),
            },
            _ = tokio::time::sleep_until(next_tick) => {
                let late = tokio::time::Instant::now().saturating_duration_since(next_tick);
                if late > LATE_TICK {
                    tracing::warn!("event loop stalled: tick ran {} ms late", late.as_millis());
                }
                let started = Instant::now();
                receiver.tick().await;
                report_slow("the periodic tick", started);
                next_tick = tokio::time::Instant::now() + receiver.poll_interval();
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    receiver.release_all();
    Ok(())
}

impl Receiver {
    async fn reply(&mut self, to: SocketAddr, token: u64, msg: Msg) {
        let counter = self.gate.borrow_mut().counter.next();
        let packet = self.cipher.seal(&Envelope { token, counter, msg }.encode());
        if let Err(error) = self.socket.send_to(&packet, to).await {
            tracing::debug!("send to {to}: {error}");
        }
    }

    async fn on_datagram(&mut self, packet: &[u8], from: SocketAddr) {
        let Some(envelope) = self.cipher.open(packet).and_then(|plain| Envelope::decode(&plain)) else {
            return;
        };
        match envelope.msg {
            Msg::Hello { nonce } => {
                let challenge = random_u64();
                self.challenge = Some((challenge, from, Instant::now()));
                self.reply(from, 0, Msg::Challenge { nonce, challenge }).await;
                return;
            }
            Msg::Join { nonce, challenge } => {
                let answered = self.challenge.is_some_and(|(issued, to, at)| {
                    issued == challenge && to == from && at.elapsed() < CHALLENGE_LIFETIME
                });
                if !answered {
                    return;
                }
                self.challenge = None;
                let token = random_u64();
                {
                    let mut gate = self.gate.borrow_mut();
                    gate.token = Some(token);
                    gate.peer = Some(from);
                    gate.tcp_counter = 0;
                }
                self.udp_counter = envelope.counter;
                self.heard();
                self.leave("new session");
                tracing::info!("session with {from}");
                self.reply(from, 0, Msg::Welcome { nonce, token }).await;
                return;
            }
            _ => {}
        }
        let (token, peer) = {
            let gate = self.gate.borrow();
            (gate.token, gate.peer)
        };
        if Some(envelope.token) != token || Some(from) != peer {
            if envelope.token != 0 && self.last_reset.elapsed() > Duration::from_millis(200) {
                self.last_reset = Instant::now();
                self.reply(from, 0, Msg::Reset { token: envelope.token, counter: envelope.counter }).await;
            }
            return;
        }
        if envelope.counter <= self.udp_counter {
            return;
        }
        self.udp_counter = envelope.counter;
        self.heard();
        let token = envelope.token;
        match envelope.msg {
            Msg::Heartbeat { entered, keys, buttons } => {
                if entered && !self.entered {
                    self.entered = true;
                    tracing::info!("in control (resumed by heartbeat)");
                } else if !entered && self.entered {
                    self.leave("heartbeat says released");
                }
                if self.entered {
                    self.apply_keys(keys);
                    if buttons != self.buttons && self.exiting.is_none() {
                        self.buttons = buttons;
                        vhid::pointing(buttons, 0, 0, 0, 0);
                    }
                }
                let mut flags = vhid::flags();
                if self.entered {
                    flags |= FLAG_ENTERED;
                }
                self.reply(from, token, Msg::Ack { flags }).await;
            }
            Msg::Enter { side, frac, speed, exit } => {
                self.exit_enabled = exit;
                self.enter(Side::from_wire(side).unwrap_or(Side::Left), frac, speed).await;
            }
            Msg::Leave => self.leave("released"),
            Msg::Pointer { dx, dy, wheel_v, wheel_h, buttons } => {
                self.pointer(dx, dy, wheel_v, wheel_h, buttons).await;
            }
            Msg::Keys(keys)
                if self.entered => {
                    self.apply_keys(keys);
                }
            _ => {}
        }
    }

    async fn enter(&mut self, arch_side: Side, frac: f32, speed: f32) {
        self.release_all();
        self.entered = true;
        self.exiting = None;
        self.exit_armed = false;
        self.exit_side = arch_side.opposite();
        self.speed = speed.clamp(0.05, 20.0);
        self.remainder = (0.0, 0.0);
        self.wheel = (0, 0);
        self.pushed = None;
        self.refresh_displays(true);
        if self.points_per_count.is_none() {
            self.calibrate().await;
        }
        self.window = None;
        if frac.is_finite() {
            if let Some(point) = self.entry_point(frac) {
                self.move_to(point).await;
            }
        } else if let Some(cursor) = display::cursor()
            && !self.displays.iter().any(|rect| rect.contains(cursor))
                && let Some(first) = self.displays.first().copied() {
                    self.move_to(first.center()).await;
                }
        self.last_nudge = Some(Instant::now());
        tracing::info!("in control (entered at {})", if frac.is_finite() { format!("{frac:.2}") } else { "hotkey".into() });
    }

    fn leave(&mut self, reason: &str) {
        if self.entered {
            tracing::info!("control returned ({reason})");
        }
        self.entered = false;
        self.exiting = None;
        self.release_all();
    }

    fn release_all(&mut self) {
        self.keys = KeyState::default();
        self.buttons = 0;
        vhid::keyboard(&self.keys);
        vhid::consumer(&[]);
        vhid::pointing(0, 0, 0, 0, 0);
    }

    fn apply_keys(&mut self, keys: KeyState) {
        if keys.modifiers != self.keys.modifiers || keys.keys != self.keys.keys {
            vhid::keyboard(&keys);
        }
        if keys.consumer != self.keys.consumer {
            vhid::consumer(&keys.consumer);
        }
        self.keys = keys;
    }

    async fn pointer(&mut self, dx: f32, dy: f32, wheel_v: i32, wheel_h: i32, buttons: u32) {
        if !self.entered {
            return;
        }
        if let Some((frac, sent)) = self.exiting {
            if sent.elapsed() >= EXIT_RESEND {
                self.send_exit(frac).await;
            }
            return;
        }
        let gain = (self.speed as f64 / self.points_per_count.unwrap_or(1.0)) as f32;
        self.remainder.0 += dx * gain;
        self.remainder.1 += dy * gain;
        let mut x = self.remainder.0.trunc() as i32;
        let mut y = self.remainder.1.trunc() as i32;
        self.remainder.0 -= x as f32;
        self.remainder.1 -= y as f32;
        self.wheel.0 += wheel_v;
        self.wheel.1 += wheel_h;
        let mut vertical = self.wheel.0 / 120;
        let mut horizontal = self.wheel.1 / 120;
        self.wheel.0 -= vertical * 120;
        self.wheel.1 -= horizontal * 120;
        if dx != 0.0 || dy != 0.0 {
            self.pushed = Some((dx, dy, Instant::now()));
        }
        let buttons_changed = buttons != self.buttons;
        self.buttons = buttons;
        if x == 0 && y == 0 && vertical == 0 && horizontal == 0 && !buttons_changed {
            return;
        }
        if let Some(window) = self.window.as_mut() {
            window.posted.0 += x as f64;
            window.posted.1 += y as f64;
        }
        post_motion(buttons, &mut x, &mut y, &mut vertical, &mut horizontal);
    }

    /// Watches for the count-to-point ratio changing under ferry (LinearMouse applying its
    /// profile, a new tracking speed) by comparing, over each tenth of a second, how far the
    /// cursor went with how many counts were posted. Windows that overlap ferry's own corrective
    /// nudges are skipped, and the ratio only moves on a sustained shift (see `shifted_ratio`),
    /// so the pointer speed never wobbles with measurement noise.
    fn learn_ratio(&mut self, cursor: Point) {
        const MARGIN: f64 = 24.0;
        const MIN_TRAVEL: f64 = 40.0;
        let inside = |point: Point| {
            self.displays.iter().any(|rect| {
                point.x >= rect.min_x() + MARGIN
                    && point.x < rect.max_x() - MARGIN
                    && point.y >= rect.min_y() + MARGIN
                    && point.y < rect.max_y() - MARGIN
            })
        };
        let Some(window) = &self.window else {
            self.window = Some(Window { start: cursor, opened: Instant::now(), posted: (0.0, 0.0) });
            return;
        };
        if window.opened.elapsed() < Duration::from_millis(100) {
            return;
        }
        let posted = window.posted.0.hypot(window.posted.1);
        let moved = (cursor.x - window.start.x).hypot(cursor.y - window.start.y);
        let ratio = self.points_per_count.unwrap_or(1.0);
        let disturbed = self.last_nudge.is_some_and(|at| at + NUDGE_SETTLE > window.opened);
        if !disturbed && posted * ratio >= MIN_TRAVEL && inside(window.start) && inside(cursor) {
            let sample = moved / posted;
            if (0.05..=10.0).contains(&sample) {
                if self.ratio_samples.len() == RATIO_SAMPLES {
                    self.ratio_samples.remove(0);
                }
                self.ratio_samples.push(sample);
                if let Some(shifted) = shifted_ratio(&self.ratio_samples, ratio) {
                    tracing::info!("virtual mouse now moves {shifted:.3} points per count; re-measuring when idle");
                    self.points_per_count = Some(shifted);
                    self.ratio_samples.clear();
                    self.recalibrate = true;
                }
            }
        }
        self.window = Some(Window { start: cursor, opened: Instant::now(), posted: (0.0, 0.0) });
    }

    /// Posts one relative move toward `target`, sized with the calibrated count-to-point ratio.
    /// Warping is avoided on purpose: macOS drops hardware mouse input for a quarter second after one.
    fn nudge_to(&self, target: Point) -> bool {
        let (Some(ratio), Some(now)) = (self.points_per_count, display::cursor()) else { return true };
        let (mut x, mut y) = (((target.x - now.x) / ratio).round() as i32, ((target.y - now.y) / ratio).round() as i32);
        if x == 0 && y == 0 {
            return true;
        }
        post_motion(self.buttons, &mut x, &mut y, &mut 0, &mut 0);
        false
    }

    /// How far inside an edge the cursor is placed: two points, plus one count's worth of travel
    /// so a rounded move can never land it on the edge itself.
    fn inset(&self) -> f64 {
        2.0 + self.points_per_count.unwrap_or(1.0)
    }

    async fn move_to(&self, target: Point) {
        for _ in 0..4 {
            if self.nudge_to(target) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(6)).await;
        }
    }

    async fn send_exit(&mut self, frac: f32) {
        self.exiting = Some((frac, Instant::now()));
        let (token, peer) = {
            let gate = self.gate.borrow();
            (gate.token, gate.peer)
        };
        if let (Some(token), Some(peer)) = (token, peer) {
            self.reply(peer, token, Msg::Exit { frac }).await;
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.entered || self.parking.is_some() { POLL_ENTERED } else { POLL_IDLE }
    }

    /// Keeps pulling the cursor back to where it left until it sits on an allowed display, since
    /// motion posted just before the crossing can still land after the first correction.
    fn settle_parking(&mut self) {
        let Some((park, since)) = self.parking else { return };
        let settled = display::cursor().is_some_and(|cursor| self.displays.iter().any(|rect| rect.contains(cursor)));
        if settled || since.elapsed() > PARKING_LIMIT {
            self.parking = None;
        } else {
            self.nudge_to(park);
        }
    }

    /// Reads every datagram already waiting. Run before judging the desktop silent, so packets
    /// left unread while this loop was busy are never mistaken for the desktop going quiet.
    async fn drain(&mut self) {
        let mut buffer = [0u8; 2048];
        while let Ok((length, from)) = self.socket.try_recv_from(&mut buffer) {
            self.on_datagram(&buffer[..length], from).await;
        }
    }

    fn heard(&mut self) {
        self.quiet = Duration::ZERO;
        self.quiet_released = false;
    }

    /// Counts the time since the last tick toward the desktop's silence, but only as much as a
    /// running loop would take: time this loop spent blocked is not the desktop's fault.
    fn note_tick(&mut self) {
        let now = Instant::now();
        let bound = self.poll_interval() * 2;
        let gap = self.last_tick.replace(now).map_or(Duration::ZERO, |at| now - at);
        self.quiet += gap.min(bound);
    }

    /// A short silence only lets go of held keys and buttons, so nothing repeats or drags on its
    /// own; control stays here and carries on with the next packet. Only a long silence, the
    /// desktop gone for good, gives control up.
    async fn check_silence(&mut self) {
        if !self.entered || self.quiet <= SILENCE_LIMIT {
            return;
        }
        self.drain().await;
        if self.quiet > LEAVE_LIMIT {
            self.leave("the desktop went quiet");
        } else if self.quiet > SILENCE_LIMIT && !self.quiet_released {
            self.quiet_released = true;
            self.release_all();
            tracing::info!("desktop quiet for {} ms; keys released, still in control", self.quiet.as_millis());
        }
    }

    async fn tick(&mut self) {
        self.note_tick();
        self.settle_parking();
        self.check_silence().await;
        let ready = vhid::pointing_ready();
        if ready != self.pointer_was_ready {
            self.pointer_was_ready = ready;
            self.recalibrate = true;
            self.last_calibration = None;
        }
        if !self.entered {
            let due = self.last_calibration.is_none_or(|at| at.elapsed() > Duration::from_secs(30));
            if (self.points_per_count.is_none() || self.recalibrate) && due && ready {
                self.calibrate().await;
            }
            return;
        }
        if self.exiting.is_some() {
            return;
        }
        self.refresh_displays(false);
        let Some(cursor) = display::cursor() else { return };
        self.learn_ratio(cursor);
        if !self.exit_armed {
            self.exit_armed = distance_from_edge(&self.displays, self.exit_side, cursor) >= EXIT_ARM_DISTANCE;
        }
        let pushing = self.pushed.filter(|(_, _, at)| at.elapsed() < PUSH_MEMORY).map(|(dx, dy, _)| (dx, dy));
        match edge_decision(&self.displays, self.exit_side, cursor, pushing, self.inset()) {
            EdgeDecision::Stay => {}
            EdgeDecision::Exit { park, .. } if !self.exit_enabled || !self.exit_armed => {
                self.nudge_to(park);
                self.last_nudge = Some(Instant::now());
            }
            EdgeDecision::Exit { frac, park } => {
                vhid::pointing(0, 0, 0, 0, 0);
                self.buttons = 0;
                self.nudge_to(park);
                self.parking = Some((park, Instant::now()));
                tracing::info!("cursor crossed the {:?} edge", self.exit_side);
                self.send_exit(frac).await;
            }
            EdgeDecision::Clamp(point) => {
                self.nudge_to(point);
                self.last_nudge = Some(Instant::now());
            }
        }
    }

    fn refresh_displays(&mut self, force: bool) {
        if !force && self.displays_at.is_some_and(|at| at.elapsed() < DISPLAY_REFRESH) {
            return;
        }
        self.displays_at = Some(Instant::now());
        let online = if force { display::online() } else { self.watcher.latest() };
        let builtin: Vec<Rect> = online.iter().filter(|d| d.builtin).map(|d| d.bounds).collect();
        self.displays = match self.confine {
            Confine::Builtin if !builtin.is_empty() => builtin,
            _ => online.iter().map(|d| d.bounds).collect(),
        };
    }

    fn entry_point(&self, frac: f32) -> Option<Point> {
        let rect = edge_rect(&self.displays, self.exit_side, None)?;
        let frac = frac.clamp(0.0, 1.0) as f64;
        let inset = self.inset();
        Some(match self.exit_side {
            Side::Right => Point { x: rect.max_x() - inset, y: rect.min_y() + frac * (rect.size.y - 1.0) },
            Side::Left => Point { x: rect.min_x() + inset, y: rect.min_y() + frac * (rect.size.y - 1.0) },
            Side::Bottom => Point { x: rect.min_x() + frac * (rect.size.x - 1.0), y: rect.max_y() - inset },
            Side::Top => Point { x: rect.min_x() + frac * (rect.size.x - 1.0), y: rect.min_y() + inset },
        })
    }

    /// Measures how many points the cursor travels per virtual-mouse count with a short
    /// out-and-back move toward the middle of its display.
    async fn calibrate(&mut self) {
        self.last_calibration = Some(Instant::now());
        if !vhid::pointing_ready() {
            return;
        }
        linear::ensure_linear_pointer().await;
        self.refresh_displays(true);
        let Some(start) = display::cursor() else { return };
        let Some(area) = self
            .displays
            .iter()
            .min_by(|a, b| a.distance(start).total_cmp(&b.distance(start)))
            .copied()
        else {
            return;
        };
        let direction: i8 = if start.x < area.center().x { 1 } else { -1 };
        let mut samples = Vec::with_capacity(2);
        let counts = (CALIBRATION_COUNTS as i32 * CALIBRATION_REPORTS) as f64;
        for _ in 0..2 {
            let Some(from) = display::cursor() else { return };
            for _ in 0..CALIBRATION_REPORTS {
                vhid::pointing(0, direction * CALIBRATION_COUNTS, 0, 0, 0);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let reached = display::cursor();
            for _ in 0..CALIBRATION_REPORTS {
                vhid::pointing(0, -direction * CALIBRATION_COUNTS, 0, 0, 0);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let Some(reached) = reached else { return };
            if (reached.y - from.y).abs() > 1.0 {
                tracing::info!("calibration disturbed by other pointer movement; retrying later");
                return;
            }
            samples.push((reached.x - from.x) * direction as f64 / counts);
        }
        let (first, second) = (samples[0], samples[1]);
        if first <= 0.02 || second <= 0.02 || (first - second).abs() > 0.05 * first.max(second) {
            tracing::info!("calibration samples disagree ({first:.3}, {second:.3}); retrying later");
            return;
        }
        let ratio = (first + second) / 2.0;
        tracing::info!("virtual mouse moves {ratio:.3} points per count");
        self.points_per_count = Some(ratio);
        self.ratio_samples.clear();
        self.recalibrate = false;
    }
}

/// Logs a handler that held the event loop long enough to delay input and replies.
fn report_slow(what: &str, started: Instant) {
    let took = started.elapsed();
    if took > SLOW_HANDLER {
        tracing::warn!("{what} held the event loop for {} ms", took.as_millis());
    }
}

/// The ratio to switch to once the last `RATIO_SAMPLES` measurements agree it has moved: their
/// median, when it differs from `ratio` by more than `RATIO_SHIFT`. Real changes (a profile
/// applied, a new tracking speed) are step changes far beyond that; anything smaller is noise.
fn shifted_ratio(samples: &[f64], ratio: f64) -> Option<f64> {
    if samples.len() < RATIO_SAMPLES {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    ((median - ratio).abs() / ratio > RATIO_SHIFT).then_some(median)
}

/// Posts a relative pointer move, split into reports that fit the virtual mouse's 8-bit fields.
fn post_motion(buttons: u32, x: &mut i32, y: &mut i32, vertical: &mut i32, horizontal: &mut i32) {
    let step = |value: &mut i32| -> i8 {
        let part = (*value).clamp(-127, 127);
        *value -= part;
        part as i8
    };
    loop {
        let (sx, sy, sv, sh) = (step(x), step(y), step(vertical), step(horizontal));
        vhid::pointing(buttons, sx, sy, sv, sh);
        if *x == 0 && *y == 0 && *vertical == 0 && *horizontal == 0 {
            break;
        }
    }
}

#[derive(Debug, PartialEq)]
enum EdgeDecision {
    Stay,
    Exit { frac: f32, park: Point },
    Clamp(Point),
}

/// The display whose `side` edge is outermost, preferring one spanning `along` (the cursor's
/// position parallel to that edge).
fn edge_rect(displays: &[Rect], side: Side, along: Option<f64>) -> Option<Rect> {
    let outer = |rect: &Rect| match side {
        Side::Right => rect.max_x(),
        Side::Left => -rect.min_x(),
        Side::Bottom => rect.max_y(),
        Side::Top => -rect.min_y(),
    };
    let extreme = displays.iter().map(outer).fold(f64::NEG_INFINITY, f64::max);
    let candidates = displays.iter().filter(|rect| (outer(rect) - extreme).abs() < 0.5);
    let span = |rect: &Rect| match side {
        Side::Left | Side::Right => (rect.min_y(), rect.max_y()),
        Side::Top | Side::Bottom => (rect.min_x(), rect.max_x()),
    };
    match along {
        Some(along) => candidates.min_by(|a, b| {
            let distance = |rect: &&Rect| {
                let (start, end) = span(rect);
                (start - along).max(0.0).max(along - (end - 1.0))
            };
            distance(a).total_cmp(&distance(b))
        }),
        None => candidates.min_by(|a, b| span(a).0.total_cmp(&span(b).0)),
    }
    .copied()
}

/// How far the cursor is from the exit edge, so a fresh arrival must move inward before the
/// same edge can send it back; without this a twitch at the edge bounces between machines.
fn distance_from_edge(displays: &[Rect], exit: Side, cursor: Point) -> f64 {
    let along = match exit {
        Side::Left | Side::Right => cursor.y,
        Side::Top | Side::Bottom => cursor.x,
    };
    let Some(edge) = edge_rect(displays, exit, Some(along)) else { return f64::INFINITY };
    match exit {
        Side::Right => edge.max_x() - cursor.x,
        Side::Left => cursor.x - edge.min_x(),
        Side::Bottom => edge.max_y() - cursor.y,
        Side::Top => cursor.y - edge.min_y(),
    }
}

/// Decides whether the cursor has left through the exit edge, wandered off the allowed displays
/// (and must be pulled back), or is fine where it is.
fn edge_decision(displays: &[Rect], exit: Side, cursor: Point, pushing: Option<(f32, f32)>, inset: f64) -> EdgeDecision {
    if displays.is_empty() {
        return EdgeDecision::Stay;
    }
    let along = match exit {
        Side::Left | Side::Right => cursor.y,
        Side::Top | Side::Bottom => cursor.x,
    };
    let Some(edge) = edge_rect(displays, exit, Some(along)) else { return EdgeDecision::Stay };
    let pushing_out = pushing.is_some_and(|(dx, dy)| match exit {
        Side::Right => dx > 0.0,
        Side::Left => dx < 0.0,
        Side::Bottom => dy > 0.0,
        Side::Top => dy < 0.0,
    });
    let beyond = match exit {
        Side::Right => cursor.x >= edge.max_x() || (pushing_out && cursor.x >= edge.max_x() - 1.5),
        Side::Left => cursor.x < edge.min_x() || (pushing_out && cursor.x < edge.min_x() + 1.5),
        Side::Bottom => cursor.y >= edge.max_y() || (pushing_out && cursor.y >= edge.max_y() - 1.5),
        Side::Top => cursor.y < edge.min_y() || (pushing_out && cursor.y < edge.min_y() + 1.5),
    };
    let (start, length) = match exit {
        Side::Left | Side::Right => (edge.min_y(), edge.size.y),
        Side::Top | Side::Bottom => (edge.min_x(), edge.size.x),
    };
    let within_span = along >= start && along < start + length;
    if beyond && within_span {
        let frac = ((along - start) / (length - 1.0).max(1.0)).clamp(0.0, 1.0) as f32;
        let park = edge.clamp(Point {
            x: match exit {
                Side::Right => edge.max_x() - inset,
                Side::Left => edge.min_x() + inset,
                _ => cursor.x,
            },
            y: match exit {
                Side::Bottom => edge.max_y() - inset,
                Side::Top => edge.min_y() + inset,
                _ => cursor.y,
            },
        });
        return EdgeDecision::Exit { frac, park };
    }
    if displays.iter().any(|rect| rect.contains(cursor)) {
        return EdgeDecision::Stay;
    }
    let nearest = displays
        .iter()
        .min_by(|a, b| a.distance(cursor).total_cmp(&b.distance(cursor)))
        .expect("displays is not empty");
    EdgeDecision::Clamp(nearest.clamp(cursor))
}

async fn serve_clipboard(mut stream: TcpStream, cipher: Rc<Cipher>, gate: Rc<RefCell<Gate>>) {
    let result: Result<()> = async {
        stream.set_nodelay(true)?;
        let request = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await??;
        let envelope = cipher
            .open(&request)
            .and_then(|plain| Envelope::decode(&plain))
            .context("unauthenticated clipboard request")?;
        if !matches!(envelope.msg, Msg::ClipPut { .. } | Msg::ClipGet { .. }) {
            bail!("unexpected clipboard request");
        }
        {
            let mut gate = gate.borrow_mut();
            if Some(envelope.token) != gate.token || envelope.counter <= gate.tcp_counter {
                bail!("stale clipboard request");
            }
            gate.tcp_counter = envelope.counter;
        }
        let (token, echo) = (envelope.token, envelope.counter);
        let reply = match envelope.msg {
            Msg::ClipPut { mime, data } => {
                let content = (mime, data);
                pasteboard::write(&content).await?;
                tracing::info!("clipboard to the Mac: {} ({} bytes)", content.0, content.1.len());
                Msg::ClipNone { echo }
            }
            Msg::ClipGet { have } => match pasteboard::read().await? {
                Some(content) if digest(&content) != have => Msg::ClipData { echo, mime: content.0, data: content.1 },
                _ => Msg::ClipNone { echo },
            },
            _ => unreachable!("checked above"),
        };
        let counter = gate.borrow_mut().counter.next();
        let frame = cipher.seal(&Envelope { token, counter, msg: reply }.encode());
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await?;
        stream.write_all(&frame).await?;
        Ok(())
    }
    .await;
    gate.borrow_mut().clipboard_busy = false;
    if let Err(error) = result {
        tracing::warn!("clipboard: {error:#}");
    }
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_CLIPBOARD + 1024 {
        bail!("oversized clipboard frame");
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

fn digest(content: &pasteboard::Content) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(content.0.as_bytes());
    hasher.update([0]);
    hasher.update(&content.1);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { origin: Point { x, y }, size: Point { x: w, y: h } }
    }

    #[test]
    fn crossing_onto_a_display_beyond_exits() {
        let builtin = [rect(0.0, 0.0, 1512.0, 982.0)];
        let decision = edge_decision(&builtin, Side::Right, Point { x: 1520.0, y: 491.0 }, Some((3.0, 0.0)), 2.0);
        match decision {
            EdgeDecision::Exit { frac, park } => {
                assert!((frac - 0.5).abs() < 0.01);
                assert_eq!(park, Point { x: 1510.0, y: 491.0 });
            }
            other => panic!("expected exit, got {other:?}"),
        }
    }

    #[test]
    fn pushing_against_a_bare_edge_exits() {
        let builtin = [rect(0.0, 0.0, 1512.0, 982.0)];
        let pinned = Point { x: 1511.0, y: 100.0 };
        assert!(matches!(edge_decision(&builtin, Side::Right, pinned, Some((2.0, 0.0)), 2.0), EdgeDecision::Exit { .. }));
        assert_eq!(edge_decision(&builtin, Side::Right, pinned, Some((-2.0, 0.0)), 2.0), EdgeDecision::Stay);
        assert_eq!(edge_decision(&builtin, Side::Right, pinned, None, 2.0), EdgeDecision::Stay);
    }

    #[test]
    fn wandering_onto_a_hidden_display_is_pulled_back() {
        let builtin = [rect(0.0, 0.0, 1512.0, 982.0)];
        let decision = edge_decision(&builtin, Side::Right, Point { x: -40.0, y: 300.0 }, Some((-5.0, 0.0)), 2.0);
        assert_eq!(decision, EdgeDecision::Clamp(Point { x: 0.0, y: 300.0 }));
    }

    #[test]
    fn exit_uses_the_outermost_display() {
        let displays = [rect(0.0, 0.0, 1512.0, 982.0), rect(1512.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(edge_decision(&displays, Side::Right, Point { x: 1600.0, y: 10.0 }, Some((1.0, 0.0)), 2.0), EdgeDecision::Stay);
        assert!(matches!(
            edge_decision(&displays, Side::Right, Point { x: 3432.0, y: 10.0 }, Some((1.0, 0.0)), 2.0),
            EdgeDecision::Exit { .. }
        ));
    }

    #[test]
    fn left_and_right_edges_behave_alike() {
        let builtin = [rect(0.0, 0.0, 1512.0, 982.0)];
        let left_pinned = Point { x: 0.0, y: 100.0 };
        assert_eq!(edge_decision(&builtin, Side::Left, left_pinned, None, 2.0), EdgeDecision::Stay);
        assert!(matches!(
            edge_decision(&builtin, Side::Left, left_pinned, Some((-2.0, 0.0)), 2.0),
            EdgeDecision::Exit { .. }
        ));
        assert!(matches!(
            edge_decision(&builtin, Side::Left, Point { x: -10.0, y: 100.0 }, None, 2.0),
            EdgeDecision::Exit { .. }
        ));
        let top_pinned = Point { x: 700.0, y: 0.0 };
        assert_eq!(edge_decision(&builtin, Side::Top, top_pinned, None, 2.0), EdgeDecision::Stay);
    }

    #[test]
    fn ratio_ignores_noise_and_follows_real_shifts() {
        let noisy = [0.69, 0.74, 0.37, 0.66, 0.71, 0.62, 0.70];
        assert_eq!(shifted_ratio(&noisy, 0.6875), None);
        assert_eq!(shifted_ratio(&noisy[..6], 0.229), None);
        let profile_applied = [0.23, 0.229, 0.4, 0.228, 0.231, 0.7, 0.229];
        assert_eq!(shifted_ratio(&profile_applied, 0.6875), Some(0.23));
    }
}
