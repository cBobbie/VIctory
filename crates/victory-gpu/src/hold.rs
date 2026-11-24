//! The order of submissions across queues for the title's in-list memory
//! waits (`wait_value`). Lists run on the host as they are submitted, each
//! to completion. A list that waits for a value another queue's later list
//! writes would run before that write, reading what it was not written yet
//! (seen in the world: a per-frame counter one short at every wait). Such a
//! list is held, with what its queue submits after it (lists and fence
//! signals, so the title does not see the work done early), until a list
//! that runs meets its waits.
//!
//! A held submission whose waits stay unmet for [`LIMIT`] runs anyway, as
//! before holding: a writer that never comes (dropped work, a write by the
//! CPU) or queues that wait on each other cannot freeze the title. Held
//! work also runs at once before a present, and before a list it holds is
//! reset or released (D3D12 allows both right after submitting).
//!
//! Lists run outside the lock; a queue running some is busy, and its later
//! submissions queue behind, so each queue keeps its order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_OK, This};

/// How long a held submission waits for its writer before it runs anyway.
const LIMIT: Duration = Duration::from_millis(100);

enum Op {
    /// Title lists (addresses).
    Run(Vec<usize>),
    /// Fence (address) and value.
    Signal(usize, u64),
}

struct Held {
    since: Instant,
    op: Op,
}

#[derive(Default)]
struct State {
    /// Each queue's held submissions, by queue address, in order.
    held: HashMap<usize, VecDeque<Held>>,
    /// Queues whose work runs now on some thread.
    busy: HashSet<usize>,
}

impl State {
    fn holding(&self, queue: usize) -> bool {
        self.busy.contains(&queue) || self.held.get(&queue).is_some_and(|d| !d.is_empty())
    }

    /// Takes the next held op of a queue not busy that may run (`force`:
    /// one held past [`LIMIT`] too; `all`: any), marking the queue busy.
    fn take(&mut self, force: bool, all: bool) -> Option<(usize, Op)> {
        let key = self.held.iter().find_map(|(&key, d)| {
            let front = d.front()?;
            let ready = all
                || match &front.op {
                    Op::Signal(..) => true,
                    Op::Run(lists) => met(lists) || force && front.since.elapsed() >= LIMIT,
                };
            (ready && !self.busy.contains(&key)).then_some(key)
        })?;
        let held = self.held.get_mut(&key)?.pop_front()?;
        #[cfg(feature = "diagnostics")]
        if let Op::Run(lists) = &held.op
            && !met(lists)
        {
            count("held submissions run without their waits met");
        }
        self.busy.insert(key);
        Some((key, held.op))
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
/// No timer thread could start: nothing is held (it could not be released).
static NO_HOLD: AtomicBool = AtomicBool::new(false);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the 32-bit value at `at` has reached `reference` (counters wrap).
fn reached(at: usize, reference: u64) -> bool {
    let mut word = [0u8; 4];
    if !crate::host::read_into(at as *const u8, &mut word) {
        return true;
    }
    at_least(u32::from_le_bytes(word), reference)
}

/// Whether a 32-bit counter at `value` has reached `reference`'s low 32
/// bits, across a wrap (within half the range).
fn at_least(value: u32, reference: u64) -> bool {
    value.wrapping_sub(reference as u32) as i32 >= 0
}

/// Whether every wait of `lists` is met: in memory now, or by a list
/// before it in the same submission (whose writes land once all have run).
fn met(lists: &[usize]) -> bool {
    lists.iter().enumerate().all(|(i, &l)| {
        let Some(list) = crate::list::host_list(l as This) else {
            return true;
        };
        list.waits().into_iter().all(|(at, reference)| {
            reached(at, reference)
                || lists[..i].iter().any(|&e| {
                    crate::list::host_list(e as This)
                        .and_then(|e| e.own_write(at))
                        .is_some_and(|v| at_least(v as u32, reference))
                })
        })
    })
}

fn perform(op: Op) {
    match op {
        Op::Run(lists) => {
            let lists: Vec<This> = lists.iter().map(|&l| l as This).collect();
            crate::queue::run(&lists);
        }
        Op::Signal(fence, value) => {
            let _ = crate::fence::signal(fence as This, value);
        }
    }
}

/// Runs what can run, each op outside the lock, until nothing can.
fn drain(force: bool, all: bool) {
    loop {
        let next = state().take(force, all);
        let Some((key, op)) = next else { return };
        perform(op);
        state().busy.remove(&key);
    }
}

/// Runs `lists` submitted on `queue` now, or holds them behind what the
/// queue holds or runs, or until their waits are met.
pub(crate) fn submit(queue: This, lists: &[This]) {
    start_timer();
    let lists: Vec<usize> = lists.iter().map(|&l| l as usize).collect();
    let key = queue as usize;
    {
        let mut s = state();
        if NO_HOLD.load(Ordering::Relaxed) || !s.holding(key) && met(&lists) {
            s.busy.insert(key);
            drop(s);
            perform(Op::Run(lists));
            state().busy.remove(&key);
        } else {
            #[cfg(feature = "diagnostics")]
            count("submissions held for a wait another queue's work meets");
            s.held.entry(key).or_default().push_back(Held {
                since: Instant::now(),
                op: Op::Run(lists),
            });
        }
    }
    drain(false, false);
}

/// Signals `fence` now, or after what `queue` holds or runs.
pub(crate) fn signal(queue: This, fence: This, value: u64) -> HRESULT {
    let key = queue as usize;
    let mut s = state();
    if s.holding(key) {
        s.held.entry(key).or_default().push_back(Held {
            since: Instant::now(),
            op: Op::Signal(fence as usize, value),
        });
        drop(s);
        drain(false, false);
        return S_OK;
    }
    drop(s);
    crate::fence::signal(fence, value)
}

/// Runs everything held, waiting for queues busy on other threads (before a
/// present: the frame's work first).
pub(crate) fn flush_all() {
    loop {
        drain(false, true);
        let s = state();
        if s.held.values().all(VecDeque::is_empty) {
            return;
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs held work up to and including `list`'s submissions, before the
/// title resets or releases it.
pub(crate) fn flush(list: This) {
    let l = list as usize;
    let holds = |s: &State| {
        s.held
            .values()
            .any(|d| d.iter().any(|h| matches!(&h.op, Op::Run(ls) if ls.contains(&l))))
    };
    while holds(&state()) {
        drain(false, true);
        if holds(&state()) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A thread that runs held submissions whose waits were met by something
/// other than a submission (a CPU write, a fence signal) and those held
/// past [`LIMIT`]. Without it nothing is held.
fn start_timer() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("victory-held-lists".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    let ran = std::panic::catch_unwind(|| drain(true, false));
                    if ran.is_err() {
                        tracing::warn!("held submissions: a release failed; holding stops");
                        NO_HOLD.store(true, Ordering::Relaxed);
                        let _ = std::panic::catch_unwind(flush_all);
                        return;
                    }
                }
            });
        if spawned.is_err() {
            NO_HOLD.store(true, Ordering::Relaxed);
        }
    });
}

/// Diagnostics: an event counted, logged at powers of two.
#[cfg(feature = "diagnostics")]
fn count(what: &'static str) {
    static COUNTS: LazyLock<Mutex<HashMap<&'static str, u64>>> = LazyLock::new(Mutex::default);
    let mut c = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let n = c.entry(what).or_default();
    *n += 1;
    if n.is_power_of_two() {
        tracing::info!(n = *n, "{what}");
    }
}

#[cfg(test)]
mod tests {
    use super::at_least;

    #[test]
    fn counters_reach_their_reference_across_a_wrap() {
        assert!(at_least(1096, 1096));
        assert!(at_least(1097, 1096));
        assert!(!at_least(1095, 1096));
        assert!(at_least(2, 0xFFFF_FFFE));
        assert!(!at_least(0xFFFF_FFFD, 0xFFFF_FFFE));
        // A 64-bit reference compares by its low 32 bits.
        assert!(at_least(5, 0x1_0000_0005));
    }
}
