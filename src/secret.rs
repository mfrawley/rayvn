//! Credentials that cannot leak through logging.

/// A credential that never prints; use `expose` at the single point of use.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// The plain value. Call it only where the credential is sent, never to build a log line.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_prints_as_stars_and_is_readable_only_through_expose() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s} {s:?}"), "*** Secret(***)");
        assert_eq!(s.expose(), "hunter2");
    }
}
