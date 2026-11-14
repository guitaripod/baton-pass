use crate::config::Side;
use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSession2Options,
    CreateSessionOptions, Deactivated, Disabled, InputCapture, Region, ReleaseOptions,
    SetPointerBarriersResponse, StartOptions, ZonesChanged,
};
use ashpd::desktop::{PersistMode, Session};
use futures_util::Stream;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub type Signals<T> = Pin<Box<dyn Stream<Item = T>>>;

/// How long the Disabled signal echoing one of Baton Pass's own barrier changes may take to arrive.
const DISABLE_ECHO: Duration = Duration::from_secs(2);

pub struct PortalSignals {
    pub activated: Signals<Activated>,
    pub deactivated: Signals<Deactivated>,
    pub disabled: Signals<Disabled>,
    pub zones_changed: Signals<ZonesChanged>,
}

pub struct Ei {
    context: ei::Context,
    pub events: EiConvertEventStream,
}

impl Ei {
    /// Binds every seat to the capabilities Baton Pass forwards; everything else stays with the compositor.
    pub fn bind_seat(&self, event: &EiEvent) {
        if let EiEvent::SeatAdded(added) = event {
            added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            if let Err(error) = self.context.flush() {
                tracing::warn!("flushing ei context: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub id: BarrierID,
    pub region: Region,
}

pub struct Portal {
    capture: &'static InputCapture,
    session: Session<InputCapture>,
    edges: Vec<Edge>,
    enabled: bool,
    expected_disables: Vec<Instant>,
}

impl Portal {
    pub async fn open(app_id: &str) -> Result<(Self, Ei, PortalSignals)> {
        if let Err(error) = ashpd::register_host_app(app_id.try_into()?).await {
            tracing::debug!("host app registration unavailable: {error}");
        }
        let capture: &'static InputCapture = Box::leak(Box::new(
            InputCapture::new().await.context("the InputCapture portal is not available")?,
        ));
        let signals = PortalSignals {
            activated: Box::pin(capture.receive_activated().await?),
            deactivated: Box::pin(capture.receive_deactivated().await?),
            disabled: Box::pin(capture.receive_disabled().await?),
            zones_changed: Box::pin(capture.receive_zones_changed().await?),
        };
        let wanted = Capabilities::KEYBOARD | Capabilities::POINTER;
        let session = if capture.version() >= 2 {
            let session = capture.create_session2(CreateSession2Options::default()).await?;
            let token = load_restore_token();
            let started = capture
                .start(
                    &session,
                    None,
                    StartOptions::default()
                        .set_capabilities(wanted)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token),
                )
                .await?
                .response()
                .context("input capture was not allowed")?;
            if !started.capabilities().contains(wanted) {
                bail!("input capture granted only {:?}", started.capabilities());
            }
            match started.restore_token() {
                Some(token) => save_restore_token(token),
                None => tracing::info!("no restore token: tick \"Allow without asking\" to skip the prompt next time"),
            }
            session
        } else {
            let (session, granted) = capture
                .create_session(None, CreateSessionOptions::default().set_capabilities(wanted))
                .await?;
            if !granted.contains(wanted) {
                bail!("input capture granted only {granted:?}");
            }
            session
        };
        let fd = capture.connect_to_eis(&session, Default::default()).await?;
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (_connection, events) = context
            .handshake_tokio("baton-pass", ei::handshake::ContextType::Receiver)
            .await
            .map_err(|error| anyhow!("ei handshake: {error:?}"))?;
        let portal = Self { capture, session, edges: Vec::new(), enabled: false, expected_disables: Vec::new() };
        Ok((portal, Ei { context, events }, signals))
    }

    /// Places barriers on the outer `side` of the desktop and enables capture.
    pub async fn arm(&mut self, side: Side) -> Result<()> {
        let zones = self.capture.zones(&self.session, Default::default()).await?.response()?;
        let edges = edges_for(zones.regions(), side);
        if edges.is_empty() {
            bail!("no screens to place a barrier on");
        }
        let barriers: Vec<Barrier> =
            edges.iter().map(|edge| Barrier::new(edge.id, barrier_line(edge.region, side))).collect();
        let response = self.set_barriers(&barriers, zones.zone_set()).await?;
        let failed = response.failed_barriers();
        let edges: Vec<Edge> = edges.into_iter().filter(|edge| !failed.contains(&edge.id)).collect();
        if edges.is_empty() {
            bail!("the compositor rejected every barrier");
        }
        self.capture.enable(&self.session, Default::default()).await?;
        tracing::info!(
            "armed {} barrier(s) on the {side:?} edge: {:?}",
            edges.len(),
            edges.iter().map(|edge| edge.region).collect::<Vec<_>>()
        );
        self.edges = edges;
        self.enabled = true;
        Ok(())
    }

    pub fn armed(&self) -> bool {
        self.enabled
    }

    /// Removes the barrier, ending any capture it started. The portal's Disable cannot do this:
    /// xdg-desktop-portal-kde (6.8.2 and its master branch) forwards Disable to KWin as an
    /// enable, so the barrier stays and a capture stays active. Changing the barriers is what
    /// really disables a session there, and it is just as valid on other portals.
    pub async fn disarm(&mut self) {
        if self.enabled {
            self.clear_barriers().await;
        }
    }

    /// Disarms even when Baton Pass believes the session is already disabled, for when the
    /// compositor keeps delivering input that Baton Pass never asked for.
    pub async fn force_disarm(&mut self) {
        self.enabled = true;
        self.clear_barriers().await;
        self.enabled = false;
    }

    async fn clear_barriers(&mut self) {
        let result = async {
            let zones = self.capture.zones(&self.session, Default::default()).await?.response()?;
            self.set_barriers(&[], zones.zone_set()).await
        }
        .await;
        match result {
            Ok(_) => self.edges.clear(),
            Err(error) => tracing::warn!("removing the barrier: {error:#}"),
        }
    }

    /// Replaces the barriers. An enabled session is disabled first, which the compositor
    /// announces with a Disabled signal of its own; that echo is remembered so it is not taken
    /// for the compositor (or its Meta+Shift+Esc) turning capture off.
    async fn set_barriers(&mut self, barriers: &[Barrier], zone_set: u32) -> Result<SetPointerBarriersResponse> {
        let response = self
            .capture
            .set_pointer_barriers(&self.session, barriers, zone_set, Default::default())
            .await?
            .response()?;
        if self.enabled {
            self.enabled = false;
            self.expected_disables.push(Instant::now() + DISABLE_ECHO);
        }
        Ok(response)
    }

    /// Sorts a Disabled signal: true when it echoes Baton Pass's own barrier change, false when the
    /// compositor disabled capture by itself (the session is then marked disabled).
    pub fn own_disable(&mut self) -> bool {
        let now = Instant::now();
        self.expected_disables.retain(|deadline| *deadline > now);
        if self.expected_disables.is_empty() {
            self.enabled = false;
            self.edges.clear();
            return false;
        }
        self.expected_disables.remove(0);
        true
    }

    pub async fn release(&self, activation_id: Option<u32>, position: (f64, f64)) -> Result<()> {
        self.capture
            .release(
                &self.session,
                ReleaseOptions::default()
                    .set_activation_id(activation_id)
                    .set_cursor_position(Some(position)),
            )
            .await?;
        Ok(())
    }

    /// Finds the edge a capture crossed, falling back to the one nearest the cursor.
    pub fn edge_for(&self, activated: &Activated) -> Option<Edge> {
        if let Some(ActivatedBarrier::Barrier(id)) = activated.barrier_id()
            && let Some(edge) = self.edges.iter().find(|edge| edge.id == id) {
                return Some(*edge);
            }
        let (x, y) = activated.cursor_position()?;
        self.edges.iter().copied().min_by(|a, b| {
            distance(a.region, x as f64, y as f64).total_cmp(&distance(b.region, x as f64, y as f64))
        })
    }

    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub async fn close(&self) {
        if let Err(error) = self.session.close().await {
            tracing::debug!("closing portal session: {error}");
        }
    }
}

fn edges_for(regions: &[Region], side: Side) -> Vec<Edge> {
    let outer = |region: &Region| -> i64 {
        let (x, y) = (region.x_offset() as i64, region.y_offset() as i64);
        let (w, h) = (region.width() as i64, region.height() as i64);
        match side {
            Side::Left => -x,
            Side::Right => x + w,
            Side::Top => -y,
            Side::Bottom => y + h,
        }
    };
    let Some(extreme) = regions.iter().map(outer).max() else { return Vec::new() };
    regions
        .iter()
        .filter(|region| outer(region) == extreme)
        .enumerate()
        .map(|(index, region)| Edge {
            id: BarrierID::new(index as u32 + 1).expect("non-zero"),
            region: *region,
        })
        .collect()
}

fn barrier_line(region: Region, side: Side) -> (i32, i32, i32, i32) {
    let (x, y) = (region.x_offset(), region.y_offset());
    let (w, h) = (region.width() as i32, region.height() as i32);
    match side {
        Side::Left => (x, y, x, y + h - 1),
        Side::Right => (x + w, y, x + w, y + h - 1),
        Side::Top => (x, y, x + w - 1, y),
        Side::Bottom => (x, y + h, x + w - 1, y + h),
    }
}

fn distance(region: Region, x: f64, y: f64) -> f64 {
    let (left, top) = (region.x_offset() as f64, region.y_offset() as f64);
    let (right, bottom) = (left + region.width() as f64, top + region.height() as f64);
    let dx = (left - x).max(0.0).max(x - right);
    let dy = (top - y).max(0.0).max(y - bottom);
    dx.hypot(dy)
}

/// Where along the edge (0 at the top or left end, 1 at the other) a point sits.
pub fn edge_fraction(region: Region, side: Side, x: f64, y: f64) -> f32 {
    let fraction = match side {
        Side::Left | Side::Right => (y - region.y_offset() as f64) / region.height().max(1) as f64,
        Side::Top | Side::Bottom => (x - region.x_offset() as f64) / region.width().max(1) as f64,
    };
    fraction.clamp(0.0, 1.0) as f32
}

/// The point `inset` pixels inside the edge, level with where `crossed` met it.
pub fn back_point(region: Region, side: Side, crossed: (f64, f64), inset: f64) -> (f64, f64) {
    edge_point(region, side, edge_fraction(region, side, crossed.0, crossed.1), inset)
}

/// The point `inset` pixels inside the edge at `fraction`, where the cursor reappears.
pub fn edge_point(region: Region, side: Side, fraction: f32, inset: f64) -> (f64, f64) {
    let (x, y) = (region.x_offset() as f64, region.y_offset() as f64);
    let (w, h) = (region.width() as f64, region.height() as f64);
    let fraction = fraction.clamp(0.0, 1.0) as f64;
    let inset = inset.clamp(1.0, (w.min(h) / 2.0).max(1.0));
    match side {
        Side::Left => (x + inset, y + fraction * (h - 1.0)),
        Side::Right => (x + w - 1.0 - inset, y + fraction * (h - 1.0)),
        Side::Top => (x + fraction * (w - 1.0), y + inset),
        Side::Bottom => (x + fraction * (w - 1.0), y + h - 1.0 - inset),
    }
}

fn state_dir() -> PathBuf {
    crate::config::xdg_dir("XDG_STATE_HOME", ".local/state")
}

fn load_restore_token() -> Option<String> {
    std::fs::read_to_string(state_dir().join("restore-token"))
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn save_restore_token(token: &str) {
    let dir = state_dir();
    let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("restore-token"), token));
    if let Err(error) = result {
        tracing::warn!("saving restore token: {error}");
    }
}
