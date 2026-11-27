//! The parallel work queue every tool's directory walk runs on.
//!
//! A walk starts with one item (the root directory). Processing an item may
//! [`Queue::push`] more items (its subdirectories). The walk is over when
//! nothing is queued and no worker is still processing an item, since only
//! a running item can add new ones.
//!
//! Workers take the newest item first, so the walk goes depth first: the
//! queue holds about depth × fan-out items, where taking the oldest first
//! (breadth first) would hold a whole level of the tree, hundreds of
//! thousands of items in a wide one. The starting items, and the items of
//! one [`Queue::push_all`], are still taken in the order given.
//!
//! Workers that run out of queued items while others are still busy sleep
//! on a condition variable until new work arrives or the walk ends, rather
//! than spinning. Workers run on scoped threads, so the processing closure
//! can borrow configuration and counters from the caller instead of cloning
//! them into `Arc`s.
//!
//! ```
//! use arcommon::walk;
//! use std::sync::atomic::{AtomicUsize, Ordering};
//!
//! // Visit a tree of depth 3 where every node has 2 children.
//! let visited = AtomicUsize::new(0);
//! walk::for_each([0u32], 4, |depth, queue| {
//!     visited.fetch_add(1, Ordering::Relaxed);
//!     if depth < 3 {
//!         queue.push(depth + 1);
//!         queue.push(depth + 1);
//!     }
//! });
//! assert_eq!(visited.into_inner(), 1 + 2 + 4 + 8);
//! ```

use std::sync::{Condvar, Mutex, MutexGuard};

/// How many results the workers of a walk may get ahead of the thread
/// that prints them. When the printer falls behind (a slow terminal, or a
/// pager the user stopped reading), workers wait for it rather than
/// piling up every result in memory: memory stays bounded however many
/// results a search finds. Large enough that the workers rarely wait while
/// the printer keeps up.
pub const RESULT_BUFFER: usize = 4096;

/// Results per batch, for a tool that hands results to its printer in
/// batches (one channel message each) rather than one at a time. With a
/// bounded channel, a printer that is the bottleneck otherwise wakes a
/// waiting worker for every single result.
pub const RESULT_BATCH: usize = 256;

/// Shared state of a running walk, handed to the processing closure so it
/// can add work or end the walk early.
pub struct Queue<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
}

struct State<T> {
    /// Taken from the back: the newest item first.
    items: Vec<T>,
    /// Items queued plus items being processed. The walk ends at zero.
    pending: usize,
    /// Workers waiting for an item. Waking one costs a system call, so new
    /// items wake only as many as there are, and none when all are busy.
    idle: usize,
    stopped: bool,
}

impl<T> Queue<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        // A panicking worker poisons the mutex; the state is still
        // consistent (every update is a single step), and the panic is
        // re-raised when the walk's threads are joined.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds an item to process. Ignored once the walk has been stopped.
    pub fn push(&self, item: T) {
        self.push_all([item]);
    }

    /// Adds several items at once, in one step under the lock (a directory's
    /// subdirectories, say), waking as many idle workers as there are new
    /// items. They are taken in the order given, before anything queued
    /// earlier. Ignored once the walk has been stopped.
    pub fn push_all<I>(&self, items: I)
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: DoubleEndedIterator,
    {
        let mut st = self.lock();
        if st.stopped {
            return;
        }
        let before = st.items.len();
        // Reversed: the first item ends up at the back, taken first.
        st.items.extend(items.into_iter().rev());
        let added = st.items.len() - before;
        st.pending += added;
        let wake = added.min(st.idle);
        drop(st);
        for _ in 0..wake {
            self.changed.notify_one();
        }
    }

    /// Ends the walk early: queued items are dropped, later pushes are
    /// ignored, and the walk returns once the items already being processed
    /// finish. Used when the answer is known (e.g. `argrep -q` found a
    /// match).
    pub fn stop(&self) {
        let mut st = self.lock();
        st.stopped = true;
        let dropped = st.items.len();
        st.items.clear();
        st.pending -= dropped;
        let wake = st.pending == 0 && st.idle > 0;
        drop(st);
        if wake {
            self.changed.notify_all();
        }
    }

    /// Whether [`stop`](Self::stop) has been called.
    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    /// Blocks until there is an item to process, or returns `None` when
    /// the walk is over.
    fn next(&self) -> Option<T> {
        let mut st = self.lock();
        loop {
            if let Some(item) = st.items.pop() {
                return Some(item);
            }
            if st.pending == 0 {
                return None;
            }
            st.idle += 1;
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            st.idle -= 1;
        }
    }

    fn finish_one(&self) {
        let mut st = self.lock();
        st.pending -= 1;
        let done = st.pending == 0 && st.idle > 0;
        drop(st);
        if done {
            self.changed.notify_all();
        }
    }
}

/// Marks an item finished when dropped, so the walk still ends (and the
/// panic surfaces) if processing an item panics.
struct Finish<'a, T>(&'a Queue<T>);

impl<T> Drop for Finish<'_, T> {
    fn drop(&mut self) {
        self.0.finish_one();
    }
}

/// Runs a walk on `workers` threads (at least one) and returns each
/// worker's state, created by `init` and updated by `process` for every
/// item that worker handled. Use the per-worker state for results that are
/// cheaper to merge at the end than to share while walking.
pub fn run<T, S>(
    initial: impl IntoIterator<Item = T>,
    workers: usize,
    init: impl Fn() -> S + Sync,
    process: impl Fn(&mut S, T, &Queue<T>) + Sync,
) -> Vec<S>
where
    T: Send,
    S: Send,
{
    // Reversed, so the first starting item is taken first.
    let mut items: Vec<T> = initial.into_iter().collect();
    items.reverse();
    let queue = Queue {
        state: Mutex::new(State {
            pending: items.len(),
            items,
            idle: 0,
            stopped: false,
        }),
        changed: Condvar::new(),
    };
    let (queue, init, process) = (&queue, &init, &process);

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.max(1))
            .map(|_| {
                scope.spawn(move || {
                    let mut state = init();
                    while let Some(item) = queue.next() {
                        let _finish = Finish(queue);
                        process(&mut state, item, queue);
                    }
                    state
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// [`run`] without per-worker state.
pub fn for_each<T: Send>(
    initial: impl IntoIterator<Item = T>,
    workers: usize,
    process: impl Fn(T, &Queue<T>) + Sync,
) {
    run(
        initial,
        workers,
        || (),
        |_, item, queue| process(item, queue),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Runs `f` on a helper thread and fails instead of hanging the tests.
    fn within(secs: u64, f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(secs))
            .expect("walk did not finish");
    }

    fn tree_size(depth: u32, fanout: u32) -> usize {
        (0..=depth).map(|d| fanout.pow(d) as usize).sum()
    }

    #[test]
    fn visits_every_item_exactly_once_for_any_worker_count() {
        for workers in [0, 1, 2, 8, 64] {
            within(10, move || {
                let seen = AtomicUsize::new(0);
                for_each([0u32], workers, |d, q| {
                    seen.fetch_add(1, Ordering::Relaxed);
                    if d < 6 {
                        for _ in 0..3 {
                            q.push(d + 1);
                        }
                    }
                });
                assert_eq!(seen.into_inner(), tree_size(6, 3), "workers={workers}");
            });
        }
    }

    /// The order one worker takes items in, for a tree where item `n` has
    /// children `n*10+1 ..= n*10+fanout` down to `depth` digits.
    fn order(start: &[u32], fanout: u32, depth: u32, batch: bool) -> Vec<u32> {
        let seen = Mutex::new(Vec::new());
        for_each(start.to_vec(), 1, |n, q| {
            seen.lock().unwrap().push(n);
            if n < 10u32.pow(depth - 1) {
                let children = (1..=fanout).map(|c| n * 10 + c);
                if batch {
                    q.push_all(children.collect::<Vec<_>>());
                } else {
                    // One at a time: each push is taken before the last.
                    for c in children {
                        q.push(c);
                    }
                }
            }
        });
        seen.into_inner().unwrap()
    }

    #[test]
    fn the_walk_is_depth_first_in_the_order_given() {
        // Starting items first to last, a batch first to last, each item's
        // children before its next sibling.
        assert_eq!(
            order(&[1, 2], 2, 3, true),
            [1, 11, 111, 112, 12, 121, 122, 2, 21, 211, 212, 22, 221, 222]
        );
        // Separate pushes: the newest first.
        assert_eq!(order(&[1], 2, 2, false), [1, 12, 11]);
    }

    #[test]
    fn the_queue_holds_about_depth_times_fanout_items() {
        // Breadth first, a tree with fan-out 10 queued a whole level (1000
        // items at the last level here); depth first, about 10 per level.
        let most = AtomicUsize::new(0);
        let state = Mutex::new(0usize);
        for_each([0u32], 1, |depth, q| {
            let mut queued = state.lock().unwrap();
            *queued = queued.saturating_sub(1); // this one, taken
            if depth < 4 {
                q.push_all(vec![depth + 1; 10]);
                *queued += 10;
            }
            most.fetch_max(*queued, Ordering::Relaxed);
        });
        let most = most.into_inner();
        assert!(most <= 4 * 10, "{most} items queued at once");
    }

    #[test]
    fn push_all_wakes_enough_workers() {
        // One item adds 8 slow ones at once: all 8 workers must get one
        // and run at the same time, not one after another.
        within(10, || {
            let running = AtomicUsize::new(0);
            let most = AtomicUsize::new(0);
            for_each([0u32], 8, |d, q| {
                if d == 0 {
                    std::thread::sleep(Duration::from_millis(50)); // all idle
                    q.push_all(vec![1; 8]);
                    return;
                }
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
                running.fetch_sub(1, Ordering::SeqCst);
            });
            assert!(most.into_inner() >= 7, "workers ran one after another");
        });
    }

    #[test]
    fn empty_start_returns_immediately() {
        within(5, || {
            let states = run(Vec::<u32>::new(), 4, || 0usize, |_, _, _| unreachable!());
            assert_eq!(states, vec![0; 4]);
        });
    }

    #[test]
    fn per_worker_state_adds_up() {
        let states = run(
            [0u32],
            4,
            || 0usize,
            |n, d, q| {
                *n += 1;
                if d < 5 {
                    q.push(d + 1);
                    q.push(d + 1);
                }
            },
        );
        assert_eq!(states.len(), 4);
        assert_eq!(states.iter().sum::<usize>(), tree_size(5, 2));
    }

    #[test]
    fn idle_workers_wait_for_slow_items_instead_of_quitting() {
        // One slow item that adds work late: workers that found the queue
        // empty must still be around to take it.
        let states = run(
            [0u32],
            4,
            || 0usize,
            |n, d, q| {
                *n += 1;
                if d == 0 {
                    std::thread::sleep(Duration::from_millis(100));
                    for _ in 0..8 {
                        q.push(1);
                    }
                }
            },
        );
        assert_eq!(states.iter().sum::<usize>(), 9);
    }

    #[test]
    fn stop_drops_queued_work_and_ends_the_walk() {
        within(10, || {
            let seen = AtomicUsize::new(0);
            for_each([0u64], 4, |d, q| {
                if seen.fetch_add(1, Ordering::Relaxed) >= 50 {
                    q.stop();
                    assert!(q.is_stopped());
                }
                // An endless tree: only stop() can end this walk.
                q.push(d + 1);
                q.push(d + 1);
            });
            let n = seen.into_inner();
            assert!((51..60).contains(&n), "processed {n}");
        });
    }

    #[test]
    fn a_panicking_item_propagates_instead_of_hanging() {
        within(10, || {
            let result = std::panic::catch_unwind(|| {
                for_each([0u32], 4, |d, q| {
                    if d == 3 {
                        panic!("boom");
                    }
                    q.push(d + 1);
                    q.push(d + 1);
                });
            });
            assert!(result.is_err());
        });
    }
}
