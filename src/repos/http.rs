//! The HTTP side of a download: conditional headers, status classification, atomic saves and proxy setup.

use super::sha256_file;
use crate::index::Validators;
use crate::settings::expand_strict;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `If-None-Match` / `If-Modified-Since` for whatever validators are held.
pub(super) fn conditional_headers(
    held: Option<&Validators>,
) -> Vec<(reqwest::header::HeaderName, String)> {
    let Some(v) = held else { return vec![] };
    let mut headers = vec![];
    if let Some(e) = &v.etag {
        headers.push((reqwest::header::IF_NONE_MATCH, e.clone()));
    }
    if let Some(m) = &v.last_modified {
        headers.push((reqwest::header::IF_MODIFIED_SINCE, m.clone()));
    }
    headers
}

#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    Body,
    NotModified,
    Missing,
    BadCredentials(u16),
    RepoError(u16),
}

/// What an HTTP status means for the caller. 404 is an ordinary miss; 401/403 with credentials is a configuration
/// error; anything else marks the repository as unusable for this run.
pub(super) fn classify(status: u16, conditional: bool, authenticated: bool) -> Outcome {
    match status {
        200..=299 => Outcome::Body,
        304 if conditional => Outcome::NotModified,
        404 => Outcome::Missing,
        401 | 403 if authenticated => Outcome::BadCredentials(status),
        _ => Outcome::RepoError(status),
    }
}

pub(super) fn response_validators(headers: &reqwest::header::HeaderMap) -> Validators {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    Validators {
        etag: text(reqwest::header::ETAG),
        last_modified: text(reqwest::header::LAST_MODIFIED),
    }
}

/// Streams `body` to a temporary file beside `dest`, checks `expect_sha` if given, then renames into place. Returns the size.
pub(super) fn save_body(
    mut body: impl std::io::Read,
    dest: &Path,
    expect_sha: Option<&str>,
    url: &str,
) -> Result<u64> {
    std::fs::create_dir_all(
        dest.parent()
            .context("destination has no parent directory")?,
    )?;
    let tmp = dest.with_extension(format!(
        "rayvn-tmp-{}",
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = std::fs::File::create(&tmp)?;
    std::io::copy(&mut body, &mut f).with_context(|| format!("downloading {url}"))?;
    f.flush()?;
    if let Some(want) = expect_sha {
        let got = sha256_file(&tmp)?;
        if got != want {
            let _ = std::fs::remove_file(&tmp);
            bail!("{url}: sha256 mismatch (lockfile {want}, got {got})");
        }
    }
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    std::fs::rename(&tmp, dest)?;
    Ok(size)
}

pub(super) fn build_proxy(p: &crate::settings::ProxyCfg) -> Result<reqwest::Proxy> {
    let mut proxy = reqwest::Proxy::all(format!("{}://{}:{}", p.protocol, p.host, p.port))
        .map_err(|e| anyhow::anyhow!("proxy '{}': {e}", p.id))?;
    if let Some(u) = &p.username {
        let user = expand_strict(u).with_context(|| format!("proxy '{}' username", p.id))?;
        let pass = match &p.password {
            Some(pw) => expand_strict(pw).with_context(|| format!("proxy '{}' password", p.id))?,
            None => String::new(),
        };
        proxy = proxy.basic_auth(&user, &pass);
    }
    // Maven's nonProxyHosts is `a|b|*.c`; reqwest wants a comma list.
    let hosts: Vec<&str> = p
        .non_proxy_hosts
        .iter()
        .flat_map(|n| n.split('|'))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    Ok(proxy.no_proxy(reqwest::NoProxy::from_string(&hosts.join(","))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::hex;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("rvn-test-{}-{name}", std::process::id()))
            .join("out.jar")
    }

    #[test]
    fn statuses_are_classified() {
        assert_eq!(classify(200, false, false), Outcome::Body);
        assert_eq!(classify(304, true, false), Outcome::NotModified);
        assert_eq!(classify(304, false, false), Outcome::RepoError(304));
        assert_eq!(classify(404, false, true), Outcome::Missing);
        assert_eq!(classify(401, false, true), Outcome::BadCredentials(401));
        assert_eq!(classify(403, false, false), Outcome::RepoError(403));
        assert_eq!(classify(502, true, true), Outcome::RepoError(502));
    }

    #[test]
    fn conditional_headers_follow_the_validators_held() {
        assert!(conditional_headers(None).is_empty());
        let v = Validators {
            etag: Some("\"e\"".into()),
            last_modified: Some("Mon".into()),
        };
        let names: Vec<_> = conditional_headers(Some(&v))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names,
            [
                reqwest::header::IF_NONE_MATCH,
                reqwest::header::IF_MODIFIED_SINCE
            ]
        );
        assert!(conditional_headers(Some(&Validators::default())).is_empty());
    }

    #[test]
    fn save_body_writes_atomically_and_verifies_the_checksum() {
        let dest = temp_file("ok");
        let sha = hex(&sha2::Digest::finalize(sha2::Digest::chain_update(
            <sha2::Sha256 as sha2::Digest>::new(),
            b"jar",
        )));
        assert_eq!(save_body(&b"jar"[..], &dest, Some(&sha), "u").unwrap(), 3);
        assert_eq!(std::fs::read(&dest).unwrap(), b"jar");
        let bad = temp_file("bad");
        let err = save_body(&b"jar"[..], &bad, Some("0000"), "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(!bad.exists());
        let leftovers = std::fs::read_dir(bad.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 0, "temporary file must be removed");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
        let _ = std::fs::remove_dir_all(bad.parent().unwrap());
    }
}
