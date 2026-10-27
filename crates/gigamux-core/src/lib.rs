//! Pure logic placeholder for future encode / prefs / Flamenco work.

/// Crate version string from Cargo.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonzero() {
        assert!(!version().is_empty());
    }
}
