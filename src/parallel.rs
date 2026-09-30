//! Bounded parallel work on scoped threads.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

use anyhow::Context as _;


// ===========
// === map ===
// ===========

/// Applies `work` to every item on at most `limit` threads at once (at least one), keeping the input order.
pub(crate) fn map<T, R, F>(items: &[T], limit: usize, work: F) -> anyhow::Result<Vec<R>> where
T: Sync,
R: Send,
F: Fn(&T) -> R + Sync {
    let next = AtomicUsize::new(0);
    let workers = limit.clamp(1, items.len().max(1));
    let batches = thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| thread::Builder::new().spawn_scoped(scope, || indexed(&next, items, &work)))
            .collect::<std::io::Result<Vec<_>>>()
            .context("failed to start worker threads");
        handles.and_then(|handles| {
            let joined = handles.into_iter().map(thread::ScopedJoinHandle::join).collect::<Vec<_>>();
            joined.into_iter().collect::<Result<Vec<_>, _>>().map_err(|_| anyhow::anyhow!("a worker thread panicked"))
        })
    });
    let mut done = batches?.into_iter().flatten().collect::<Vec<_>>();
    done.sort_by_key(|item| item.index);
    Ok(done.into_iter().map(|item| item.result).collect())
}

struct Indexed<R> {
    index: usize,
    result: R,
}

fn indexed<T, R, F>(next: &AtomicUsize, items: &[T], work: &F) -> Vec<Indexed<R>> where
F: Fn(&T) -> R {
    let mut done = Vec::new();
    loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        match items.get(index) {
            Some(item) => done.push(Indexed { index, result: work(item) }),
            None => break done,
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    use super::map;

    #[test]
    fn keeps_the_input_order() -> anyhow::Result<()> {
        let items = (0..50).collect::<Vec<u32>>();
        let doubled = map(&items, 8, |item| {
            thread::sleep(Duration::from_millis(u64::from(50 - item) % 7));
            item * 2
        })?;
        assert_eq!(doubled, items.iter().map(|item| item * 2).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn never_runs_more_than_the_limit_at_once() -> anyhow::Result<()> {
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let items = vec![(); 24];
        map(&items, 3, |()| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(5));
            running.fetch_sub(1, Ordering::SeqCst);
        })?;
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert!(peak.load(Ordering::SeqCst) >= 2, "nothing ran in parallel");
        Ok(())
    }

    #[test]
    #[allow(clippy::panic)]
    fn workers_that_panic_are_an_error() {
        let items = vec![1_u8, 2, 3, 4];
        let mapped = map(&items, 4, |item| match item % 2 {
            0 => panic!("worker for {item} failed"),
            _ => *item,
        });
        let message = mapped.err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert_eq!(message, "a worker thread panicked");
    }

    #[test]
    fn handles_empty_input_and_a_zero_limit() -> anyhow::Result<()> {
        assert_eq!(map(&Vec::<u8>::new(), 8, |item| *item)?, Vec::<u8>::new());
        assert_eq!(map(&[1_u8, 2], 0, |item| *item)?, vec![1, 2]);
        Ok(())
    }
}
