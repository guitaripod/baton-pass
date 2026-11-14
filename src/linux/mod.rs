pub mod clipboard;
pub mod control;
pub mod install;
mod keymap;
mod link;
mod portal;
pub mod probe;
mod slam;

use crate::config::{Config, Side};
use crate::crypto::{Cipher, Counter};
use crate::proto::{Envelope, Msg};
use anyhow::Result;
use ashpd::desktop::input_capture::Activated;
use control::Request;
use futures_util::StreamExt;
use keymap::Hotkey;
use link::{Incoming, Link};
use portal::{Edge, Ei, Portal, PortalSignals};
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState as EiKeyState;
use reis::event::EiEvent;
use std::collections::BTreeSet;
use std::future::pending;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};

const TICK: Duration = Duration::from_millis(25);
const HEARTBEAT_CAPTURED: Duration = Duration::from_millis(100);
const HEARTBEAT_IDLE: Duration = Duration::from_secs(1);
const FAILSAFE: Duration = Duration::from_millis(750);
const TOGGLE_WINDOW: Duration = Duration::from_millis(1500);
const REARM_DELAY: Duration = Duration::from_millis(400);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const REOPEN_DELAY: Duration = Duration::from_secs(2);
const SLAM_RETRY: Duration = Duration::from_millis(250);
const RETURN_INSET: f64 = 48.0;
const BOUNCE_GUARD: Duration = Duration::from_millis(400);

struct Capture {
    activation_id: Option<u32>,
    edge: Edge,
    entry: (f64, f64),
    return_to: Option<(f64, f64)>,
    via_hotkey: bool,
    since: Instant,
    pressed: BTreeSet<u32>,
    buttons: u32,
    motion: (f32, f32),
    wheel: (f32, f32),
    dirty: bool,
}

enum ClipJob {
    Push { peer: SocketAddr, token: u64 },
    Pull { peer: SocketAddr, token: u64 },
    Flush(oneshot::Sender<()>),
}

/// A hotkey switch in progress: where the cursor was, and the shoves still to try if the
/// previous one did not reach the barrier.
struct Toggle {
    position: Option<(f64, f64)>,
    started: Instant,
    plans: Vec<Option<(i32, i32)>>,
    next_attempt: Instant,
}

struct Daemon {
    config: Config,
    hotkey: Hotkey,
    link: Link,
    portal: Option<Portal>,
    capture: Option<Capture>,
    pending_toggle: Option<Toggle>,
    slammer: Option<slam::Slammer>,
    bus: zbus::Connection,
    clip_jobs: mpsc::UnboundedSender<ClipJob>,
    rearm_at: Option<Instant>,
    reopen_at: Option<Instant>,
    last_heartbeat: Instant,
    portal_error: Option<String>,
    opening: bool,
    opened: mpsc::UnboundedSender<Result<(Portal, Ei, PortalSignals)>>,
    session_lost: Option<String>,
    last_release: Option<Instant>,
}

pub async fn run(config: Config) -> Result<()> {
    let cipher = Rc::new(Cipher::load(&Config::key_path())?);
    let hotkey = Hotkey::parse(&config.hotkey)?;
    if config.peers.is_empty() {
        anyhow::bail!("no peers configured in {}", Config::path().display());
    }
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let bus = control::serve(requests_tx).await?;
    let slammer = slam::Slammer::new()
        .inspect_err(|error| tracing::warn!("hotkey pointer unavailable (/dev/uinput): {error}"))
        .ok();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(clipboard_worker(cipher.clone(), clip_rx));
    let link = Link::new(cipher, config.peer_addresses()).await?;
    let (opened_tx, mut opened) = mpsc::unbounded_channel();

    let mut daemon = Daemon {
        config,
        hotkey,
        link,
        portal: None,
        capture: None,
        pending_toggle: None,
        slammer,
        bus,
        clip_jobs: clip_tx,
        rearm_at: None,
        reopen_at: None,
        last_heartbeat: Instant::now(),
        portal_error: None,
        opening: false,
        opened: opened_tx,
        session_lost: None,
        last_release: None,
    };
    let mut ei: Option<Ei> = None;
    let mut streams = Streams::default();
    daemon.start_opening();

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buffer = vec![0u8; 2048];

    tracing::info!("ferry {} running; hotkey {}", env!("CARGO_PKG_VERSION"), daemon.hotkey.qt_name());
    loop {
        tokio::select! {
            received = daemon.link.recv(&mut buffer) => match received {
                Ok((length, from)) => daemon.on_datagram(&buffer[..length], from).await,
                Err(error) => tracing::debug!("udp receive: {error}"),
            },
            event = next_ei(&mut ei) => match event {
                Some(Ok(event)) => {
                    if let Some(ei) = &ei {
                        ei.bind_seat(&event);
                    }
                    daemon.on_ei(event).await;
                }
                other => daemon.session_lost = Some(format!("the input capture connection ended: {other:?}")),
            },
            activated = next_signal(&mut streams.activated) => match activated {
                Some(activated) => daemon.on_activated(activated).await,
                None => daemon.session_lost = Some("portal signals stopped".into()),
            },
            deactivated = next_signal(&mut streams.deactivated) => match deactivated {
                Some(deactivated) => daemon.on_deactivated(deactivated.activation_id()).await,
                None => daemon.session_lost = Some("portal signals stopped".into()),
            },
            disabled = next_signal(&mut streams.disabled) => match disabled {
                Some(_) => {
                    daemon.end_capture("capture disabled").await;
                    if let Some(portal) = daemon.portal.as_mut() {
                        portal.armed = false;
                    }
                    daemon.rearm_at = Some(Instant::now() + REARM_DELAY);
                }
                None => daemon.session_lost = Some("portal signals stopped".into()),
            },
            zones = next_signal(&mut streams.zones_changed) => match zones {
                Some(_) => {
                    tracing::info!("screens changed; re-placing the barrier");
                    daemon.rearm_at = Some(Instant::now() + REARM_DELAY);
                }
                None => daemon.session_lost = Some("portal signals stopped".into()),
            },
            result = opened.recv() => {
                daemon.opening = false;
                match result {
                    Some(Ok((portal, opened_ei, signals))) => {
                        daemon.portal = Some(portal);
                        daemon.portal_error = None;
                        daemon.rearm_at = Some(Instant::now());
                        ei = Some(opened_ei);
                        streams = Streams::from(Some(signals));
                        tracing::info!("input capture session ready");
                    }
                    Some(Err(error)) => {
                        tracing::error!("input capture unavailable: {error:#}");
                        if !was_declined(&error) {
                            daemon.reopen_at = Some(Instant::now() + RETRY_DELAY * 6);
                        }
                        daemon.portal_error = Some(format!("{error:#}"));
                    }
                    None => {}
                }
            }
            request = requests.recv() => match request {
                Some(Request::Toggle(position)) => {
                    if daemon.portal.is_none() {
                        daemon.start_opening();
                    }
                    daemon.on_toggle(position).await;
                }
                Some(Request::Status(reply)) => {
                    let _ = reply.send(daemon.status());
                }
                None => break,
            },
            _ = ticker.tick() => {
                if daemon.reopen_at.is_some_and(|at| Instant::now() >= at) {
                    daemon.reopen_at = None;
                    daemon.start_opening();
                }
                daemon.tick().await;
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
        if let Some(reason) = daemon.session_lost.take() {
            tracing::warn!("reopening input capture: {reason}");
            daemon.drop_portal().await;
            ei = None;
            streams = Streams::default();
            daemon.reopen_at = Some(Instant::now() + REOPEN_DELAY);
        }
    }
    daemon.shutdown().await;
    Ok(())
}

async fn next_ei(ei: &mut Option<Ei>) -> Option<Result<EiEvent, reis::Error>> {
    match ei {
        Some(ei) => ei.events.next().await,
        None => pending().await,
    }
}

async fn next_signal<T>(signals: &mut Option<portal::Signals<T>>) -> Option<T> {
    match signals {
        Some(signals) => signals.next().await,
        None => pending().await,
    }
}

#[derive(Default)]
struct Streams {
    activated: Option<portal::Signals<Activated>>,
    deactivated: Option<portal::Signals<ashpd::desktop::input_capture::Deactivated>>,
    disabled: Option<portal::Signals<ashpd::desktop::input_capture::Disabled>>,
    zones_changed: Option<portal::Signals<ashpd::desktop::input_capture::ZonesChanged>>,
}

impl From<Option<PortalSignals>> for Streams {
    fn from(signals: Option<PortalSignals>) -> Self {
        match signals {
            Some(signals) => Self {
                activated: Some(signals.activated),
                deactivated: Some(signals.deactivated),
                disabled: Some(signals.disabled),
                zones_changed: Some(signals.zones_changed),
            },
            None => Self::default(),
        }
    }
}

impl Daemon {
    /// Asks the portal for a capture session in the background: the first time, KDE shows a
    /// permission dialog, and the daemon must keep answering status calls while it waits.
    fn start_opening(&mut self) {
        if self.opening || self.portal.is_some() {
            return;
        }
        self.opening = true;
        let opened = self.opened.clone();
        tokio::task::spawn_local(async move {
            let _ = opened.send(Portal::open(install::APP_ID).await);
        });
    }

    async fn drop_portal(&mut self) {
        self.end_capture("input capture connection ended").await;
        if let Some(portal) = self.portal.take() {
            portal.close().await;
        }
    }

    async fn on_datagram(&mut self, packet: &[u8], from: SocketAddr) {
        match self.link.accept(packet, from).await {
            Some(Incoming::Connected) => {
                self.rearm_at = Some(Instant::now());
                self.last_heartbeat = Instant::now() - HEARTBEAT_IDLE;
            }
            Some(Incoming::Message(Msg::Ack { flags })) => self.link.flags = flags,
            Some(Incoming::Message(Msg::Exit { frac })) => {
                if let Some(capture) = &self.capture {
                    let target = portal::edge_point(capture.edge.region, self.config.side, frac, RETURN_INSET);
                    self.release(target, "the cursor left the Mac's edge").await;
                }
            }
            _ => {}
        }
    }

    async fn on_activated(&mut self, activated: Activated) {
        if self.capture.is_some() {
            return;
        }
        let Some(portal) = &self.portal else { return };
        let Some(edge) = portal.edge_for(&activated).or_else(|| portal.edges().first().copied()) else {
            return;
        };
        let side = self.config.side;
        let entry = activated
            .cursor_position()
            .map(|(x, y)| (x as f64, y as f64))
            .unwrap_or_else(|| portal::edge_point(edge.region, side, 0.5, 1.0));
        let toggled = self.pending_toggle.take().filter(|toggle| toggle.started.elapsed() < TOGGLE_WINDOW);
        let just_returned = self.last_release.is_some_and(|at| at.elapsed() < BOUNCE_GUARD);
        if toggled.is_none() && (!self.config.edge || just_returned) {
            let inset = if just_returned { RETURN_INSET } else { 1.0 };
            let back = portal::edge_point(edge.region, side, portal::edge_fraction(edge.region, side, entry.0, entry.1), inset);
            if let Err(error) = portal.release(activated.activation_id(), back).await {
                self.session_lost = Some(format!("releasing an edge crossing failed: {error:#}"));
            }
            return;
        }
        let mut capture = Capture {
            activation_id: activated.activation_id(),
            edge,
            entry,
            return_to: toggled.as_ref().and_then(|toggle| toggle.position),
            via_hotkey: toggled.is_some(),
            since: Instant::now(),
            pressed: BTreeSet::new(),
            buttons: 0,
            motion: (0.0, 0.0),
            wheel: (0.0, 0.0),
            dirty: false,
        };
        if !self.link.connected() {
            capture.return_to = None;
            self.capture = Some(capture);
            let back = portal::edge_point(edge.region, side, portal::edge_fraction(edge.region, side, entry.0, entry.1), 1.0);
            self.release(back, "the Mac is not linked").await;
            return;
        }
        let frac = if capture.via_hotkey {
            f32::NAN
        } else {
            portal::edge_fraction(edge.region, side, entry.0, entry.1)
        };
        tracing::info!("captured at {entry:?}{}", if capture.via_hotkey { " by hotkey" } else { "" });
        self.capture = Some(capture);
        self.link.send(Msg::Enter { side: side.to_wire(), frac, speed: self.config.speed, exit: self.config.edge }).await;
        self.heartbeat().await;
        self.queue_clipboard(true);
    }

    async fn on_ei(&mut self, event: EiEvent) {
        let natural = self.config.natural_scroll;
        let Some(capture) = self.capture.as_mut() else { return };
        match event {
            EiEvent::PointerMotion(motion) => {
                capture.motion.0 += motion.dx;
                capture.motion.1 += motion.dy;
                capture.dirty = true;
            }
            EiEvent::Button(button) => {
                if let Some(bit) = keymap::button_bit(button.button) {
                    if button.state == ButtonState::Press {
                        capture.buttons |= bit;
                    } else {
                        capture.buttons &= !bit;
                    }
                    capture.dirty = true;
                }
            }
            EiEvent::ScrollDiscrete(scroll) => {
                let (horizontal, vertical) = wheel_to_hid(scroll.discrete_dx as f32, scroll.discrete_dy as f32, natural);
                capture.wheel.0 += vertical;
                capture.wheel.1 += horizontal;
                capture.dirty = true;
            }
            EiEvent::ScrollDelta(scroll) => {
                let (horizontal, vertical) = wheel_to_hid(scroll.dx * 8.0, scroll.dy * 8.0, natural);
                capture.wheel.0 += vertical;
                capture.wheel.1 += horizontal;
                capture.dirty = true;
            }
            EiEvent::Frame(_) => self.flush_pointer().await,
            EiEvent::KeyboardKey(key) => {
                let pressed = key.state == EiKeyState::Press;
                if pressed && self.hotkey.matches(key.key, &capture.pressed) {
                    let target = capture.return_to;
                    let target = target.unwrap_or_else(|| self.return_point());
                    self.release(target, "hotkey").await;
                    return;
                }
                let changed = if pressed { capture.pressed.insert(key.key) } else { capture.pressed.remove(&key.key) };
                if changed {
                    let state = keymap::key_state(&capture.pressed, self.config.swap_alt_meta);
                    self.link.send(Msg::Keys(state)).await;
                }
            }
            EiEvent::Disconnected(disconnected) => tracing::warn!("ei disconnected: {:?}", disconnected.reason),
            _ => {}
        }
    }

    async fn flush_pointer(&mut self) {
        let Some(capture) = self.capture.as_mut() else { return };
        if !capture.dirty {
            return;
        }
        let wheel_v = capture.wheel.0.trunc();
        let wheel_h = capture.wheel.1.trunc();
        let msg = Msg::Pointer {
            dx: capture.motion.0,
            dy: capture.motion.1,
            wheel_v: wheel_v as i32,
            wheel_h: wheel_h as i32,
            buttons: capture.buttons,
        };
        capture.motion = (0.0, 0.0);
        capture.wheel = (capture.wheel.0 - wheel_v, capture.wheel.1 - wheel_h);
        capture.dirty = false;
        self.link.send(msg).await;
    }

    /// Where the cursor returns when the capture ends without an edge crossing on the Mac.
    fn return_point(&self) -> (f64, f64) {
        let Some(capture) = &self.capture else { return (0.0, 0.0) };
        let side = self.config.side;
        let frac = portal::edge_fraction(capture.edge.region, side, capture.entry.0, capture.entry.1);
        portal::edge_point(capture.edge.region, side, frac, RETURN_INSET)
    }

    async fn release(&mut self, target: (f64, f64), reason: &str) {
        let Some(capture) = self.capture.take() else { return };
        self.last_release = Some(Instant::now());
        tracing::info!("released ({reason}) after {:.1} s", capture.since.elapsed().as_secs_f32());
        self.link.send(Msg::Leave).await;
        if let Some(portal) = &self.portal
            && let Err(error) = portal.release(capture.activation_id, target).await
        {
            self.session_lost = Some(format!("releasing the capture failed: {error:#}"));
        }
        self.queue_clipboard(false);
    }

    async fn end_capture(&mut self, reason: &str) {
        if let Some(capture) = self.capture.take() {
            tracing::info!("capture ended ({reason}) after {:.1} s", capture.since.elapsed().as_secs_f32());
            self.link.send(Msg::Leave).await;
            self.queue_clipboard(false);
        }
    }

    /// Ends the capture the compositor deactivated, ignoring a late signal for an earlier one.
    async fn on_deactivated(&mut self, activation_id: Option<u32>) {
        let current = self.capture.as_ref().and_then(|capture| capture.activation_id);
        if let (Some(ended), Some(current)) = (activation_id, current)
            && ended != current
        {
            return;
        }
        self.end_capture("the compositor ended the capture").await;
    }

    async fn on_toggle(&mut self, position: Option<(f64, f64)>) {
        if self.capture.is_some() {
            let target = self
                .capture
                .as_ref()
                .and_then(|capture| capture.return_to)
                .unwrap_or_else(|| self.return_point());
            self.release(target, "toggle").await;
            return;
        }
        if !self.link.connected() {
            control::notify(&self.bus, "The Mac is not reachable", "ferry keeps looking for it").await;
            return;
        }
        let armed = self.portal.as_ref().is_some_and(|portal| portal.armed);
        if !armed {
            self.arm().await;
        }
        let Some(portal) = self.portal.as_ref().filter(|portal| portal.armed) else {
            control::notify(&self.bus, "Input capture is not ready", self.portal_error.as_deref().unwrap_or("")).await;
            return;
        };
        if self.slammer.is_none() {
            control::notify(&self.bus, "Hotkey needs /dev/uinput", "Move the pointer past the edge instead").await;
            return;
        }
        let plans = slam_plans(self.config.side, portal.edges(), position);
        self.pending_toggle = Some(Toggle { position, started: Instant::now(), plans, next_attempt: Instant::now() });
        self.advance_toggle().await;
    }

    /// Tries the next shove toward the barrier when the previous one has not started a capture.
    async fn advance_toggle(&mut self) {
        let side = self.config.side;
        let Some(toggle) = self.pending_toggle.as_mut() else { return };
        if self.capture.is_some() || Instant::now() < toggle.next_attempt || toggle.plans.is_empty() {
            return;
        }
        let perpendicular = toggle.plans.remove(0);
        toggle.next_attempt = Instant::now() + SLAM_RETRY;
        if let Some(slammer) = self.slammer.as_mut()
            && let Err(error) = slammer.slam(toward(side), perpendicular).await
        {
            tracing::warn!("hotkey pointer: {error}");
        }
    }

    async fn arm(&mut self) {
        let side = self.config.side;
        let Some(portal) = self.portal.as_mut() else { return };
        portal.disarm().await;
        match portal.arm(side).await {
            Ok(()) => self.portal_error = None,
            Err(error) => {
                tracing::warn!("placing the barrier: {error:#}");
                self.portal_error = Some(format!("{error:#}"));
                self.rearm_at = Some(Instant::now() + RETRY_DELAY);
            }
        }
    }

    async fn heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
        let (entered, keys, buttons) = match &self.capture {
            Some(capture) => (true, keymap::key_state(&capture.pressed, self.config.swap_alt_meta), capture.buttons),
            None => (false, Default::default(), 0),
        };
        self.link.send(Msg::Heartbeat { entered, keys, buttons }).await;
    }

    async fn tick(&mut self) {
        self.link.maintain().await;
        let connected = self.link.connected();

        let silent = self.capture.as_ref().is_some_and(|capture| self.link.last_heard.max(capture.since).elapsed() > FAILSAFE);
        if self.capture.is_some() && (!connected || silent) {
            let target = self
                .capture
                .as_ref()
                .and_then(|capture| capture.return_to)
                .unwrap_or_else(|| self.return_point());
            self.release(target, "the Mac stopped answering").await;
            control::notify(&self.bus, "Lost the Mac", "Mouse and keyboard are back on this desktop").await;
        }

        if !connected
            && let Some(portal) = self.portal.as_mut()
            && portal.armed
        {
            portal.disarm().await;
            self.rearm_at = Some(Instant::now());
        }
        if connected && self.config.edge && self.capture.is_none() && self.rearm_at.is_some_and(|at| Instant::now() >= at) {
            self.rearm_at = None;
            self.arm().await;
        }
        let needed = self.config.edge || self.capture.is_some() || self.pending_toggle.is_some();
        if !needed
            && let Some(portal) = self.portal.as_mut()
            && portal.armed
        {
            portal.disarm().await;
        }

        self.advance_toggle().await;
        if self.pending_toggle.as_ref().is_some_and(|toggle| toggle.started.elapsed() > TOGGLE_WINDOW) {
            self.pending_toggle = None;
            control::notify(&self.bus, "Could not reach the edge", "Move the pointer past the edge instead").await;
        }

        let interval = if self.capture.is_some() { HEARTBEAT_CAPTURED } else { HEARTBEAT_IDLE };
        if connected && self.last_heartbeat.elapsed() >= interval {
            self.heartbeat().await;
        }
    }

    /// Pushes this desktop's clipboard to the Mac on the way over, or pulls the Mac's on the way back.
    fn queue_clipboard(&self, push: bool) {
        if !self.config.clipboard {
            return;
        }
        if let (Some(peer), Some(token)) = (self.link.peer, self.link.token) {
            let job = if push { ClipJob::Push { peer, token } } else { ClipJob::Pull { peer, token } };
            let _ = self.clip_jobs.send(job);
        }
    }

    fn status(&self) -> String {
        let mut lines = Vec::new();
        lines.push(match (self.link.token, self.link.peer) {
            (Some(_), Some(peer)) => format!(
                "link: connected to {peer}, last reply {} ms ago",
                self.link.last_heard.elapsed().as_millis()
            ),
            _ => format!("link: looking for {}", self.config.peer_addresses().join(", ")),
        });
        if self.link.connected() {
            let flags = self.link.flags;
            let mut mac = Vec::new();
            mac.push(if flags & crate::proto::FLAG_KEYBOARD_READY != 0 { "keyboard ready" } else { "keyboard NOT ready" });
            mac.push(if flags & crate::proto::FLAG_POINTING_READY != 0 { "pointer ready" } else { "pointer NOT ready" });
            if flags & crate::proto::FLAG_VERSION_MISMATCH != 0 {
                mac.push("Karabiner driver version mismatch");
            }
            if flags & crate::proto::FLAG_ENTERED != 0 {
                mac.push("in control");
            }
            lines.push(format!("mac: {}", mac.join(", ")));
        }
        lines.push(match (&self.capture, &self.portal) {
            (Some(capture), _) => format!(
                "capture: active for {:.1} s{}",
                capture.since.elapsed().as_secs_f32(),
                if capture.via_hotkey { " (hotkey)" } else { "" }
            ),
            (None, Some(_)) if !self.config.edge => format!("capture: hotkey only ({}), edge off", self.hotkey.qt_name()),
            (None, Some(portal)) if portal.armed => format!(
                "capture: armed on the {:?} edge ({} barrier(s)), hotkey {}",
                self.config.side,
                portal.edges().len(),
                self.hotkey.qt_name()
            ),
            (None, Some(_)) => "capture: not armed".into(),
            (None, None) if self.opening => "capture: waiting for the input capture permission dialog".into(),
            (None, None) => "capture: no portal session".into(),
        });
        if let Some(error) = &self.portal_error {
            lines.push(format!("last capture error: {error}"));
        }
        lines.join("\n")
    }

    async fn shutdown(&mut self) {
        if self.capture.is_some() {
            let target = self.return_point();
            self.release(target, "shutting down").await;
            let (done, finished) = oneshot::channel();
            if self.clip_jobs.send(ClipJob::Flush(done)).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(6), finished).await;
            }
        }
        if let Some(portal) = &self.portal {
            portal.close().await;
        }
    }
}

/// True when the user pressed Deny in KDE's input capture dialog, which must not be re-asked unprompted.
fn was_declined(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ashpd::Error>(),
            Some(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled))
        )
    })
}

/// Converts Wayland wheel motion into HID wheel units: positive vertical means away from the
/// user and positive horizontal means right, as the physical wheel turned before natural scrolling.
fn wheel_to_hid(dx: f32, dy: f32, natural: bool) -> (f32, f32) {
    if natural { (-dx, dy) } else { (dx, -dy) }
}

fn toward(side: Side) -> (i32, i32) {
    match side {
        Side::Left => (-1, 0),
        Side::Right => (1, 0),
        Side::Top => (0, -1),
        Side::Bottom => (0, 1),
    }
}

/// The perpendicular shoves to try, in order, before pushing into the barrier: none when the
/// cursor already lines up with the barrier, otherwise toward the barrier's span first and the
/// other way second, since a big shove can overshoot the span on unevenly stacked screens.
fn slam_plans(side: Side, edges: &[Edge], position: Option<(f64, f64)>) -> Vec<Option<(i32, i32)>> {
    let vertical_edge = matches!(side, Side::Left | Side::Right);
    let step = |sign: i32| if vertical_edge { Some((0, sign)) } else { Some((sign, 0)) };
    let Some((x, y)) = position else { return vec![None, step(-1), step(1)] };
    let spans: Vec<(f64, f64)> = edges
        .iter()
        .map(|edge| {
            let region = edge.region;
            if vertical_edge {
                (region.y_offset() as f64, region.y_offset() as f64 + region.height() as f64)
            } else {
                (region.x_offset() as f64, region.x_offset() as f64 + region.width() as f64)
            }
        })
        .collect();
    let along = if vertical_edge { y } else { x };
    if spans.iter().any(|(start, end)| along >= *start && along < *end) {
        return vec![None, step(-1), step(1)];
    }
    let lowest = spans.iter().map(|(start, _)| *start).fold(f64::INFINITY, f64::min);
    let first = if along < lowest { 1 } else { -1 };
    vec![step(first), step(-first), None]
}

/// Runs clipboard transfers one at a time so their counters reach the Mac in order. `synced` is
/// the digest of the last content that crossed in either direction, so nothing is sent twice.
async fn clipboard_worker(cipher: Rc<Cipher>, mut jobs: mpsc::UnboundedReceiver<ClipJob>) {
    let mut counter = Counter::new();
    let mut synced: Option<[u8; 32]> = None;
    while let Some(job) = jobs.recv().await {
        let result: Result<()> = async {
            match job {
                ClipJob::Flush(done) => {
                    let _ = done.send(());
                }
                ClipJob::Push { peer, token } => {
                    let Some(content) = clipboard::read().await? else { return Ok(()) };
                    let hash = clipboard::digest(&content);
                    if synced == Some(hash) {
                        return Ok(());
                    }
                    let request = counter.next();
                    let msg = Msg::ClipPut { mime: content.0, data: content.1 };
                    let frame = cipher.seal(&Envelope { token, counter: request, msg }.encode());
                    match clipboard::exchange(&cipher, peer, frame, token).await?.msg {
                        Msg::ClipNone { echo } if echo == request => synced = Some(hash),
                        _ => anyhow::bail!("the Mac did not take the clipboard"),
                    }
                }
                ClipJob::Pull { peer, token } => {
                    let before = clipboard::read().await?.map(|content| clipboard::digest(&content));
                    let request = counter.next();
                    let msg = Msg::ClipGet { have: synced.unwrap_or([0; 32]) };
                    let frame = cipher.seal(&Envelope { token, counter: request, msg }.encode());
                    match clipboard::exchange(&cipher, peer, frame, token).await?.msg {
                        Msg::ClipData { echo, mime, data } if echo == request => {
                            let now = clipboard::read().await?.map(|content| clipboard::digest(&content));
                            if now != before {
                                tracing::info!("kept this desktop's clipboard: it changed while the Mac's was on its way");
                                return Ok(());
                            }
                            let content = (mime, data);
                            clipboard::write(&content).await?;
                            synced = Some(clipboard::digest(&content));
                            tracing::info!("clipboard from the Mac: {} ({} bytes)", content.0, content.1.len());
                        }
                        Msg::ClipNone { echo } if echo == request => {}
                        _ => anyhow::bail!("unexpected clipboard reply"),
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!("clipboard sync: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_scroll_restores_physical_direction() {
        assert_eq!(wheel_to_hid(0.0, -120.0, true), (0.0, -120.0));
        assert_eq!(wheel_to_hid(0.0, 120.0, false), (0.0, -120.0));
        assert_eq!(wheel_to_hid(-120.0, 0.0, true), (120.0, 0.0));
        assert_eq!(wheel_to_hid(120.0, 0.0, false), (120.0, 0.0));
    }
}
