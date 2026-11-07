//! A bounded, ordered parallel map on scoped threads: the fetch pool from `docs/pipeline.md`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Run `f` over `items` on at most `workers` threads. Results come back in input order,
/// whatever order the work finishes in.
pub fn map_ordered<T: Send, R: Send>(
    items: Vec<T>,
    workers: usize,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let n = items.len();
    let inputs: Vec<Mutex<Option<T>>> = items.into_iter().map(|i| Mutex::new(Some(i))).collect();
    let outputs: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, n.max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let item = inputs[i]
                    .lock()
                    .expect("input lock")
                    .take()
                    .expect("each index is claimed once");
                let result = f(item);
                *outputs[i].lock().expect("output lock") = Some(result);
            });
        }
    });
    outputs
        .into_iter()
        .map(|m| {
            m.into_inner()
                .expect("output lock")
                .expect("every index ran")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn keeps_input_order_when_work_finishes_out_of_order() {
        let out = map_ordered(vec![30u64, 0, 20, 10], 4, |ms| {
            std::thread::sleep(Duration::from_millis(ms));
            ms
        });
        assert_eq!(out, [30, 0, 20, 10]);
    }

    #[test]
    fn runs_with_fewer_workers_than_items_and_with_none() {
        assert_eq!(
            map_ordered((0..10).collect(), 3, |x| x * 2),
            (0..10).map(|x| x * 2).collect::<Vec<_>>()
        );
        assert!(map_ordered(Vec::<u8>::new(), 6, |x| x).is_empty());
    }
}
