//! Steadies input over a link that stalls. On Wi-Fi the Mac's radio leaves its channel for AWDL
//! (AirDrop, Universal Control, Sidecar, Continuity), holds the desktop's packets for 100 ms or
//! more a couple of times a second, then hands them all over at once. Measured against the
//! desktop's send stamps those packets are late, and two things follow: a key the desktop let go
//! of during the stall must not sit pressed long enough for macOS to start repeating it, and a
//! burst of late motion is played out over a few frames instead of landing as one jump.
//!
//! Every time here is in microseconds: `now` on the Mac's clock, `at` on the desktop's.

use crate::proto::KeyState;
use std::collections::VecDeque;

/// How long the desktop's word on held keys stays good: it restates them every 10 ms, so a gap
/// this long means the link has stalled.
pub const KEY_GRACE: u64 = 40_000;
/// How long before macOS's first repeat a key nobody vouches for is let go.
const REPEAT_MARGIN: u64 = 25_000;
/// Motion at least this late belongs to a burst and is played out rather than posted at once.
pub const LATE_MOTION: u64 = 15_000;
/// A packet at least this late marks a stall of the link.
const STALL: u64 = 50_000;
/// How long each window of the lateness floor lasts; the floor is the lower of two.
const FLOOR_WINDOW: u64 = 2_000_000;
/// How far back the stall meter counts.
const STALL_WINDOW: u64 = 60_000_000;
/// The time constant of the burst playout.
const PLAYOUT_TAU: f64 = 12_000.0;

/// How late each desktop packet arrives. The smallest difference between receive time and the
/// desktop's send stamp over the last few seconds stands for an on-time packet; anything above
/// it is lateness. Two rotating windows let the floor follow the clocks' drift.
#[derive(Debug, Default)]
pub struct Lateness {
    current: Option<i64>,
    previous: Option<i64>,
    rotated_at: u64,
}

impl Lateness {
    /// Takes a packet stamped `at` and received `now`, and returns how late it is.
    pub fn observe(&mut self, at: u64, now: u64) -> u64 {
        if now.saturating_sub(self.rotated_at) >= FLOOR_WINDOW {
            self.previous = self.current.take();
            self.rotated_at = now;
        }
        let gap = now as i64 - at as i64;
        self.current = Some(self.current.map_or(gap, |floor| floor.min(gap)));
        let floor = self.floor().unwrap_or(gap);
        (gap - floor).max(0) as u64
    }

    fn floor(&self) -> Option<i64> {
        match (self.current, self.previous) {
            (Some(current), Some(previous)) => Some(current.min(previous)),
            (current, previous) => current.or(previous),
        }
    }

    /// The desktop's clock at `now`, as on-time packets tell it.
    pub fn desktop_now(&self, now: u64) -> Option<u64> {
        self.floor().map(|floor| (now as i64 - floor).max(0) as u64)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Usage {
    Key(u16),
    Consumer(u16),
}

#[derive(Debug)]
struct Held {
    usage: Usage,
    pressed: u64,
    since: u64,
    suspended: bool,
}

/// Holds keys only while the desktop keeps vouching for them. macOS repeats a key held past its
/// initial repeat delay, so a key the desktop has not restated for `KEY_GRACE` is let go just
/// before that delay runs out: if the desktop released it during a stall, nothing repeats. When
/// fresh word comes back that the key is still down, and has been for at least the repeat delay,
/// it is pressed again, standing in for the repeat that would have started. Modifiers never
/// repeat and are not part of this.
#[derive(Debug, Default)]
pub struct KeyLease {
    wanted: KeyState,
    confirmed: Option<u64>,
    held: Vec<Held>,
    suspensions: u32,
    represses: u32,
}

impl KeyLease {
    /// Takes the desktop's key state stamped `at`, and returns the state to post now.
    pub fn confirm(&mut self, keys: KeyState, at: u64, now: u64, desktop_now: u64, repeat_delay: u64) -> KeyState {
        if self.confirmed.is_some_and(|confirmed| at < confirmed) {
            return self.effective();
        }
        let fresh = desktop_now.saturating_sub(at) <= KEY_GRACE;
        let usages = usages(&keys);
        self.held.retain(|held| usages.contains(&held.usage));
        for usage in usages {
            match self.held.iter_mut().find(|held| held.usage == usage) {
                Some(held) if held.suspended && fresh && at.saturating_sub(held.since) >= repeat_delay => {
                    held.suspended = false;
                    held.pressed = now;
                    self.represses += 1;
                }
                Some(_) => {}
                None => self.held.push(Held { usage, pressed: now, since: at, suspended: false }),
            }
        }
        self.wanted = keys;
        self.confirmed = Some(at);
        self.effective()
    }

    /// Lets go of every key whose repeat is about to start while the desktop has gone quiet,
    /// and returns the state to post when that changed anything.
    pub fn expire(&mut self, now: u64, desktop_now: u64, repeat_delay: u64) -> Option<KeyState> {
        let confirmed = self.confirmed?;
        if desktop_now.saturating_sub(confirmed) <= KEY_GRACE {
            return None;
        }
        let mut changed = false;
        for held in self.held.iter_mut().filter(|held| !held.suspended) {
            if now + REPEAT_MARGIN >= held.pressed + repeat_delay {
                held.suspended = true;
                changed = true;
                self.suspensions += 1;
            }
        }
        changed.then(|| self.effective())
    }

    /// Lets go of every key at once, for a silence long enough that the desktop may be gone.
    /// They come back only the way `expire`'s do.
    pub fn suspend_all(&mut self) {
        for held in self.held.iter_mut().filter(|held| !held.suspended) {
            held.suspended = true;
            self.suspensions += 1;
        }
    }

    pub fn clear(&mut self) {
        self.wanted = KeyState::default();
        self.confirmed = None;
        self.held.clear();
    }

    /// Keys let go and pressed again since the last call.
    pub fn take_counts(&mut self) -> (u32, u32) {
        (std::mem::take(&mut self.suspensions), std::mem::take(&mut self.represses))
    }

    fn suspended(&self, usage: Usage) -> bool {
        self.held.iter().any(|held| held.usage == usage && held.suspended)
    }

    fn effective(&self) -> KeyState {
        KeyState {
            modifiers: self.wanted.modifiers,
            keys: self.wanted.keys.iter().copied().filter(|key| !self.suspended(Usage::Key(*key))).collect(),
            consumer: self.wanted.consumer.iter().copied().filter(|key| !self.suspended(Usage::Consumer(*key))).collect(),
        }
    }
}

fn usages(keys: &KeyState) -> Vec<Usage> {
    keys.keys.iter().map(|key| Usage::Key(*key)).chain(keys.consumer.iter().map(|key| Usage::Consumer(*key))).collect()
}

/// Motion that arrived late, in desktop pixels, waiting to be played out.
#[derive(Debug, Default)]
pub struct Backlog {
    dx: f32,
    dy: f32,
}

impl Backlog {
    pub fn add(&mut self, dx: f32, dy: f32) {
        self.dx += dx;
        self.dy += dy;
    }

    pub fn is_empty(&self) -> bool {
        self.dx == 0.0 && self.dy == 0.0
    }

    pub fn take_all(&mut self) -> (f32, f32) {
        let all = (self.dx, self.dy);
        *self = Self::default();
        all
    }

    /// The share to post after `elapsed`: an exponential playout that clears a 130 ms burst in
    /// about four 120 Hz frames, and all of it once less than a pixel would be left.
    pub fn take(&mut self, elapsed: u64) -> (f32, f32) {
        let share = (1.0 - (-(elapsed as f64) / PLAYOUT_TAU).exp()) as f32;
        let (dx, dy) = (self.dx * share, self.dy * share);
        let (rest_x, rest_y) = (self.dx - dx, self.dy - dy);
        if rest_x.hypot(rest_y) < 1.0 {
            return self.take_all();
        }
        (self.dx, self.dy) = (rest_x, rest_y);
        (dx, dy)
    }
}

/// Counts the link's stalls over the last minute: runs of packets at least `STALL` late, each
/// with the worst lateness seen in it.
#[derive(Debug, Default)]
pub struct StallMeter {
    stalls: VecDeque<(u64, u64)>,
    stalled: bool,
}

impl StallMeter {
    pub fn observe(&mut self, lateness: u64, now: u64) {
        if lateness < STALL {
            self.stalled = false;
        } else if self.stalled {
            if let Some((_, worst)) = self.stalls.back_mut() {
                *worst = (*worst).max(lateness);
            }
        } else {
            self.stalled = true;
            self.stalls.push_back((now, lateness));
        }
        self.prune(now);
    }

    fn prune(&mut self, now: u64) {
        while self.stalls.front().is_some_and(|(at, _)| now.saturating_sub(*at) > STALL_WINDOW) {
            self.stalls.pop_front();
        }
    }

    /// Stalls in the last minute, and the worst lateness among them in milliseconds.
    pub fn summary(&mut self, now: u64) -> (u16, u16) {
        self.prune(now);
        let worst = self.stalls.iter().map(|(_, worst)| *worst).max().unwrap_or(0);
        (self.stalls.len().min(u16::MAX as usize) as u16, (worst / 1000).min(u16::MAX as u64) as u16)
    }
}

/// Plays macOS's key repeat over a timeline of posted key states, for tests: how many repeats
/// each held stretch of `usage` produced, given the initial delay and the repeat interval.
#[cfg(test)]
pub fn simulated_repeats(posts: &[(u64, KeyState)], usage: u16, end: u64, delay: u64, interval: u64) -> Vec<u64> {
    let mut stretches = Vec::new();
    let mut down: Option<u64> = None;
    for (at, state) in posts {
        let pressed = state.keys.contains(&usage);
        match (down, pressed) {
            (None, true) => down = Some(*at),
            (Some(since), false) => {
                stretches.push(at - since);
                down = None;
            }
            _ => {}
        }
    }
    if let Some(since) = down {
        stretches.push(end - since);
    }
    stretches.into_iter().map(|held| if held < delay { 0 } else { (held - delay) / interval + 1 }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;
    const DELAY: u64 = 166 * MS;
    const INTERVAL: u64 = 16_667;
    const A: u16 = 4;
    const SHIFT: u8 = 0x02;

    fn keys(list: &[u16]) -> KeyState {
        KeyState { modifiers: 0, keys: list.to_vec(), consumer: Vec::new() }
    }

    /// A Mac receiving desktop packets over a link with a fixed 2 ms delay plus `stall`, which
    /// holds every packet sent inside it until it ends. The desktop restates its keys every
    /// 10 ms and the Mac ticks every 4 ms, like the daemons. Without `leased`, the Mac posts
    /// each state as it arrives, as Baton Pass did before the lease.
    fn type_over(stall: Option<(u64, u64)>, timeline: &[(u64, KeyState)], end: u64, leased: bool) -> Vec<(u64, KeyState)> {
        let mut packets = Vec::new();
        let mut sent_at = 0;
        let mut state = KeyState::default();
        let mut changes = timeline.iter().peekable();
        while sent_at <= end {
            while changes.peek().is_some_and(|(at, _)| *at <= sent_at) {
                state = changes.next().unwrap().1.clone();
                packets.push((sent_at, state.clone()));
            }
            packets.push((sent_at, state.clone()));
            sent_at += 10 * MS;
        }
        let arrival = |at: u64| match stall {
            Some((from, until)) if at >= from && at < until => until + 2 * MS,
            _ => at + 2 * MS,
        };
        let mut lateness = Lateness::default();
        let mut lease = KeyLease::default();
        let mut posts = vec![(0, KeyState::default())];
        let post = |posts: &mut Vec<(u64, KeyState)>, at: u64, state: KeyState| {
            if posts.last().map(|(_, last)| last) != Some(&state) {
                posts.push((at, state));
            }
        };
        let mut queue = packets.into_iter().map(|(at, state)| (arrival(at), at, state)).peekable();
        let mut now = 0;
        while now <= end + 300 * MS {
            while queue.peek().is_some_and(|(arrives, _, _)| *arrives <= now) {
                let (arrives, at, state) = queue.next().unwrap();
                lateness.observe(at, arrives);
                let desktop_now = lateness.desktop_now(arrives).unwrap();
                let effective = if leased { lease.confirm(state, at, arrives, desktop_now, DELAY) } else { state };
                post(&mut posts, arrives, effective);
            }
            if leased
                && let Some(desktop_now) = lateness.desktop_now(now)
                && let Some(effective) = lease.expire(now, desktop_now, DELAY)
            {
                post(&mut posts, now, effective);
            }
            now += 4 * MS;
        }
        posts
    }

    #[test]
    fn lateness_is_measured_against_the_fastest_packet() {
        let mut lateness = Lateness::default();
        assert_eq!(lateness.observe(1_000, 50_000), 0);
        assert_eq!(lateness.observe(2_000, 51_000), 0);
        assert_eq!(lateness.observe(3_000, 182_000), 130_000);
        assert_eq!(lateness.desktop_now(200_000), Some(151_000));
    }

    #[test]
    fn the_floor_follows_a_slower_path_after_two_windows() {
        let mut lateness = Lateness::default();
        lateness.observe(0, 1_000);
        let later = 3 * FLOOR_WINDOW;
        assert_eq!(lateness.observe(later, later + 6_000), 5_000);
        let next = later + FLOOR_WINDOW;
        assert_eq!(lateness.observe(next, next + 6_000), 0);
    }

    #[test]
    fn a_tap_held_up_by_a_stall_repeats_only_without_the_lease() {
        let timeline = [(100 * MS, keys(&[A])), (190 * MS, keys(&[]))];
        let stall = Some((120 * MS, 300 * MS));
        let leased = type_over(stall, &timeline, 600 * MS, true);
        assert_eq!(simulated_repeats(&leased, A, 900 * MS, DELAY, INTERVAL), vec![0], "{leased:?}");
        let raw = type_over(stall, &timeline, 600 * MS, false);
        assert_eq!(simulated_repeats(&raw, A, 900 * MS, DELAY, INTERVAL), vec![3], "{raw:?}");
    }

    #[test]
    fn taps_between_stalls_pass_through_untouched() {
        let timeline = [(100 * MS, keys(&[A])), (180 * MS, keys(&[])), (300 * MS, keys(&[A])), (390 * MS, keys(&[]))];
        let leased = type_over(Some((600 * MS, 730 * MS)), &timeline, 900 * MS, true);
        let raw = type_over(Some((600 * MS, 730 * MS)), &timeline, 900 * MS, false);
        assert_eq!(leased, raw);
    }

    #[test]
    fn a_long_hold_keeps_repeating_and_resumes_after_a_stall() {
        let timeline = [(100 * MS, keys(&[A])), (1_100 * MS, keys(&[]))];
        let posts = type_over(Some((500 * MS, 640 * MS)), &timeline, 1_400 * MS, true);
        let repeats = simulated_repeats(&posts, A, 1_700 * MS, DELAY, INTERVAL);
        assert_eq!(repeats.len(), 2, "pressed once and again after the stall: {posts:?}");
        assert!(repeats.iter().all(|count| *count > 0), "{repeats:?}");
        assert!(posts.last().is_some_and(|(_, state)| state.keys.is_empty()));
    }

    #[test]
    fn a_hold_ending_inside_a_stall_stops_at_the_stall() {
        let timeline = [(100 * MS, keys(&[A])), (520 * MS, keys(&[]))];
        let posts = type_over(Some((500 * MS, 640 * MS)), &timeline, 900 * MS, true);
        let repeats = simulated_repeats(&posts, A, 1_200 * MS, DELAY, INTERVAL);
        let up_to_release = (520 * MS - 100 * MS - DELAY) / INTERVAL + 1;
        assert_eq!(repeats.len(), 1, "{posts:?}");
        assert!(repeats[0] <= up_to_release + 3, "{repeats:?} against {up_to_release}");
    }

    #[test]
    fn modifiers_ride_out_a_stall() {
        let shifted = KeyState { modifiers: SHIFT, keys: Vec::new(), consumer: Vec::new() };
        let timeline = [(100 * MS, shifted.clone()), (900 * MS, KeyState::default())];
        let posts = type_over(Some((300 * MS, 600 * MS)), &timeline, 1_000 * MS, true);
        let shift_changes = posts.windows(2).filter(|pair| pair[0].1.modifiers != pair[1].1.modifiers).count();
        assert_eq!(shift_changes, 2, "{posts:?}");
    }

    #[test]
    fn wifi_jitter_posts_at_once_and_a_stall_burst_is_late() {
        let mut lateness = Lateness::default();
        lateness.observe(0, 3 * MS);
        assert!(lateness.observe(10 * MS, 18 * MS) < LATE_MOTION);
        assert!(lateness.observe(20 * MS, 150 * MS) >= LATE_MOTION);
        lateness.reset();
        assert_eq!(lateness.desktop_now(150 * MS), None);
        assert_eq!(lateness.observe(30 * MS, 151 * MS), 0);
    }

    #[test]
    fn clearing_forgets_keys_and_old_word() {
        let mut lease = KeyLease::default();
        lease.confirm(keys(&[A]), 500 * MS, 500 * MS, 500 * MS, DELAY);
        lease.clear();
        assert_eq!(lease.expire(900 * MS, 900 * MS, DELAY), None);
        assert_eq!(lease.confirm(keys(&[A]), 10 * MS, 901 * MS, 10 * MS, DELAY), keys(&[A]));
    }

    #[test]
    fn stale_word_after_a_stall_does_not_press_again() {
        let mut lease = KeyLease::default();
        lease.confirm(keys(&[A]), 0, 0, 0, DELAY);
        assert_eq!(lease.expire(150 * MS, 150 * MS, DELAY), Some(keys(&[])));
        let stale = lease.confirm(keys(&[A]), 200 * MS, 400 * MS, 380 * MS, DELAY);
        assert_eq!(stale, keys(&[]));
        let fresh = lease.confirm(keys(&[A]), 375 * MS, 401 * MS, 381 * MS, DELAY);
        assert_eq!(fresh, keys(&[A]));
        assert_eq!(lease.take_counts(), (1, 1));
    }

    #[test]
    fn a_suspended_key_waits_for_the_repeat_delay_before_coming_back() {
        let mut lease = KeyLease::default();
        lease.confirm(keys(&[A]), 0, 0, 0, DELAY);
        lease.suspend_all();
        assert_eq!(lease.confirm(keys(&[A]), 100 * MS, 101 * MS, 100 * MS, DELAY), keys(&[]));
        assert_eq!(lease.confirm(keys(&[A]), 170 * MS, 171 * MS, 170 * MS, DELAY), keys(&[A]));
    }

    #[test]
    fn backlog_plays_out_in_a_few_frames() {
        let mut backlog = Backlog::default();
        backlog.add(130.0, -26.0);
        let mut posted = (0.0, 0.0);
        let mut ticks = 0;
        while !backlog.is_empty() {
            let (dx, dy) = backlog.take(4 * MS);
            posted = (posted.0 + dx, posted.1 + dy);
            ticks += 1;
        }
        assert!((posted.0 - 130.0f32).abs() < 1e-3 && (posted.1 + 26.0f32).abs() < 1e-3, "{posted:?}");
        assert!((5..=20).contains(&ticks), "{ticks} ticks");
    }

    #[test]
    fn stall_meter_counts_runs_of_late_packets() {
        let mut meter = StallMeter::default();
        for (lateness, now) in [(0, 0), (130 * MS, MS), (90 * MS, 2 * MS), (10 * MS, 3 * MS), (60 * MS, 500 * MS)] {
            meter.observe(lateness, now);
        }
        assert_eq!(meter.summary(600 * MS), (2, 130));
        assert_eq!(meter.summary(600 * MS + STALL_WINDOW), (0, 0));
    }
}
