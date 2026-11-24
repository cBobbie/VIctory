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
//! CPU) or queues that wait on each other cannot freeze the title.

use std::collections::{HashMap, VecDeque};
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

/// Each queue's held submissions, by queue address, in submission order.
static QUEUES: LazyLock<Mutex<HashMap<usize, VecDeque<Held>>>> = LazyLock::new(Mutex::default);

fn queues() -> MutexGuard<'static, HashMap<usize, VecDeque<Held>>> {
    QUEUES.lock().unwrap_or_else(|e| e.into_inner())
}

fn holding(q: &HashMap<usize, VecDeque<Held>>, queue: usize) -> bool {
    q.get(&queue).is_some_and(|d| !d.is_empty())
}

/// Whether every wait the lists recorded is met in memory now.
fn met(lists: &[usize]) -> bool {
    lists.iter().all(|&l| {
        crate::list::host_list(l as This).is_none_or(crate::host::HostList::waits_met)
    })
}

/// Runs `lists` submitted on `queue` now, or holds them behind what the
/// queue already holds or until their waits are met.
pub(crate) fn submit(queue: This, lists: &[This], run: fn(&[This])) {
    start_timer(run);
    let lists: Vec<usize> = lists.iter().map(|&l| l as usize).collect();
    let mut q = queues();
    let key = queue as usize;
    if !holding(&q, key) && met(&lists) {
        // Outside the lock, as before holding: signals on other queues do
        // not wait for these lists to run.
        drop(q);
        run(&as_this(&lists));
        q = queues();
    } else {
        #[cfg(feature = "diagnostics")]
        count("submissions held for a wait another queue's work meets");
        q.entry(key).or_default().push_back(Held {
            since: Instant::now(),
            op: Op::Run(lists),
        });
    }
    drain(&mut q, run, false);
}

/// Signals `fence` now, or after what `queue` holds runs.
pub(crate) fn signal(queue: This, fence: This, value: u64) -> HRESULT {
    let mut q = queues();
    let key = queue as usize;
    if holding(&q, key) {
        q.entry(key).or_default().push_back(Held {
            since: Instant::now(),
            op: Op::Signal(fence as usize, value),
        });
        return S_OK;
    }
    drop(q);
    crate::fence::signal(fence, value)
}

fn as_this(lists: &[usize]) -> Vec<This> {
    lists.iter().map(|&l| l as This).collect()
}

/// Runs held submissions that can run, in each queue's order, until none
/// can; with `force`, a queue's first one held past [`LIMIT`] runs too.
fn drain(q: &mut HashMap<usize, VecDeque<Held>>, run: fn(&[This]), force: bool) {
    loop {
        let mut progress = false;
        for held in q.values_mut() {
            while let Some(front) = held.front() {
                let ready = match &front.op {
                    Op::Run(lists) => {
                        let late = force && front.since.elapsed() >= LIMIT;
                        #[cfg(feature = "diagnostics")]
                        if late && !met(lists) {
                            count("held submissions run without their waits met");
                        }
                        late || met(lists)
                    }
                    Op::Signal(..) => true,
                };
                if !ready {
                    break;
                }
                let Some(front) = held.pop_front() else { break };
                match front.op {
                    Op::Run(lists) => run(&as_this(&lists)),
                    Op::Signal(fence, value) => {
                        let _ = crate::fence::signal(fence as This, value);
                    }
                }
                progress = true;
            }
        }
        if !progress {
            return;
        }
    }
}

/// A thread that runs submissions held past [`LIMIT`], when no later
/// submission comes to release them.
fn start_timer(run: fn(&[This])) {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("victory-held-lists".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    let mut q = queues();
                    let late = q.values().any(|d| d.front().is_some_and(|h| h.since.elapsed() >= LIMIT));
                    if late {
                        drain(&mut q, run, true);
                    }
                }
            });
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
