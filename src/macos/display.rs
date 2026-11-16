use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const UNREACHABLE_LIMIT: Duration = Duration::from_secs(5);

/// Set once a user is logged in at the console. Until then nothing here touches the window
/// server: macOS refuses a daemon that connects before login and keeps refusing it for the
/// rest of the process's life, which silently leaves the cursor unconfined and edges dead.
static WINDOW_SERVER: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Point,
}

impl Rect {
    pub fn min_x(&self) -> f64 {
        self.origin.x
    }
    pub fn min_y(&self) -> f64 {
        self.origin.y
    }
    pub fn max_x(&self) -> f64 {
        self.origin.x + self.size.x
    }
    pub fn max_y(&self) -> f64 {
        self.origin.y + self.size.y
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.min_x() && point.x < self.max_x() && point.y >= self.min_y() && point.y < self.max_y()
    }

    pub fn clamp(&self, point: Point) -> Point {
        Point {
            x: point.x.clamp(self.min_x(), self.max_x() - 1.0),
            y: point.y.clamp(self.min_y(), self.max_y() - 1.0),
        }
    }

    pub fn distance(&self, point: Point) -> f64 {
        let clamped = self.clamp(point);
        (clamped.x - point.x).hypot(clamped.y - point.y)
    }

    pub fn center(&self) -> Point {
        Point { x: self.origin.x + self.size.x / 2.0, y: self.origin.y + self.size.y / 2.0 }
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> Point;
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGDisplayIsBuiltin(display: u32) -> i32;
    fn CGDisplayMirrorsDisplay(display: u32) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

pub fn cursor() -> Option<Point> {
    if !WINDOW_SERVER.load(Ordering::Relaxed) {
        return None;
    }
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some(point)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Display {
    pub bounds: Rect,
    pub builtin: bool,
}

/// Why this process can no longer use the window server and must be replaced by a fresh one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Loss {
    SessionEnded,
    Unreachable,
}

impl std::fmt::Display for Loss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Loss::SessionEnded => "the console login session this process connected under ended",
            Loss::Unreachable => "the window server stopped reporting the cursor and displays",
        })
    }
}

#[derive(Debug, PartialEq)]
enum Step {
    Waiting,
    Opened(u32),
    Connected,
    Lost(Loss),
}

/// Follows the console's owner and the window server connection made on that user's behalf.
#[derive(Default)]
struct Session {
    connected_as: Option<u32>,
    failing_since: Option<Instant>,
}

impl Session {
    /// Opens the connection once someone logs in, and reports it lost when that user's session
    /// ends (a connection never survives into the next one) or when it stops answering for
    /// `UNREACHABLE_LIMIT`, as one made a moment too early in a login does.
    fn step(&mut self, console: Option<u32>, reachable: impl FnOnce() -> bool, now: Instant) -> Step {
        let Some(user) = self.connected_as else {
            return match console {
                Some(uid) => {
                    self.connected_as = Some(uid);
                    Step::Opened(uid)
                }
                None => Step::Waiting,
            };
        };
        if console != Some(user) {
            return Step::Lost(Loss::SessionEnded);
        }
        if reachable() {
            self.failing_since = None;
            return Step::Connected;
        }
        let since = *self.failing_since.get_or_insert(now);
        if now.duration_since(since) >= UNREACHABLE_LIMIT { Step::Lost(Loss::Unreachable) } else { Step::Connected }
    }
}

/// Keeps a fresh copy of `online()` from a background thread, so the receive loop never waits
/// on WindowServer for the display list (it can be slow to answer while an external display's
/// link is retraining), and watches the login session that allows using WindowServer at all.
pub struct Watcher {
    latest: Arc<Mutex<Vec<Display>>>,
    lost: Arc<OnceLock<Loss>>,
}

impl Watcher {
    pub fn start(interval: Duration) -> Self {
        let latest = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(OnceLock::new());
        let mut session = Session::default();
        let mut follow = {
            let latest = latest.clone();
            let lost = lost.clone();
            move || follow_session(&mut session, &latest, &lost)
        };
        follow();
        let spawned = std::thread::Builder::new().name("baton-pass-displays".into()).spawn(move || {
            while follow() {
                std::thread::sleep(interval);
            }
        });
        if let Err(error) = spawned {
            tracing::warn!("display watcher thread: {error}");
        }
        Self { latest, lost }
    }

    pub fn latest(&self) -> Vec<Display> {
        self.latest.lock().map(|latest| latest.clone()).unwrap_or_default()
    }

    pub fn lost(&self) -> Option<Loss> {
        self.lost.get().copied()
    }
}

/// One round of the watcher: advances the session, refreshes the display list, and returns
/// false once the connection is lost for good.
fn follow_session(session: &mut Session, latest: &Mutex<Vec<Display>>, lost: &OnceLock<Loss>) -> bool {
    let mut displays = Vec::new();
    let step = session.step(
        super::pasteboard::console_uid(),
        || {
            displays = online();
            !displays.is_empty() && cursor().is_some()
        },
        Instant::now(),
    );
    match step {
        Step::Waiting => return true,
        Step::Opened(uid) => {
            WINDOW_SERVER.store(true, Ordering::Relaxed);
            displays = online();
            tracing::info!("user {uid} is logged in at the console; reading the cursor and displays");
        }
        Step::Connected => {}
        Step::Lost(loss) => {
            WINDOW_SERVER.store(false, Ordering::Relaxed);
            let _ = lost.set(loss);
            return false;
        }
    }
    if let Ok(mut latest) = latest.lock() {
        *latest = displays;
    }
    true
}

/// Every non-mirroring display, in global points with the origin at the main display's top left.
/// Sleep state is not consulted: outside the GUI session macOS reports every display as asleep,
/// and the first injected movement wakes a sleeping one anyway.
pub fn online() -> Vec<Display> {
    if !WINDOW_SERVER.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    if unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize]
        .iter()
        .filter(|id| unsafe { CGDisplayMirrorsDisplay(**id) == 0 })
        .map(|id| Display {
            bounds: unsafe { CGDisplayBounds(*id) },
            builtin: unsafe { CGDisplayIsBuiltin(*id) != 0 },
        })
        .filter(|display| display.bounds.size.x > 0.0 && display.bounds.size.y > 0.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_a_login_before_connecting() {
        let mut session = Session::default();
        let now = Instant::now();
        assert_eq!(session.step(None, || panic!("probed before login"), now), Step::Waiting);
        assert_eq!(session.step(Some(501), || panic!("probed while opening"), now), Step::Opened(501));
        assert_eq!(session.step(Some(501), || true, now), Step::Connected);
    }

    #[test]
    fn a_logout_or_another_user_ends_the_connection() {
        let now = Instant::now();
        let mut session = Session::default();
        session.step(Some(501), || true, now);
        assert_eq!(session.step(None, || true, now), Step::Lost(Loss::SessionEnded));
        let mut session = Session::default();
        session.step(Some(501), || true, now);
        assert_eq!(session.step(Some(502), || true, now), Step::Lost(Loss::SessionEnded));
    }

    #[test]
    fn only_a_lasting_silence_counts_as_unreachable() {
        let start = Instant::now();
        let mut session = Session::default();
        session.step(Some(501), || true, start);
        assert_eq!(session.step(Some(501), || false, start), Step::Connected);
        assert_eq!(session.step(Some(501), || true, start + Duration::from_secs(4)), Step::Connected);
        assert_eq!(session.step(Some(501), || false, start + Duration::from_secs(6)), Step::Connected);
        assert_eq!(session.step(Some(501), || false, start + Duration::from_secs(10)), Step::Connected);
        assert_eq!(session.step(Some(501), || false, start + Duration::from_secs(11)), Step::Lost(Loss::Unreachable));
    }
}
