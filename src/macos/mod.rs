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
const DISPLAY_REFRESH: Duration = Duration::from_secs(1);
const EXIT_RESEND: Duration = Duration::from_millis(30);
const PUSH_MEMORY: Duration = Duration::from_millis(60);
const CALIBRATION_COUNTS: i8 = 60;
const PARKING_LIMIT: Duration = Duration::from_millis(500);

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
    synced: Option<[u8; 32]>,
}

struct Receiver {
    socket: Rc<UdpSocket>,
    cipher: Rc<Cipher>,
    gate: Rc<RefCell<Gate>>,
    confine: Confine,
    udp_counter: u64,
    last_heard: Instant,
    last_reset: Instant,
    entered: bool,
    exiting: Option<(f32, Instant)>,
    exit_side: Side,
    speed: f32,
    keys: KeyState,
    buttons: u32,
    remainder: (f32, f32),
    wheel: (i32, i32),
    pushed: Option<(f32, f32, Instant)>,
    points_per_count: Option<f64>,
    last_calibration: Option<Instant>,
    pointer_was_ready: bool,
    window: Option<Window>,
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
        synced: None,
    }));
    let mut receiver = Receiver {
        socket,
        cipher: cipher.clone(),
        gate: gate.clone(),
        confine: config.confine,
        udp_counter: 0,
        last_heard: Instant::now(),
        last_reset: Instant::now() - Duration::from_secs(1),
        entered: false,
        exiting: None,
        exit_side: Side::Right,
        speed: 1.0,
        keys: KeyState::default(),
        buttons: 0,
        remainder: (0.0, 0.0),
        wheel: (0, 0),
        pushed: None,
        points_per_count: None,
        last_calibration: None,
        pointer_was_ready: false,
        window: None,
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
                Ok((length, from)) => receiver.on_datagram(&buffer[..length], from).await,
                Err(error) => tracing::debug!("udp receive: {error}"),
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, from)) => {
                    let allowed = gate.borrow().peer.map(|peer| peer.ip()) == Some(from.ip());
                    if allowed {
                        tokio::task::spawn_local(serve_clipboard(stream, cipher.clone(), gate.clone()));
                    }
                }
                Err(error) => tracing::debug!("tcp accept: {error}"),
            },
            _ = tokio::time::sleep_until(next_tick) => {
                receiver.tick().await;
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
        if let Msg::Hello { nonce } = envelope.msg {
            let token = random_u64();
            {
                let mut gate = self.gate.borrow_mut();
                gate.token = Some(token);
                gate.peer = Some(from);
                gate.tcp_counter = 0;
            }
            self.udp_counter = envelope.counter;
            self.last_heard = Instant::now();
            self.leave("new session");
            tracing::info!("session with {from}");
            self.reply(from, 0, Msg::Welcome { nonce, token }).await;
            return;
        }
        let (token, peer) = {
            let gate = self.gate.borrow();
            (gate.token, gate.peer)
        };
        if Some(envelope.token) != token || Some(from) != peer {
            if self.last_reset.elapsed() > Duration::from_millis(200) {
                self.last_reset = Instant::now();
                self.reply(from, 0, Msg::Reset).await;
            }
            return;
        }
        if envelope.counter <= self.udp_counter {
            return;
        }
        self.udp_counter = envelope.counter;
        self.last_heard = Instant::now();
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
            Msg::Enter { side, frac, speed } => {
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

    /// Refines the count-to-point ratio from real use: over each tenth of a second, compares
    /// how far the cursor actually went with how many counts were posted. This tracks changes
    /// to the Mac's tracking speed, and LinearMouse settling, without moving the cursor itself.
    fn learn_ratio(&mut self, cursor: Point) {
        const MARGIN: f64 = 24.0;
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
        if posted >= 40.0 && inside(window.start) && inside(cursor) {
            let sample = moved / posted;
            if (0.05..=10.0).contains(&sample) {
                let ratio = self.points_per_count.map_or(sample, |ratio| ratio * 0.7 + sample * 0.3);
                if self.points_per_count.is_some_and(|old| (old - ratio).abs() / old > 0.05) {
                    tracing::info!("virtual mouse now moves {ratio:.3} points per count");
                }
                self.points_per_count = Some(ratio);
            }
        }
        self.window = Some(Window { start: cursor, opened: Instant::now(), posted: (0.0, 0.0) });
    }

    /// Posts one relative move toward `target`, sized with the calibrated count-to-point ratio.
    /// Warping is avoided on purpose: macOS drops hardware mouse input for a quarter second after one.
    fn nudge_to(&self, target: Point) -> bool {
        let (Some(ratio), Some(now)) = (self.points_per_count, display::cursor()) else { return true };
        let (dx, dy) = (target.x - now.x, target.y - now.y);
        if dx.abs() < 1.0 && dy.abs() < 1.0 {
            return true;
        }
        let (mut x, mut y) = ((dx / ratio).round() as i32, (dy / ratio).round() as i32);
        post_motion(self.buttons, &mut x, &mut y, &mut 0, &mut 0);
        false
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

    async fn tick(&mut self) {
        self.settle_parking();
        if self.entered && self.last_heard.elapsed() > SILENCE_LIMIT {
            self.leave("the desktop went quiet");
        }
        let ready = vhid::pointing_ready();
        if ready != self.pointer_was_ready {
            self.pointer_was_ready = ready;
            self.points_per_count = None;
            self.last_calibration = None;
        }
        if !self.entered {
            let due = self.last_calibration.is_none_or(|at| at.elapsed() > Duration::from_secs(30));
            if self.points_per_count.is_none() && due && ready {
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
        let pushing = self.pushed.filter(|(_, _, at)| at.elapsed() < PUSH_MEMORY).map(|(dx, dy, _)| (dx, dy));
        match edge_decision(&self.displays, self.exit_side, cursor, pushing) {
            EdgeDecision::Stay => {}
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
            }
        }
    }

    fn refresh_displays(&mut self, force: bool) {
        if !force && self.displays_at.is_some_and(|at| at.elapsed() < DISPLAY_REFRESH) {
            return;
        }
        self.displays_at = Some(Instant::now());
        let online = display::online();
        let builtin: Vec<Rect> = online.iter().filter(|d| d.builtin).map(|d| d.bounds).collect();
        self.displays = match self.confine {
            Confine::Builtin if !builtin.is_empty() => builtin,
            _ => online.iter().map(|d| d.bounds).collect(),
        };
    }

    fn entry_point(&self, frac: f32) -> Option<Point> {
        let rect = edge_rect(&self.displays, self.exit_side, None)?;
        let frac = frac.clamp(0.0, 1.0) as f64;
        Some(match self.exit_side {
            Side::Right => Point { x: rect.max_x() - 2.0, y: rect.min_y() + frac * (rect.size.y - 1.0) },
            Side::Left => Point { x: rect.min_x() + 1.0, y: rect.min_y() + frac * (rect.size.y - 1.0) },
            Side::Bottom => Point { x: rect.min_x() + frac * (rect.size.x - 1.0), y: rect.max_y() - 2.0 },
            Side::Top => Point { x: rect.min_x() + frac * (rect.size.x - 1.0), y: rect.min_y() + 1.0 },
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
        vhid::pointing(0, direction * CALIBRATION_COUNTS, 0, 0, 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let moved = display::cursor().map(|now| (now.x - start.x) * direction as f64);
        vhid::pointing(0, -direction * CALIBRATION_COUNTS, 0, 0, 0);
        match moved {
            Some(moved) if moved > 1.0 => {
                let ratio = moved / CALIBRATION_COUNTS as f64;
                tracing::info!("virtual mouse moves {ratio:.3} points per count");
                self.points_per_count = Some(ratio);
            }
            other => tracing::warn!("calibration saw no movement ({other:?}); retrying later"),
        }
    }
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

/// Decides whether the cursor has left through the exit edge, wandered off the allowed displays
/// (and must be pulled back), or is fine where it is.
fn edge_decision(displays: &[Rect], exit: Side, cursor: Point, pushing: Option<(f32, f32)>) -> EdgeDecision {
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
        Side::Right => cursor.x >= edge.max_x() - 0.5 || (pushing_out && cursor.x >= edge.max_x() - 1.5),
        Side::Left => cursor.x < edge.min_x() + 0.5 || (pushing_out && cursor.x < edge.min_x() + 1.5),
        Side::Bottom => cursor.y >= edge.max_y() - 0.5 || (pushing_out && cursor.y >= edge.max_y() - 1.5),
        Side::Top => cursor.y < edge.min_y() + 0.5 || (pushing_out && cursor.y < edge.min_y() + 1.5),
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
                Side::Right => edge.max_x() - 2.0,
                Side::Left => edge.min_x() + 1.0,
                _ => cursor.x,
            },
            y: match exit {
                Side::Bottom => edge.max_y() - 2.0,
                Side::Top => edge.min_y() + 1.0,
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
        let token = {
            let mut gate = gate.borrow_mut();
            if Some(envelope.token) != gate.token || envelope.counter <= gate.tcp_counter {
                bail!("stale clipboard request");
            }
            gate.tcp_counter = envelope.counter;
            envelope.token
        };
        let reply = match envelope.msg {
            Msg::ClipPut { mime, data } => {
                let content = (mime, data);
                pasteboard::write(&content).await?;
                gate.borrow_mut().synced = Some(digest(&content));
                tracing::info!("clipboard to the Mac: {} ({} bytes)", content.0, content.1.len());
                Msg::ClipNone
            }
            Msg::ClipGet => match pasteboard::read().await? {
                Some(content) if gate.borrow().synced != Some(digest(&content)) => {
                    gate.borrow_mut().synced = Some(digest(&content));
                    Msg::ClipData { mime: content.0, data: content.1 }
                }
                _ => Msg::ClipNone,
            },
            _ => bail!("unexpected clipboard request"),
        };
        let counter = gate.borrow_mut().counter.next();
        let frame = cipher.seal(&Envelope { token, counter, msg: reply }.encode());
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await?;
        stream.write_all(&frame).await?;
        Ok(())
    }
    .await;
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
        let decision = edge_decision(&builtin, Side::Right, Point { x: 1520.0, y: 491.0 }, Some((3.0, 0.0)));
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
        assert!(matches!(edge_decision(&builtin, Side::Right, pinned, Some((2.0, 0.0))), EdgeDecision::Exit { .. }));
        assert_eq!(edge_decision(&builtin, Side::Right, pinned, Some((-2.0, 0.0))), EdgeDecision::Stay);
        assert_eq!(edge_decision(&builtin, Side::Right, pinned, None), EdgeDecision::Stay);
    }

    #[test]
    fn wandering_onto_a_hidden_display_is_pulled_back() {
        let builtin = [rect(0.0, 0.0, 1512.0, 982.0)];
        let decision = edge_decision(&builtin, Side::Right, Point { x: -40.0, y: 300.0 }, Some((-5.0, 0.0)));
        assert_eq!(decision, EdgeDecision::Clamp(Point { x: 0.0, y: 300.0 }));
    }

    #[test]
    fn exit_uses_the_outermost_display() {
        let displays = [rect(0.0, 0.0, 1512.0, 982.0), rect(1512.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(edge_decision(&displays, Side::Right, Point { x: 1600.0, y: 10.0 }, Some((1.0, 0.0))), EdgeDecision::Stay);
        assert!(matches!(
            edge_decision(&displays, Side::Right, Point { x: 3432.0, y: 10.0 }, Some((1.0, 0.0))),
            EdgeDecision::Exit { .. }
        ));
    }
}
