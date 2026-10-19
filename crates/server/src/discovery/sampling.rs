//! Shuffle only the bounded candidates actually observed, never claim uniform
//! coverage of the remote catalogue. The persisted source seed survives restart.
use sha2::{Digest, Sha256};

pub fn rank(uri: &str, seed: i64, round: i64) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(seed.to_le_bytes());
    hash.update(round.to_le_bytes());
    hash.update(uri.as_bytes());
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_sampling_rotates_and_does_not_depend_on_remote_order() {
        let urls: Vec<_> = (0..20)
            .map(|i| format!("https://example.org/users/{i}"))
            .collect();
        let sample = |mut rows: Vec<String>, round| {
            rows.sort_by_key(|u| rank(u, 123, round));
            rows.truncate(3);
            rows
        };
        let first = sample(urls.clone(), 0);
        let mut reversed = urls.clone();
        reversed.reverse();
        assert_eq!(first, sample(reversed, 0));
        let observed: std::collections::HashSet<_> = (0..20)
            .flat_map(|round| sample(urls.clone(), round))
            .collect();
        assert!(
            observed.len() > 15,
            "rotation reaches beyond the first few directory rows"
        );
    }
}
