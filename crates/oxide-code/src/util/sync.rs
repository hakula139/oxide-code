//! Mutex recovery for shared application state.

use std::sync::{Mutex, MutexGuard};

use tracing::warn;

/// Locks `mutex` even when a panicking holder poisoned it, logging a warning that names `label`.
pub(crate) fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, label: &str) -> MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|error| {
        warn!("{label} mutex poisoned, recovering");
        error.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod lock_or_recover {
        use super::*;

        #[test]
        fn preserves_poisoned_state_and_allows_mutation() {
            let mutex = Mutex::new(vec![1]);
            std::thread::scope(|scope| {
                let result = scope
                    .spawn(|| {
                        let mut state = mutex.lock().unwrap();
                        state.push(2);
                        panic!("deliberate poison");
                    })
                    .join();
                assert!(result.is_err());
            });
            assert!(mutex.is_poisoned());
            lock_or_recover(&mutex, "test").push(3);
            assert_eq!(*lock_or_recover(&mutex, "test"), [1, 2, 3]);
        }
    }
}
