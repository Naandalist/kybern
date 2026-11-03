//! What a forward has fetched for the page loading now (ADE-126), so a slow page looks slow instead of broken.
//!
//! The viewing side of a forward that parses HTTP ([`super::http`]) counts each response's bytes as they arrive. A load
//! starts with each navigation, or with the first request after [`QUIET`] without one open, and ends when none has been
//! open for [`QUIET`]. Its total is kept as the estimate of the next load, and its rate as the link's speed.
//!
//! What is not part of a page load is left out, so it neither keeps a load open nor shows as one:
//! * a request open [`STALLED`] with no bytes for as long (a long poll, an idle stream);
//! * a request open [`TRICKLE_AFTER`] that brings less than [`TRICKLE_RATE`] on average (a stream of small events);
//! * a request of an earlier page: a navigation starts a new load, and what was still open from the old page drops out.
//!
//! The forward's view carries the load ([`PortLoad`]) only while one of its requests has been open [`SLOW_REQUEST`] and
//! still brings bytes, so a fast page, or one that polls quickly, shows nothing. Clients hear about it only while it
//! [`looks_slow`], at most every [`ANNOUNCE_EVERY`].

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kybern_protocol::PortLoad;

/// A load ends, and the next request starts a new one, after this long with no request open.
pub const QUIET: Duration = Duration::from_millis(1500);
/// A load is announced to clients only once it has run this long: shorter ones never show.
pub const ANNOUNCE_AFTER: Duration = Duration::from_secs(2);
/// Clients hear about a load in progress at most this often.
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(3);
/// The transfer rate is measured over this window.
const RATE_WINDOW: Duration = Duration::from_secs(3);
/// A request open this long with no bytes for as long is a long poll or an idle stream, not part of the load.
pub const STALLED: Duration = Duration::from_secs(5);
/// A request open this long that brought less than [`TRICKLE_RATE`] on average is a stream, not part of the load.
pub const TRICKLE_AFTER: Duration = Duration::from_secs(20);
/// Bytes per second.
pub const TRICKLE_RATE: f64 = 512.0;
/// A load shows only while one of its requests has been open this long: on a fast link each one ends sooner.
pub const SLOW_REQUEST: Duration = Duration::from_secs(2);
/// Below this rate a load is slow (bytes per second)…
pub const SLOW_RATE: u64 = 500_000;
/// …and so is one expected to take longer than this to finish, or one that has run this long without an estimate.
pub const LONG_LOAD: Duration = Duration::from_secs(10);

/// Whether a load is worth telling the user about: a low rate, or a long time left (or taken, with no estimate).
pub fn looks_slow(load: &PortLoad) -> bool {
    if load.bytes_per_second < SLOW_RATE {
        return true;
    }
    let done = load.received_bytes + load.cached_bytes;
    match load.expected_bytes {
        Some(expected) => expected.saturating_sub(done) as f64 / load.bytes_per_second as f64 > LONG_LOAD.as_secs_f64(),
        None => load.elapsed_ms > LONG_LOAD.as_millis() as u64,
    }
}

/// One request of the load.
struct Open {
    /// The page load it belongs to.
    generation: u64,
    began: Instant,
    last_bytes: Option<Instant>,
    bytes: u64,
    /// Left out of the load for good: an earlier page, a long poll or a stream.
    left: bool,
}

impl Open {
    /// Whether it no longer counts at `now`, and stays out from then on.
    fn leaves(&mut self, now: Instant, generation: u64) -> bool {
        if !self.left {
            let open = now.duration_since(self.began);
            let idle = now.duration_since(self.last_bytes.unwrap_or(self.began));
            let trickle = open >= TRICKLE_AFTER && (self.bytes as f64) < TRICKLE_RATE * open.as_secs_f64();
            self.left = self.generation != generation || (open >= STALLED && idle >= STALLED) || trickle;
        }
        self.left
    }
}

#[derive(Default)]
struct State {
    started: Option<Instant>,
    received: u64,
    cached: u64,
    last_activity: Option<Instant>,
    /// Bytes received at each moment, for the rate.
    samples: VecDeque<(Instant, u64)>,
    /// The total of the last complete load.
    previous: Option<u64>,
    /// The average rate of the last complete load that brought enough to tell.
    last_rate: Option<u64>,
    requests: HashMap<u64, Open>,
    next_id: u64,
    /// Navigations so far: a request counts for the page load it began in.
    generation: u64,
}

impl State {
    /// The requests that count now, and whether one of them has been open [`SLOW_REQUEST`].
    fn counted(&mut self, now: Instant) -> (u32, bool) {
        let generation = self.generation;
        let (mut open, mut slow) = (0, false);
        for request in self.requests.values_mut() {
            if !request.leaves(now, generation) {
                open += 1;
                slow |= now.duration_since(request.began) >= SLOW_REQUEST;
            }
        }
        (open, slow)
    }

    /// End a load that has been quiet long enough, keeping its total and rate.
    fn settle(&mut self, now: Instant) {
        let Some(started) = self.started else { return };
        if self.counted(now).0 > 0 || self.last_activity.is_some_and(|at| now.duration_since(at) < QUIET) {
            return;
        }
        let total = self.received + self.cached;
        if total > 0 {
            self.previous = Some(total);
        }
        let took = self.last_activity.unwrap_or(started).duration_since(started).as_secs_f64();
        if self.received >= 64 * 1024 && took > 0.2 {
            self.last_rate = Some((self.received as f64 / took) as u64);
        }
        self.start(None);
    }

    fn start(&mut self, at: Option<Instant>) {
        self.started = at;
        self.received = 0;
        self.cached = 0;
        self.samples.clear();
    }
}

/// The load of one forward.
#[derive(Default)]
pub struct LoadMeter {
    state: Mutex<State>,
    /// A task repeats the load to clients while it runs.
    ticking: AtomicBool,
    /// Every byte received over the forward's life.
    total: AtomicU64,
}

impl LoadMeter {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A navigation: a new page load starts now, and what the old page still has open no longer counts.
    pub fn navigated(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        state.settle(now);
        state.generation += 1;
        state.start(Some(now));
        state.last_activity = Some(now);
    }

    /// A request starts; its id for [`Self::received`] and [`Self::end`].
    pub fn begin(&self) -> u64 {
        let now = Instant::now();
        let mut state = self.lock();
        state.settle(now);
        if state.started.is_none() {
            state.start(Some(now));
        }
        state.next_id += 1;
        let id = state.next_id;
        let generation = state.generation;
        state.requests.insert(id, Open { generation, began: now, last_bytes: None, bytes: 0, left: false });
        state.last_activity = Some(now);
        id
    }

    /// Bytes of request `id`'s response arrived over the network.
    pub fn received(&self, id: u64, bytes: usize) {
        self.total.fetch_add(bytes as u64, Ordering::Relaxed);
        let now = Instant::now();
        let mut state = self.lock();
        let generation = state.generation;
        let Some(request) = state.requests.get_mut(&id) else { return };
        let counts = !request.leaves(now, generation);
        request.last_bytes = Some(now);
        request.bytes += bytes as u64;
        if !counts {
            return;
        }
        state.received += bytes as u64;
        let total = state.received;
        state.samples.push_back((now, total));
        while state.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW) && state.samples.len() > 1 {
            state.samples.pop_front();
        }
        state.last_activity = Some(now);
    }

    /// A response was answered from this machine's cache.
    pub fn cached(&self, bytes: usize) {
        let mut state = self.lock();
        state.cached += bytes as u64;
        state.last_activity = Some(Instant::now());
    }

    /// Request `id` ended (answered in full, failed or abandoned).
    pub fn end(&self, id: u64) {
        let now = Instant::now();
        let mut state = self.lock();
        let generation = state.generation;
        if state.requests.remove(&id).is_some_and(|mut request| !request.leaves(now, generation)) {
            state.last_activity = Some(now);
        }
    }

    /// The load in progress, while one of its requests has been open [`SLOW_REQUEST`] and still counts.
    pub fn view(&self) -> Option<PortLoad> {
        let now = Instant::now();
        let mut state = self.lock();
        state.settle(now);
        let started = state.started?;
        let (open, slow) = state.counted(now);
        if open == 0 || !slow {
            return None;
        }
        let rate = match (state.samples.front(), state.samples.back()) {
            (Some((first_at, first)), Some((_, last))) => {
                // From the oldest sample in the window to now: a stall shows as a falling rate.
                let span = now.duration_since(*first_at).as_secs_f64().max(0.25);
                ((last - first) as f64 / span) as u64
            }
            _ => 0,
        };
        Some(PortLoad {
            received_bytes: state.received,
            cached_bytes: state.cached,
            expected_bytes: state.previous,
            bytes_per_second: rate,
            open_requests: open,
            elapsed_ms: now.duration_since(started).as_millis().min(u64::MAX as u128) as u64,
        })
    }

    /// Every byte received over the forward's life.
    pub fn total_received(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// The average rate of the last complete load, when it brought enough to tell (bytes per second).
    pub fn last_rate(&self) -> Option<u64> {
        self.lock().last_rate
    }

    /// Whether a load runs (requests open, or not yet quiet).
    pub fn active(&self) -> bool {
        let mut state = self.lock();
        state.settle(Instant::now());
        state.started.is_some()
    }

    /// Claim the task that repeats the load to clients; false when one runs already.
    pub fn start_ticking(&self) -> bool {
        !self.ticking.swap(true, Ordering::AcqRel)
    }

    pub fn stop_ticking(&self) {
        self.ticking.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Move every clock of the meter back by `by`, as if that much time had passed.
    fn age(meter: &LoadMeter, by: Duration) {
        let mut state = meter.lock();
        state.started = state.started.map(|at| at - by);
        state.last_activity = state.last_activity.map(|at| at - by);
        for request in state.requests.values_mut() {
            request.began -= by;
            request.last_bytes = request.last_bytes.map(|at| at - by);
        }
        for sample in state.samples.iter_mut() {
            sample.0 -= by;
        }
    }

    #[test]
    fn a_load_counts_bytes_while_slow_requests_are_open_and_estimates_the_next_from_its_total() {
        let meter = LoadMeter::default();
        assert!(meter.view().is_none());
        let (first, second) = (meter.begin(), meter.begin());
        meter.received(first, 1000);
        meter.cached(500);
        assert!(meter.view().is_none(), "nothing shows while every request is quick");
        age(&meter, SLOW_REQUEST);
        meter.received(second, 100_000);
        let load = meter.view().unwrap();
        assert_eq!((load.received_bytes, load.cached_bytes, load.open_requests, load.expected_bytes), (101_000, 500, 2, None));
        meter.end(first);
        meter.end(second);
        assert!(meter.view().is_none(), "nothing shows once every request is answered");
        assert!(meter.active(), "the load is still settling");
        // After the quiet time the total becomes the estimate of the next load.
        age(&meter, QUIET);
        assert!(!meter.active());
        assert!(meter.last_rate().is_some(), "enough bytes to measure the link");
        let next = meter.begin();
        age(&meter, SLOW_REQUEST);
        meter.received(next, 1);
        assert_eq!(meter.view().unwrap().expected_bytes, Some(101_500));
        assert_eq!(meter.view().unwrap().received_bytes, 1, "a new load counts from zero");
    }

    #[test]
    fn a_long_poll_or_a_trickling_stream_is_not_a_load() {
        let meter = LoadMeter::default();
        let poll = meter.begin();
        age(&meter, STALLED);
        assert!(meter.view().is_none(), "open with no bytes: a long poll");
        age(&meter, QUIET);
        assert!(!meter.active(), "and it does not keep a load open");
        meter.received(poll, 5000);
        assert!(meter.view().is_none(), "its answer is not a load either");

        let stream = meter.begin();
        for _ in 0..10 {
            age(&meter, Duration::from_secs(2));
            meter.received(stream, 100);
        }
        assert!(meter.view().is_none(), "a heartbeat every two seconds is a stream");
        age(&meter, QUIET);
        assert!(!meter.active());
    }

    #[test]
    fn quick_polling_never_shows_as_a_slow_load() {
        let meter = LoadMeter::default();
        for _ in 0..30 {
            let poll = meter.begin();
            meter.received(poll, 2000);
            assert!(meter.view().is_none());
            meter.end(poll);
            age(&meter, Duration::from_secs(1));
        }
    }

    #[test]
    fn a_navigation_starts_a_new_load_and_the_old_page_drops_out() {
        let meter = LoadMeter::default();
        let old = meter.begin();
        age(&meter, SLOW_REQUEST);
        meter.received(old, 50_000);
        assert_eq!(meter.view().unwrap().received_bytes, 50_000);
        meter.navigated();
        assert!(meter.view().is_none(), "the old page's request no longer counts");
        meter.received(old, 50_000);
        let page = meter.begin();
        age(&meter, SLOW_REQUEST);
        meter.received(page, 1000);
        let load = meter.view().unwrap();
        assert_eq!((load.received_bytes, load.open_requests), (1000, 1), "only the new page");
        assert!(load.elapsed_ms < 10_000);
    }

    #[test]
    fn the_rate_covers_recent_bytes() {
        let meter = LoadMeter::default();
        meter.begin();
        age(&meter, SLOW_REQUEST);
        {
            let mut state = meter.lock();
            let now = Instant::now();
            state.samples.push_back((now - Duration::from_secs(2), 0));
            state.received = 200_000;
            state.samples.push_back((now, 200_000));
        }
        let rate = meter.view().unwrap().bytes_per_second;
        assert!((90_000..=110_000).contains(&rate), "{rate}");
    }

    #[test]
    fn only_a_low_rate_or_a_long_time_left_is_slow() {
        let load = |rate, received, expected, elapsed_ms| PortLoad {
            received_bytes: received,
            cached_bytes: 0,
            expected_bytes: expected,
            bytes_per_second: rate,
            open_requests: 3,
            elapsed_ms,
        };
        assert!(looks_slow(&load(90_000, 4_000_000, Some(31_000_000), 4000)), "90 KB/s");
        assert!(!looks_slow(&load(5_000_000, 4_000_000, Some(31_000_000), 4000)), "5 s left at 5 MB/s");
        assert!(looks_slow(&load(1_000_000, 4_000_000, Some(31_000_000), 4000)), "27 s left at 1 MB/s");
        assert!(!looks_slow(&load(2_000_000, 4_000_000, None, 4000)), "fast, no estimate yet");
        assert!(looks_slow(&load(2_000_000, 24_000_000, None, 12_000)), "fast, but long with no estimate");
    }
}
