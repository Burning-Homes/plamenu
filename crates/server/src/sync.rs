//! Recovery policy for reconstructable in-memory cache and registry mutexes.

use std::sync::{Mutex, MutexGuard};

/// Lock a reconstructable cache/registry without turning one unwound worker
/// panic into a permanent process-wide panic loop.
pub trait RecoverableMutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: Default> RecoverableMutex<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!("recovering poisoned mutex for reconstructable in-memory state");
                let mut guard = poisoned.into_inner();
                // A panic may have interrupted a multi-step mutation. These
                // mutexes protect reconstructable cache/registry state, so a
                // known-empty value is safer than publishing a partial one.
                *guard = T::default();
                self.clear_poison();
                guard
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::RecoverableMutex as _;

    #[test]
    fn poisoned_registry_recovers_and_clears_poison() {
        let registry = Arc::new(Mutex::new(vec![1]));
        let panicking = Arc::clone(&registry);
        let _ = std::thread::spawn(move || {
            let mut held = panicking.lock().unwrap();
            held.push(2);
            panic!("poison representative registry");
        })
        .join();

        registry.lock_or_recover().push(3);
        assert!(!registry.is_poisoned());
        assert_eq!(*registry.lock().unwrap(), vec![3]);
    }
}
