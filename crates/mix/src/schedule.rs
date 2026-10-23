//! Who processes the independent strips of a block: the rendering thread alone, or it and a pool.
//!
//! Handing a block's strips to other threads is not free on the audio thread. The callback has to
//! wake the workers and then wait for the last strip, so every block depends on how soon the
//! scheduler runs threads of ordinary priority, and how busy the rest of the machine is. Measured
//! on a 42-track session at 128 frames, sharing every block's strips through a thread pool took
//! the worst 1% of blocks past the deadline, where processing them on the callback's own thread
//! kept them under a third of it.
//!
//! So a realtime render processes its strips serially and measures what they cost. Only when they
//! take a large share of the block for a while does it share them out: then the rendering thread
//! keeps working too, claiming strips from the same counter as the workers (heaviest first), so it
//! waits only for strips a worker is already processing. Offline renders (bounces, exports) have
//! no deadline and always share their strips.
//!
//! The pool is SoundCraft's own, not the process-wide one other work (video decoding) queues on,
//! and is never created on the audio thread.

use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Share a realtime block's strips once their cost peaks above this share of the block's duration.
const SHARE_ABOVE: f64 = 0.5;
/// Go back to serial processing once the peak falls below this (hysteresis: no flapping).
const SERIAL_BELOW: f64 = 0.25;
/// The remembered peak halves in this much audio (ns), so one slow block does not decide alone.
const PEAK_HALF_LIFE_NS: f64 = 500_000_000.0;
/// Fewer strips than this are never shared: there is too little to gain.
#[cfg(not(target_arch = "wasm32"))]
const MIN_SHARED: usize = 4;

/// Runs a block's strips (see the module docs). Keeps what it needs between blocks, so that steady
/// state does not allocate.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) struct StripRunner<T> {
    /// Each item's processing time in the last block (ns), by position in the work list.
    #[cfg(not(target_arch = "wasm32"))]
    costs: Vec<AtomicU64>,
    /// The items while they are shared: each is claimed by exactly one thread, so its lock is
    /// never contended.
    cells: Vec<Mutex<Option<T>>>,
    /// Positions in the work list, heaviest first: the order the threads claim them in.
    claim: Vec<usize>,
    /// Decaying peak of the strips' total cost, as a share of the block's duration.
    peak: f64,
    shared: bool,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl<T: Send> StripRunner<T> {
    pub(crate) fn new() -> Self {
        StripRunner {
            #[cfg(not(target_arch = "wasm32"))]
            costs: Vec::new(),
            cells: Vec::new(),
            claim: Vec::new(),
            peak: 0.0,
            shared: false,
        }
    }

    /// A runner that shares a realtime block's strips until it has measured them.
    #[cfg(test)]
    pub(crate) fn sharing_from_the_start() -> Self {
        let mut r = Self::new();
        r.shared = true;
        r
    }

    /// Whether the last realtime blocks were heavy enough to share their strips.
    #[cfg(test)]
    pub(crate) fn sharing(&self) -> bool {
        self.shared
    }

    /// Runs `f` once on every item of `work` (in no particular order across threads). `realtime`:
    /// the block has a deadline `period_ns` long, and its strips are shared only under load.
    pub(crate) fn run(&mut self, work: &mut Vec<T>, realtime: bool, period_ns: f64, f: impl Fn(&mut T) + Sync) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (realtime, period_ns);
            work.iter_mut().for_each(f);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.costs.len() < work.len() {
                self.costs.resize_with(work.len(), AtomicU64::default);
            }
            let share = work.len() >= MIN_SHARED && (!realtime || self.shared);
            match pool().filter(|_| share) {
                Some(pool) => self.run_shared(pool, work, &f),
                None => {
                    for (w, cost) in work.iter_mut().zip(&self.costs) {
                        timed(cost, || f(w));
                    }
                }
            }
            if realtime {
                let total: u64 = self.costs.iter().take(work.len()).map(|c| c.load(Ordering::Relaxed)).sum();
                self.observe(total as f64 / period_ns.max(1.0), period_ns);
            }
        }
    }

    /// Folds one block's load (strip time over block duration) into the peak and picks the mode.
    fn observe(&mut self, load: f64, period_ns: f64) {
        let decay = 0.5f64.powf(period_ns.max(0.0) / PEAK_HALF_LIFE_NS);
        self.peak = load.max(self.peak * decay);
        self.shared = if self.shared { self.peak > SERIAL_BELOW } else { self.peak > SHARE_ABOVE };
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_shared(&mut self, pool: &rayon::ThreadPool, work: &mut Vec<T>, f: &(impl Fn(&mut T) + Sync)) {
        let n = work.len();
        self.claim.clear();
        self.claim.extend(0..n);
        let costs = &self.costs;
        self.claim.sort_unstable_by_key(|&k| std::cmp::Reverse(costs.get(k).map_or(0, |c| c.load(Ordering::Relaxed))));
        self.cells.clear();
        self.cells.extend(work.drain(..).map(|w| Mutex::new(Some(w))));
        let next = AtomicUsize::new(0);
        let audio = crate::on_audio_thread();
        let (cells, claim) = (&self.cells, &self.claim);
        let drain = || {
            while let Some(&k) = claim.get(next.fetch_add(1, Ordering::Relaxed)) {
                let (Some(cell), Some(cost)) = (cells.get(k), costs.get(k)) else { continue };
                let mut item = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(w) = item.as_mut() {
                    timed(cost, || f(w));
                }
            }
        };
        let helpers = pool.current_num_threads().min(n.saturating_sub(1));
        pool.in_place_scope(|scope| {
            for _ in 0..helpers {
                // Workers process strips on behalf of this thread: on the audio thread, they are it.
                scope.spawn(|_| crate::as_audio_thread(audio, drain));
            }
            drain();
        });
        work.extend(self.cells.drain(..).filter_map(|c| c.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn timed(cost: &AtomicU64, f: impl FnOnce()) {
    let t0 = std::time::Instant::now();
    f();
    cost.store(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// The strip pool, made on first use off the audio thread (on it, until then: none, so serial).
#[cfg(not(target_arch = "wasm32"))]
fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    if let Some(p) = POOL.get() {
        return p.as_ref();
    }
    if crate::on_audio_thread() {
        return None;
    }
    POOL.get_or_init(|| {
        // The rendering thread works too: one worker fewer than there are cores.
        let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get).saturating_sub(1).clamp(1, 15);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("soundcraft-mix-{i}"))
            .build()
            .inspect_err(|e| log::warn!("mix: no strip pool, strips run on one thread: {e}"))
            .ok()
    })
    .as_ref()
}

/// Makes the strip pool now, so the first realtime render that needs it finds it. Call off the
/// audio thread; [`MixEngine::new`](crate::MixEngine::new) does.
pub(crate) fn warm_up() {
    #[cfg(not(target_arch = "wasm32"))]
    let _ = pool();
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: f64 = 128.0 / 48_000.0 * 1e9;

    #[test]
    fn light_blocks_stay_serial_and_heavy_ones_share() {
        let mut r = StripRunner::<u32>::new();
        for _ in 0..1000 {
            r.observe(0.3, PERIOD);
        }
        assert!(!r.sharing(), "30% of the block is handled on one thread");
        r.observe(0.8, PERIOD);
        assert!(r.sharing(), "one block over the threshold shares the next ones");
        // The peak decays: it takes a while under the lower threshold to go back.
        for _ in 0..10 {
            r.observe(0.1, PERIOD);
        }
        assert!(r.sharing(), "a few light blocks do not switch back");
        for _ in 0..1000 {
            r.observe(0.1, PERIOD);
        }
        assert!(!r.sharing(), "sustained light load goes back to serial");
    }

    #[test]
    fn every_item_runs_once_either_way() {
        for (realtime, shared) in [(true, false), (true, true), (false, false)] {
            let mut r = StripRunner::new();
            r.shared = shared;
            let mut work: Vec<(usize, u32)> = (0..20).map(|i| (i, 0)).collect();
            r.run(&mut work, realtime, PERIOD, |w| w.1 += 1);
            work.sort_unstable();
            assert_eq!(work, (0..20).map(|i| (i, 1)).collect::<Vec<_>>(), "realtime {realtime} shared {shared}");
        }
    }
}
