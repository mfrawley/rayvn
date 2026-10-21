//! Where artifacts come from: the ordered list of remote repositories (with routes, hints, mirrors, proxies
//! and credentials), the local cache, the miss cache, SNAPSHOT revalidation and parallel downloads.

use crate::index::{Index, SnapshotStamp, Validators};
use crate::pom::RepoDecl;
use crate::secret::Secret;
use crate::settings::{Config, Route, Settings, expand_strict};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct Located {
    pub path: PathBuf,
    pub downloaded: bool,
    pub repo: Option<String>,
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    use std::io::Read;
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut h, &buf[..n]);
    }
    Ok(hex(&sha2::Digest::finalize(h)))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct Remote {
    pub id: String,
    pub url: String,
}

pub struct Repos {
    local: PathBuf,
    remotes: RwLock<Vec<Remote>>,
    mirrors: Vec<crate::settings::Mirror>,
    servers: HashMap<String, (String, String)>,
    config_repos: Vec<crate::settings::RepoCfg>,
    routes: Vec<Route>,
    hints: Vec<crate::settings::Hint>,
    central_second: bool,
    auth: RwLock<HashMap<String, Arc<Cred>>>,
    allow_http_auth: HashSet<String>,
    agent: reqwest::blocking::Client,
    index: Index,
    /// Extra cache directories that are read but never written; the writable `local` is checked first.
    read_only: Vec<PathBuf>,
    snapshot_ttl: u64,
    refresh: bool,
    snap_meta_done: Mutex<HashSet<PathBuf>>,
    dead: Mutex<HashSet<String>>,
    pub requests: AtomicUsize,
    cached: AtomicUsize,
    downloaded: AtomicUsize,
    bytes: AtomicU64,
    avoided: AtomicUsize,
    by_repo: Mutex<BTreeMap<String, usize>>,
    last_draw: Mutex<Instant>,
    tty: bool,
}

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Repos {
    /// `declared` are the project's own `<repositories>`; settings, config repos and Central follow.
    pub fn new(declared: &[RepoDecl], settings: &Settings, config: &Config, refresh: bool, cold: bool) -> Result<Self> {
        let local = settings.local_repo.clone().unwrap_or_else(crate::settings::default_cache_dir);

        let mut all: Vec<RepoDecl> = declared.to_vec();
        all.extend(settings.repos.iter().cloned());
        all.extend(config.repo.iter().map(|r| RepoDecl { id: r.id.clone(), url: r.url.clone() }));
        all.push(RepoDecl { id: "central".into(), url: "https://repo1.maven.org/maven2".into() });

        let mut remotes: Vec<Remote> = vec![];
        for r in all {
            let (id, url) = apply_mirror(&settings.mirrors, r);
            if !remotes.iter().any(|x| x.id == id) {
                remotes.push(Remote { id, url });
            }
        }

        for route in &config.route {
            if !remotes.iter().any(|r| r.id == route.repo) {
                bail!("route refers to unknown repository '{}'", route.repo);
            }
        }

        let mut auth: HashMap<String, Arc<Cred>> = HashMap::new();
        for r in &remotes {
            if let Some(c) = credentials_for(&r.id, &config.repo, &settings.servers)? {
                auth.insert(r.id.clone(), Arc::new(c));
            }
        }

        // Bookkeeping lives inside the cache directory, so a fresh cache directory is a cold run.
        let miss_ttl = config.miss_ttl_hours.unwrap_or(24) * 3600;
        let index = Index::load(&local, now_secs(), miss_ttl, !refresh && miss_ttl > 0);
        let mut read_only: Vec<PathBuf> = config.read_only_repos.iter().map(PathBuf::from).collect();
        if let Some(v) = std::env::var_os("RAYVN_RO_REPO") {
            read_only.extend(std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()));
        }
        read_only.retain(|p| *p != local && !cold);

        let mut builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(32)
            .user_agent(concat!("rayvn/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = settings.proxies.first() {
            builder = builder.proxy(build_proxy(p)?);
        }
        let agent = builder.build().context("building HTTP client")?;

        Ok(Repos {
            local,
            remotes: RwLock::new(remotes),
            mirrors: settings.mirrors.clone(),
            servers: settings.servers.clone(),
            config_repos: config.repo.clone(),
            routes: config.route.clone(),
            central_second: config.central_second != Some(false),
            hints: {
                let mut h = config.hint.clone();
                if config.default_hints != Some(false) {
                    h.extend(crate::settings::default_hints());
                }
                h
            },
            auth: RwLock::new(auth),
            allow_http_auth: config.allow_http_auth.iter().cloned().collect(),
            agent,
            index,
            read_only,
            snapshot_ttl: config.snapshot_ttl_hours.unwrap_or(24) * 3600,
            refresh,
            snap_meta_done: Mutex::new(HashSet::new()),
            dead: Mutex::new(HashSet::new()),
            requests: AtomicUsize::new(0),
            cached: AtomicUsize::new(0),
            downloaded: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            avoided: AtomicUsize::new(0),
            by_repo: Mutex::new(BTreeMap::new()),
            last_draw: Mutex::new(Instant::now()),
            tty: std::io::stderr().is_terminal(),
        })
    }

    /// Repositories worth asking for `group`, in order. Routes are exclusive; otherwise hinted ones come first.
    pub fn candidates(&self, group: &str) -> Vec<Remote> {
        let remotes = self.remotes.read().unwrap();
        let best = self.routes.iter().filter_map(|r| r.specificity(group)).max();
        match best {
            Some(score) => self
                .routes
                .iter()
                .filter(|r| r.specificity(group) == Some(score))
                .filter_map(|r| remotes.iter().find(|x| x.id == r.repo))
                .cloned()
                .collect(),
            None => hinted_order(&remotes, &self.hints, group, self.central_second),
        }
    }

    /// Adds repositories declared by a dependency's POM (lowest priority; an id already known keeps its URL).
    pub fn add_repos(&self, declared: &[RepoDecl]) {
        for d in declared.iter().filter(|d| !d.url.contains("${")) {
            if self.remotes.read().unwrap().iter().any(|x| x.id == d.id) {
                continue;
            }
            let (id, url) = apply_mirror(&self.mirrors, d.clone());
            let creds = match credentials_for(&id, &self.config_repos, &self.servers) {
                Ok(c) => c,
                Err(e) => {
                    self.clear_progress();
                    eprintln!("warning: repository '{id}': {e:#}");
                    None
                }
            };
            let mut remotes = self.remotes.write().unwrap();
            if remotes.iter().any(|x| x.id == id) {
                continue;
            }
            remotes.push(Remote { id: id.clone(), url });
            if let Some(c) = creds {
                self.auth.write().unwrap().insert(id, Arc::new(c));
            }
        }
    }

    /// Id and URL of a known repository, for recording in the lockfile.
    pub fn url_of(&self, id: &str) -> Option<String> {
        self.remotes.read().unwrap().iter().find(|r| r.id == id).map(|r| r.url.clone())
    }

    pub fn relative_path(group: &str, artifact: &str, version: &str, classifier: Option<&str>, ext: &str) -> String {
        let c = classifier.map(|c| format!("-{c}")).unwrap_or_default();
        format!("{}/{artifact}/{version}/{artifact}-{version}{c}.{ext}", group.replace('.', "/"))
    }

    /// Remembers which repository supplied `rel`, so later runs on a warm cache can still write it to `rayvn.lock`.
    fn downloaded_from(&self, r: &Remote, rel: &str, path: PathBuf) -> Located {
        self.index.set_origin(rel, &r.id);
        Located { path, downloaded: true, repo: Some(r.id.clone()) }
    }

    /// Repository a cached file came from: where rayvn downloaded it, or for a SNAPSHOT the repo it was last checked against.
    fn origin(&self, rel: &str) -> Option<String> {
        self.index.origin(rel).or_else(|| self.index.snapshot(rel).map(|s| s.repo))
    }

    fn rel_of(&self, p: &Path) -> String {
        p.strip_prefix(&self.local).unwrap_or(p).to_string_lossy().into_owned()
    }

    /// An immutable artifact present in a read-only cache layer. SNAPSHOTs are never served from one.
    fn read_only_hit(&self, rel: &str, snapshot: bool) -> Option<Located> {
        if snapshot {
            return None;
        }
        let path = self.read_only.iter().map(|d| d.join(rel)).find(|p| p.is_file())?;
        self.cached.fetch_add(1, Ordering::Relaxed);
        self.progress();
        Some(Located { path, downloaded: false, repo: None })
    }

    pub fn get(&self, group: &str, artifact: &str, version: &str, classifier: Option<&str>, ext: &str) -> Result<Option<Located>> {
        let rel = Self::relative_path(group, artifact, version, classifier, ext);
        let local = self.local.join(&rel);
        let snapshot = version.ends_with("-SNAPSHOT");
        if local.is_file() {
            if snapshot {
                if let Some(updated) = self.revalidate_snapshot(group, artifact, version, classifier, ext, &local)? {
                    return Ok(Some(updated));
                }
            }
            self.cached.fetch_add(1, Ordering::Relaxed);
            self.progress();
            return Ok(Some(Located { path: local, downloaded: false, repo: self.origin(&rel) }));
        }
        if let Some(hit) = self.read_only_hit(&rel, snapshot) {
            return Ok(Some(hit));
        }
        if snapshot {
            return self.get_snapshot(group, artifact, version, classifier, ext, &local);
        }
        let cands = self.candidates(group);
        self.avoided.fetch_add(self.remotes.read().unwrap().len().saturating_sub(cands.len()), Ordering::Relaxed);
        for r in &cands {
            if self.is_dead(&r.id) {
                continue;
            }
            // Private repos answer 404 to anonymous requests, so a miss only counts for the same auth state.
            let authed = if self.auth.read().unwrap().contains_key(&r.id) { "auth" } else { "anon" };
            let miss_key = format!("{}\t{authed}\t{rel}", r.id);
            if self.index.has_miss(&miss_key) {
                self.avoided.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match self.download(r, &rel, &local, None)? {
                true => return Ok(Some(self.downloaded_from(r, &rel, local))),
                false if self.is_dead(&r.id) => {}
                false => self.index.add_miss(&miss_key, now_secs()),
            }
        }
        Ok(None)
    }

    /// Resolve a SNAPSHOT through `maven-metadata.xml` and store it under the plain `-SNAPSHOT` name, as Maven does.
    fn get_snapshot(&self, group: &str, artifact: &str, version: &str, classifier: Option<&str>, ext: &str, local: &Path) -> Result<Option<Located>> {
        let dir = snapshot_dir(group, artifact, version);
        for r in &self.candidates(group) {
            if self.is_dead(&r.id) {
                continue;
            }
            let meta = metadata_path(local, &r.id);
            if !self.fetch_metadata(r, &dir, &meta)? {
                continue;
            }
            let Some(value) = snapshot_value(&std::fs::read_to_string(&meta)?, version, classifier, ext) else { continue };
            if self.download(r, &snapshot_rel(&dir, artifact, &value, classifier, ext), local, None)? {
                self.index.set_snapshot(&self.rel_of(local), SnapshotStamp { repo: r.id.clone(), value, checked: now_secs() });
                return Ok(Some(Located { path: local.to_path_buf(), downloaded: true, repo: Some(r.id.clone()) }));
            }
        }
        Ok(None)
    }

    /// Fetches `maven-metadata.xml` into `meta`, conditionally if we hold validators for the copy on disk.
    /// True if `meta` is now usable (new content or `304 Not Modified`).
    fn fetch_metadata(&self, r: &Remote, dir: &str, meta: &Path) -> Result<bool> {
        if meta.is_file() && self.snap_meta_done.lock().unwrap().contains(meta) {
            return Ok(true);
        }
        let key = self.rel_of(meta);
        let held = if meta.is_file() { self.index.validators(&key) } else { None };
        match self.fetch(r, &format!("{dir}/maven-metadata.xml"), meta, None, held.as_ref())? {
            Fetched::Got(v) => {
                self.index.set_validators(&key, v);
                self.snap_meta_done.lock().unwrap().insert(meta.to_path_buf());
                Ok(true)
            }
            Fetched::NotModified => {
                self.snap_meta_done.lock().unwrap().insert(meta.to_path_buf());
                Ok(true)
            }
            Fetched::Missing => Ok(false),
        }
    }

    /// A SNAPSHOT that rayvn downloaded earlier is rechecked once its stamp is older than `snapshot_ttl_hours`
    /// (always with `--refresh`). Files rayvn didn't download, such as `mvn install` output, are left alone.
    fn revalidate_snapshot(&self, group: &str, artifact: &str, version: &str, classifier: Option<&str>, ext: &str, local: &Path) -> Result<Option<Located>> {
        let Some(stamp) = self.index.snapshot(&self.rel_of(local)) else { return Ok(None) };
        if !self.refresh && now_secs().saturating_sub(stamp.checked) < self.snapshot_ttl {
            return Ok(None);
        }
        let Some(r) = self.remotes.read().unwrap().iter().find(|r| r.id == stamp.repo).cloned() else { return Ok(None) };
        let dir = snapshot_dir(group, artifact, version);
        let meta = metadata_path(local, &r.id);
        let usable = self.fetch_metadata(&r, &dir, &meta)?;
        if !usable {
            return Ok(None);
        }
        let Some(value) = snapshot_value(&std::fs::read_to_string(&meta)?, version, classifier, ext) else { return Ok(None) };
        if value != stamp.value && self.download(&r, &snapshot_rel(&dir, artifact, &value, classifier, ext), local, None)? {
            self.index.set_snapshot(&self.rel_of(local), SnapshotStamp { repo: r.id.clone(), value, checked: now_secs() });
            return Ok(Some(Located { path: local.to_path_buf(), downloaded: true, repo: Some(r.id.clone()) }));
        }
        self.index.set_snapshot(&self.rel_of(local), SnapshotStamp { checked: now_secs(), ..stamp });
        Ok(None)
    }

    /// Fetch from the repo recorded in the lockfile (no probing); falls back to normal lookup.
    pub fn get_locked(&self, repo: Option<&str>, group: &str, artifact: &str, version: &str, classifier: Option<&str>, ext: &str, sha: Option<&str>) -> Result<Option<Located>> {
        let rel = Self::relative_path(group, artifact, version, classifier, ext);
        let local = self.local.join(&rel);
        if local.is_file() {
            self.cached.fetch_add(1, Ordering::Relaxed);
            self.progress();
            return Ok(Some(Located { path: local, downloaded: false, repo: self.origin(&rel) }));
        }
        if let Some(hit) = self.read_only_hit(&rel, version.ends_with("-SNAPSHOT")) {
            return Ok(Some(hit));
        }
        let known = repo.and_then(|id| self.remotes.read().unwrap().iter().find(|r| r.id == id).cloned());
        if let Some(r) = known {
            if self.download(&r, &rel, &local, sha)? {
                return Ok(Some(self.downloaded_from(&r, &rel, local)));
            }
        }
        let loc = self.get(group, artifact, version, classifier, ext)?;
        if let (Some(l), Some(want)) = (&loc, sha) {
            if l.downloaded && sha256_file(&l.path)? != want {
                bail!("{}: sha256 differs from lockfile", l.path.display());
            }
        }
        Ok(loc)
    }

    pub fn remote_count(&self) -> usize {
        self.remotes.read().unwrap().len()
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Single updating status line on stderr, only on a terminal and at most ~12 times a second.
    fn progress(&self) {
        if !self.tty {
            return;
        }
        let Ok(mut last) = self.last_draw.try_lock() else { return };
        if last.elapsed() < Duration::from_millis(80) {
            return;
        }
        *last = Instant::now();
        eprint!(
            "\r\x1b[2K  fetching: {} cached, {} downloaded ({}), {} requests",
            self.cached.load(Ordering::Relaxed),
            self.downloaded.load(Ordering::Relaxed),
            human(self.bytes.load(Ordering::Relaxed)),
            self.requests.load(Ordering::Relaxed)
        );
    }

    pub fn clear_progress(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
    }

    /// One-line account of where files came from and how many requests were saved.
    pub fn summary(&self) -> String {
        self.clear_progress();
        let n = self.downloaded.load(Ordering::Relaxed);
        let from = self.by_repo.lock().unwrap().iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
        let dl = if n == 0 { "nothing downloaded".to_string() } else { format!("{n} files downloaded ({}: {from})", human(self.bytes.load(Ordering::Relaxed))) };
        format!(
            "{} files from cache, {dl}, {} http requests ({} avoided by routes/miss cache)",
            self.cached.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.avoided.load(Ordering::Relaxed)
        )
    }

    fn is_dead(&self, id: &str) -> bool {
        self.dead.lock().unwrap().contains(id)
    }

    fn mark_dead(&self, r: &Remote, why: &str) {
        if self.dead.lock().unwrap().insert(r.id.clone()) {
            self.clear_progress();
            eprintln!("warning: skipping repository '{}' ({}): {why}", r.id, r.url);
        }
    }

    fn download(&self, r: &Remote, rel: &str, dest: &Path, expect_sha: Option<&str>) -> Result<bool> {
        Ok(matches!(self.fetch(r, rel, dest, expect_sha, None)?, Fetched::Got(_)))
    }

    fn fetch(&self, r: &Remote, rel: &str, dest: &Path, expect_sha: Option<&str>, held: Option<&Validators>) -> Result<Fetched> {
        let url = format!("{}/{rel}", r.url);
        self.requests.fetch_add(1, Ordering::Relaxed);
        let cred = self.auth.read().unwrap().get(&r.id).cloned();
        let mut req = self.agent.get(&url);
        if let Some(c) = &cred {
            req = req.header(reqwest::header::AUTHORIZATION, self.authorization(r, &url, c)?);
        }
        for (name, value) in conditional_headers(held) {
            req = req.header(name, value);
        }
        let resp = match req.send() {
            Ok(resp) => resp,
            Err(e) => {
                // Like Maven: an unreachable repository is skipped with a warning, not fatal.
                self.mark_dead(r, &e.without_url().to_string());
                return Ok(Fetched::Missing);
            }
        };
        if std::env::var_os("RAYVN_DEBUG_HTTP").is_some() {
            eprintln!("  http {} {url}", resp.status().as_u16());
        }
        match classify(resp.status().as_u16(), held.is_some(), cred.is_some()) {
            Outcome::Body => {}
            Outcome::NotModified => return Ok(Fetched::NotModified),
            Outcome::Missing => return Ok(Fetched::Missing),
            Outcome::BadCredentials(code) => {
                bail!("{url}: HTTP {code}; check credentials for repository '{}' (settings.xml <server> or rayvn.toml [[repo]])", r.id)
            }
            Outcome::RepoError(code) => {
                self.mark_dead(r, &format!("HTTP {code}"));
                return Ok(Fetched::Missing);
            }
        }
        let validators = response_validators(resp.headers());
        let size = save_body(resp, dest, expect_sha, &url)?;
        self.downloaded.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(size, Ordering::Relaxed);
        *self.by_repo.lock().unwrap().entry(r.id.clone()).or_default() += 1;
        self.progress();
        Ok(Fetched::Got(validators))
    }

    /// The `Authorization` header for `r`; refuses plain http unless the repository is listed in `allow_http_auth`.
    fn authorization(&self, r: &Remote, url: &str, cred: &Cred) -> Result<reqwest::header::HeaderValue> {
        if url.starts_with("http://") && !self.allow_http_auth.contains(&r.id) {
            bail!("refusing to send credentials for repository '{}' over plain http ({}); use https or add the id to allow_http_auth in rayvn.toml", r.id, r.url);
        }
        let mut v = reqwest::header::HeaderValue::from_str(cred.get(&r.id)?.expose()).context("credential is not a valid header value")?;
        v.set_sensitive(true);
        Ok(v)
    }
}

/// `If-None-Match` / `If-Modified-Since` for whatever validators are held.
fn conditional_headers(held: Option<&Validators>) -> Vec<(reqwest::header::HeaderName, String)> {
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
enum Outcome {
    Body,
    NotModified,
    Missing,
    BadCredentials(u16),
    RepoError(u16),
}

/// What an HTTP status means for the caller. 404 is an ordinary miss; 401/403 with credentials is a configuration
/// error; anything else marks the repository as unusable for this run.
fn classify(status: u16, conditional: bool, authenticated: bool) -> Outcome {
    match status {
        200..=299 => Outcome::Body,
        304 if conditional => Outcome::NotModified,
        404 => Outcome::Missing,
        401 | 403 if authenticated => Outcome::BadCredentials(status),
        _ => Outcome::RepoError(status),
    }
}

fn response_validators(headers: &reqwest::header::HeaderMap) -> Validators {
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    Validators { etag: text(reqwest::header::ETAG), last_modified: text(reqwest::header::LAST_MODIFIED) }
}

/// Streams `body` to a temporary file beside `dest`, checks `expect_sha` if given, then renames into place. Returns the size.
fn save_body(mut body: impl std::io::Read, dest: &Path, expect_sha: Option<&str>, url: &str) -> Result<u64> {
    std::fs::create_dir_all(dest.parent().context("destination has no parent directory")?)?;
    let tmp = dest.with_extension(format!("rayvn-tmp-{}", TMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
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

enum Fetched {
    Got(Validators),
    NotModified,
    Missing,
}

fn snapshot_dir(group: &str, artifact: &str, version: &str) -> String {
    format!("{}/{artifact}/{version}", group.replace('.', "/"))
}

fn snapshot_rel(dir: &str, artifact: &str, value: &str, classifier: Option<&str>, ext: &str) -> String {
    let c = classifier.map(|c| format!("-{c}")).unwrap_or_default();
    format!("{dir}/{artifact}-{value}{c}.{ext}")
}

fn metadata_path(local: &Path, repo_id: &str) -> PathBuf {
    local.parent().unwrap().join(format!("maven-metadata-{repo_id}.xml"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The timestamped version of a SNAPSHOT file listed in a repository's `maven-metadata.xml`.
fn snapshot_value(xml: &str, version: &str, classifier: Option<&str>, ext: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let text = |n: roxmltree::Node, name: &str| n.children().find(|c| c.has_tag_name(name)).and_then(|c| c.text()).map(|t| t.trim().to_string());
    // Some repositories list every historical build; the newest `<updated>` wins (later entries win ties), as in Maven.
    let newest = doc
        .descendants()
        .filter(|n| n.has_tag_name("snapshotVersion"))
        .filter(|sv| text(*sv, "extension").as_deref() == Some(ext) && text(*sv, "classifier").as_deref() == classifier)
        .max_by_key(|sv| text(*sv, "updated").unwrap_or_default());
    if let Some(value) = newest.and_then(|sv| text(sv, "value")) {
        return Some(value);
    }
    let snap = doc.descendants().find(|n| n.has_tag_name("snapshot"))?;
    let (ts, build) = (text(snap, "timestamp")?, text(snap, "buildNumber")?);
    Some(version.trim_end_matches("SNAPSHOT").to_string() + &format!("{ts}-{build}"))
}

/// Remotes in their original order, except that those named by a matching hint come first, most specific hint first
/// (earlier hints win ties, so user hints beat shipped ones), then Central if `central_second`.
fn hinted_order(remotes: &[Remote], hints: &[crate::settings::Hint], group: &str, central_second: bool) -> Vec<Remote> {
    let mut matched: Vec<(usize, usize)> = hints.iter().enumerate().filter_map(|(i, h)| h.specificity(group).map(|s| (s, i))).collect();
    matched.sort_by_key(|&(score, i)| (std::cmp::Reverse(score), i));
    let mut out: Vec<Remote> = vec![];
    for (_, i) in matched {
        for r in remotes.iter().filter(|r| hints[i].names(&r.id, &r.url)) {
            if !out.iter().any(|o| o.id == r.id) {
                out.push(r.clone());
            }
        }
    }
    if central_second {
        if let Some(c) = remotes.iter().find(|r| r.id == "central" || r.url.trim_end_matches('/') == "https://repo1.maven.org/maven2") {
            if !out.iter().any(|o| o.id == c.id) {
                out.push(c.clone());
            }
        }
    }
    out.extend(remotes.iter().filter(|r| !out.iter().any(|o| o.id == r.id)).cloned().collect::<Vec<_>>());
    out
}

fn apply_mirror(mirrors: &[crate::settings::Mirror], r: RepoDecl) -> (String, String) {
    let (id, url) = match mirrors.iter().find(|m| m.matches(&r.id, &r.url)) {
        Some(m) => (m.id.clone(), m.url.clone()),
        None => (r.id, r.url),
    };
    (id, url.trim_end_matches('/').to_string())
}

/// rayvn.toml wins over settings.xml `<server>`.
fn credentials_for(id: &str, config_repos: &[crate::settings::RepoCfg], servers: &HashMap<String, (String, String)>) -> Result<Option<Cred>> {
    for c in config_repos.iter().filter(|c| c.id == id) {
        if let Some(h) = config_auth(c)? {
            return Ok(Some(h));
        }
    }
    if let Some((u, p)) = servers.get(id) {
        if p.starts_with('{') && p.ends_with('}') {
            eprintln!("warning: server '{id}' uses an encrypted password; unsupported, requests will be unauthenticated");
            return Ok(None);
        }
        let (u, p) = (u.clone(), p.clone());
        return Ok(Some(Cred::new(move || Ok(basic(&expand_strict(&u).context("settings.xml server username")?, &expand_strict(&p).context("settings.xml server password")?)))));
    }
    Ok(None)
}

fn basic(user: &str, pass: &str) -> Secret {
    Secret::new(format!("Basic {}", STANDARD.encode(format!("{user}:{pass}"))))
}

/// A credential produced on first use (once per process), so cached or locked runs never need it.
struct Cred {
    init: Box<dyn Fn() -> Result<Secret> + Send + Sync>,
    cell: OnceLock<std::result::Result<Secret, String>>,
}

impl Cred {
    fn new(f: impl Fn() -> Result<Secret> + Send + Sync + 'static) -> Self {
        Cred { init: Box::new(f), cell: OnceLock::new() }
    }

    fn get(&self, repo: &str) -> Result<&Secret> {
        self.cell
            .get_or_init(|| (self.init)().map_err(|e| format!("repository '{repo}': {e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

fn env_var(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => bail!("environment variable {name} is not set or empty"),
    }
}

fn run_helper(argv: &[String], user: Option<&str>) -> Result<Secret> {
    let (prog, args) = argv.split_first().context("empty credential command")?;
    let out = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("could not run credential command '{prog}'"))?;
    if !out.status.success() {
        bail!("credential command '{prog}' failed ({})", out.status);
    }
    let value = String::from_utf8(out.stdout).with_context(|| format!("credential command '{prog}' printed non-UTF-8"))?;
    let value = value.trim();
    if value.is_empty() {
        bail!("credential command '{prog}' printed nothing");
    }
    Ok(match user {
        Some(u) => basic(u, value),
        None => Secret::new(format!("Bearer {value}")),
    })
}

fn config_auth(c: &crate::settings::RepoCfg) -> Result<Option<Cred>> {
    let bad = || anyhow::anyhow!("repository '{}': use exactly one of token_env, user_env+password_env, token_cmd, or password_cmd+user", c.id);
    match (&c.token_env, &c.user_env, &c.password_env, &c.token_cmd, &c.password_cmd) {
        (None, None, None, None, None) if c.user.is_none() => Ok(None),
        (Some(t), None, None, None, None) if c.user.is_none() => {
            let t = t.clone();
            Ok(Some(Cred::new(move || Ok(Secret::new(format!("Bearer {}", env_var(&t)?))))))
        }
        (None, Some(u), Some(p), None, None) if c.user.is_none() => {
            let (u, p) = (u.clone(), p.clone());
            Ok(Some(Cred::new(move || Ok(basic(&env_var(&u)?, &env_var(&p)?)))))
        }
        (None, None, None, Some(argv), None) if c.user.is_none() => {
            let argv = argv.clone();
            Ok(Some(Cred::new(move || run_helper(&argv, None))))
        }
        (None, _, None, None, Some(argv)) => {
            if c.user.is_some() == c.user_env.is_some() {
                return Err(bad());
            }
            let (argv, lit, env) = (argv.clone(), c.user.clone(), c.user_env.clone());
            Ok(Some(Cred::new(move || {
                let user = match (&lit, &env) {
                    (Some(u), _) => u.clone(),
                    (None, Some(e)) => env_var(e)?,
                    (None, None) => unreachable!(),
                };
                run_helper(&argv, Some(&user))
            })))
        }
        _ => Err(bad()),
    }
}

fn build_proxy(p: &crate::settings::ProxyCfg) -> Result<reqwest::Proxy> {
    let mut proxy = reqwest::Proxy::all(format!("{}://{}:{}", p.protocol, p.host, p.port)).map_err(|e| anyhow::anyhow!("proxy '{}': {e}", p.id))?;
    if let Some(u) = &p.username {
        let user = expand_strict(u).with_context(|| format!("proxy '{}' username", p.id))?;
        let pass = match &p.password {
            Some(pw) => expand_strict(pw).with_context(|| format!("proxy '{}' password", p.id))?,
            None => String::new(),
        };
        proxy = proxy.basic_auth(&user, &pass);
    }
    // Maven's nonProxyHosts is `a|b|*.c`; reqwest wants a comma list.
    let hosts: Vec<&str> = p.non_proxy_hosts.iter().flat_map(|n| n.split('|')).map(str::trim).filter(|h| !h.is_empty()).collect();
    Ok(proxy.no_proxy(reqwest::NoProxy::from_string(&hosts.join(","))))
}

fn human(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KB", b as f64 / 1024.0),
        _ => format!("{:.1} MB", b as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rvn-test-{}-{name}", std::process::id())).join("out.jar")
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
        let v = Validators { etag: Some("\"e\"".into()), last_modified: Some("Mon".into()) };
        let names: Vec<_> = conditional_headers(Some(&v)).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, [reqwest::header::IF_NONE_MATCH, reqwest::header::IF_MODIFIED_SINCE]);
        assert!(conditional_headers(Some(&Validators::default())).is_empty());
    }

    #[test]
    fn save_body_writes_atomically_and_verifies_the_checksum() {
        let dest = temp_file("ok");
        let sha = hex(&sha2::Digest::finalize(sha2::Digest::chain_update(<sha2::Sha256 as sha2::Digest>::new(), b"jar")));
        assert_eq!(save_body(&b"jar"[..], &dest, Some(&sha), "u").unwrap(), 3);
        assert_eq!(std::fs::read(&dest).unwrap(), b"jar");
        let bad = temp_file("bad");
        let err = save_body(&b"jar"[..], &bad, Some("0000"), "u").unwrap_err().to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(!bad.exists());
        let leftovers = std::fs::read_dir(bad.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 0, "temporary file must be removed");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
        let _ = std::fs::remove_dir_all(bad.parent().unwrap());
    }

    #[test]
    fn snapshot_value_picks_the_newest_listed_build() {
        let xml = "<metadata><versioning><snapshot><timestamp>20240724.090251</timestamp><buildNumber>12</buildNumber></snapshot><snapshotVersions>\
            <snapshotVersion><extension>jar</extension><value>v-20220210.121839-1</value><updated>20220210121843</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><value>v-20240724.090251-12</value><updated>20240724090441</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><value>v-20230929.101120-10</value><updated>20230929101207</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><classifier>sources</classifier><value>v-1</value><updated>1</updated></snapshotVersion>\
            </snapshotVersions></versioning></metadata>";
        assert_eq!(snapshot_value(xml, "v-SNAPSHOT", None, "jar").as_deref(), Some("v-20240724.090251-12"));
        assert_eq!(snapshot_value(xml, "v-SNAPSHOT", Some("sources"), "jar").as_deref(), Some("v-1"));
        assert_eq!(snapshot_value(xml, "v-SNAPSHOT", None, "pom").as_deref(), Some("v-20240724.090251-12"));
    }

    use super::*;
    use crate::settings::Hint;

    fn remote(id: &str) -> Remote {
        Remote { id: id.into(), url: format!("https://{id}.example/repo") }
    }

    fn hint(repo: &str, groups: &[&str]) -> Hint {
        Hint { repo: Some(repo.into()), url: None, groups: groups.iter().map(|g| g.to_string()).collect() }
    }

    fn ids(v: Vec<Remote>) -> Vec<String> {
        v.into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn unmatched_group_keeps_original_order() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(ids(hinted_order(&remotes, &[hint("b", &["org.x.*"])], "com.y", false)), ["a", "b", "central"]);
    }

    #[test]
    fn matching_hint_moves_repo_to_front() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(ids(hinted_order(&remotes, &[hint("b", &["org.x.*"])], "org.x.sub", false)), ["b", "a", "central"]);
    }

    #[test]
    fn more_specific_hint_goes_first_and_catch_all_follows() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        let hints = [hint("central", &["*"]), hint("b", &["org.x.*"])];
        assert_eq!(ids(hinted_order(&remotes, &hints, "org.x", false)), ["b", "central", "a"]);
        assert_eq!(ids(hinted_order(&remotes, &hints, "other", false)), ["central", "a", "b"]);
    }

    #[test]
    fn earlier_hint_wins_a_tie() {
        let remotes = [remote("a"), remote("b")];
        let hints = [hint("b", &["org.x.*"]), hint("a", &["org.x.*"])];
        assert_eq!(ids(hinted_order(&remotes, &hints, "org.x", false)), ["b", "a"]);
    }

    #[test]
    fn url_hint_ignores_unknown_repositories_and_trailing_slash() {
        let remotes = [remote("a"), remote("b")];
        let by_url = |u: &str| Hint { repo: None, url: Some(u.into()), groups: vec!["org.x.*".into()] };
        assert_eq!(ids(hinted_order(&remotes, &[by_url("https://b.example/repo/")], "org.x", false)), ["b", "a"]);
        assert_eq!(ids(hinted_order(&remotes, &[by_url("https://nowhere.example")], "org.x", false)), ["a", "b"]);
    }

    #[test]
    fn central_second_follows_hints_and_precedes_other_repos() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(ids(hinted_order(&remotes, &[hint("b", &["org.x.*"])], "org.x", true)), ["b", "central", "a"]);
        assert_eq!(ids(hinted_order(&remotes, &[], "org.y", true)), ["central", "a", "b"]);
    }

    #[test]
    fn shipped_hint_list_parses() {
        assert!(!crate::settings::default_hints().is_empty());
    }

    #[test]
    fn sibling_prefix_does_not_match() {
        assert_eq!(hint("a", &["org.example.*"]).specificity("org.examplefoo"), None);
    }

    /// Serves `files` (path -> body) over HTTP on a loopback port until the process exits; returns the base URL.
    fn serve(files: &'static [(&'static str, &'static str)]) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap_or(0);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut stream = &stream;
                let reply = match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        url
    }

    /// A `Repos` over one repository `id` at `url`, caching in `cache`.
    fn repos_for(cache: &Path, id: &str, url: &str) -> Repos {
        let settings = Settings { local_repo: Some(cache.to_path_buf()), ..Settings::default() };
        let config = Config { central_second: Some(false), default_hints: Some(false), ..Config::default() };
        Repos::new(&[RepoDecl { id: id.into(), url: url.into() }], &settings, &config, false, false).unwrap()
    }

    fn fresh_cache(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rvn-cache-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const LIB: &[(&str, &str)] = &[("/g/lib/1.0/lib-1.0.jar", "jar-bytes")];

    #[test]
    fn warm_cache_hits_still_report_the_repository_they_came_from() {
        let url = serve(LIB);
        let cache = fresh_cache("origin-get");

        let cold = repos_for(&cache, "mine", &url).get("g", "lib", "1.0", None, "jar").unwrap().unwrap();
        assert_eq!((cold.downloaded, cold.repo.as_deref()), (true, Some("mine")));

        // A new process: same cache directory, nothing in memory.
        let warm = repos_for(&cache, "mine", &url).get("g", "lib", "1.0", None, "jar").unwrap().unwrap();
        assert_eq!((warm.downloaded, warm.repo.as_deref()), (false, Some("mine")));
    }

    #[test]
    fn warm_locked_lookups_report_the_repository_too() {
        let url = serve(LIB);
        let cache = fresh_cache("origin-locked");

        let cold = repos_for(&cache, "mine", &url).get_locked(Some("mine"), "g", "lib", "1.0", None, "jar", None).unwrap().unwrap();
        assert_eq!(cold.repo.as_deref(), Some("mine"));
        let warm = repos_for(&cache, "mine", &url).get_locked(Some("mine"), "g", "lib", "1.0", None, "jar", None).unwrap().unwrap();
        assert_eq!((warm.downloaded, warm.repo.as_deref()), (false, Some("mine")));
    }

    #[test]
    fn files_rayvn_did_not_download_have_no_known_origin() {
        let cache = fresh_cache("origin-unknown");
        let jar = cache.join("g/lib/1.0/lib-1.0.jar");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, "x").unwrap();
        let loc = repos_for(&cache, "mine", "http://127.0.0.1:1").get("g", "lib", "1.0", None, "jar").unwrap().unwrap();
        assert_eq!((loc.downloaded, loc.repo), (false, None));
    }
}
