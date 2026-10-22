//! Where artifacts come from: the ordered list of remote repositories (with routes, hints, mirrors, proxies
//! and credentials), the local cache, the miss cache, SNAPSHOT revalidation and parallel downloads.
//!
//! This file holds [`Repos`], which ties the pieces together; the pieces live in submodules: `order` (which
//! repository to ask first), `auth` (credentials), `http` (statuses, conditional requests, atomic saves, proxies)
//! and `metadata` (SNAPSHOT layout and `maven-metadata.xml`).

use crate::index::{Auth, Index, MissKey, SnapshotStamp, Validators};
use crate::pom::RepoDecl;
use crate::settings::{Config, Route, Settings};
use crate::version::is_snapshot;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// One file of an artifact in a repository: its coordinates, optional classifier and extension.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactFile<'a> {
    pub group: &'a str,
    pub artifact: &'a str,
    pub version: &'a str,
    pub classifier: Option<&'a str>,
    pub ext: &'a str,
}

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

mod auth;
mod http;
mod metadata;
mod order;

use auth::{Cred, credentials_for};
use http::{Outcome, build_proxy, classify, conditional_headers, response_validators, save_body};
use metadata::{listed_versions, metadata_path, snapshot_dir, snapshot_rel, snapshot_value};
pub use order::Remote;
use order::{CENTRAL_ID, CENTRAL_URL, apply_mirror, hinted_order};

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
    /// Draw the status line? Only on a terminal, and not while several modules print their own lines.
    tty: AtomicBool,
}

impl Repos {
    /// `declared` are the project's own `<repositories>`; settings, config repos and Central follow.
    pub fn new(
        declared: &[RepoDecl],
        settings: &Settings,
        config: &Config,
        refresh: bool,
        cold: bool,
    ) -> Result<Self> {
        let local = settings
            .local_repo
            .clone()
            .unwrap_or_else(crate::settings::default_cache_dir);

        let mut all: Vec<RepoDecl> = declared.to_vec();
        all.extend(settings.repos.iter().cloned());
        all.extend(config.repo.iter().map(|r| RepoDecl {
            id: r.id.clone(),
            url: r.url.clone(),
        }));
        all.push(RepoDecl {
            id: CENTRAL_ID.into(),
            url: CENTRAL_URL.into(),
        });

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
        let mut read_only: Vec<PathBuf> =
            config.read_only_repos.iter().map(PathBuf::from).collect();
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
            tty: AtomicBool::new(std::io::stderr().is_terminal()),
        })
    }

    /// Repositories worth asking for `group`, in order. Routes are exclusive; otherwise hinted ones come first.
    pub fn candidates(&self, group: &str) -> Vec<Remote> {
        let remotes = self.remotes.read().unwrap();
        let best = self
            .routes
            .iter()
            .filter_map(|r| r.specificity(group))
            .max();
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
            remotes.push(Remote {
                id: id.clone(),
                url,
            });
            if let Some(c) = creds {
                self.auth.write().unwrap().insert(id, Arc::new(c));
            }
        }
    }

    /// Id and URL of a known repository, for recording in the lockfile.
    pub fn url_of(&self, id: &str) -> Option<String> {
        self.remotes
            .read()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.url.clone())
    }

    pub fn relative_path(
        group: &str,
        artifact: &str,
        version: &str,
        classifier: Option<&str>,
        ext: &str,
    ) -> String {
        let c = classifier.map(|c| format!("-{c}")).unwrap_or_default();
        format!(
            "{}/{artifact}/{version}/{artifact}-{version}{c}.{ext}",
            group.replace('.', "/")
        )
    }

    /// Remembers which repository supplied `rel`, so later runs on a warm cache can still write it to `rayvn.lock`.
    fn downloaded_from(&self, r: &Remote, rel: &str, path: PathBuf) -> Located {
        self.index.set_origin(rel, &r.id);
        Located {
            path,
            downloaded: true,
            repo: Some(r.id.clone()),
        }
    }

    /// Repository a cached file came from: where rayvn downloaded it, or for a SNAPSHOT the repo it was last checked against.
    fn origin(&self, rel: &str) -> Option<String> {
        self.index
            .origin(rel)
            .or_else(|| self.index.snapshot(rel).map(|s| s.repo))
    }

    fn rel_of(&self, p: &Path) -> String {
        p.strip_prefix(&self.local)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    }

    /// An immutable artifact present in a read-only cache layer. SNAPSHOTs are never served from one.
    fn read_only_hit(&self, rel: &str, snapshot: bool) -> Option<Located> {
        if snapshot {
            return None;
        }
        let path = self
            .read_only
            .iter()
            .map(|d| d.join(rel))
            .find(|p| p.is_file())?;
        self.cached.fetch_add(1, Ordering::Relaxed);
        self.progress();
        Some(Located {
            path,
            downloaded: false,
            repo: None,
        })
    }

    pub fn get(
        &self,
        group: &str,
        artifact: &str,
        version: &str,
        classifier: Option<&str>,
        ext: &str,
    ) -> Result<Option<Located>> {
        let rel = Self::relative_path(group, artifact, version, classifier, ext);
        let local = self.local.join(&rel);
        let snapshot = is_snapshot(version);
        if local.is_file() {
            if snapshot
                && let Some(updated) =
                    self.revalidate_snapshot(group, artifact, version, classifier, ext, &local)?
            {
                return Ok(Some(updated));
            }
            self.cached.fetch_add(1, Ordering::Relaxed);
            self.progress();
            return Ok(Some(Located {
                path: local,
                downloaded: false,
                repo: self.origin(&rel),
            }));
        }
        if let Some(hit) = self.read_only_hit(&rel, snapshot) {
            return Ok(Some(hit));
        }
        if snapshot {
            return self.get_snapshot(group, artifact, version, classifier, ext, &local);
        }
        let cands = self.candidates(group);
        self.avoided.fetch_add(
            self.remotes
                .read()
                .unwrap()
                .len()
                .saturating_sub(cands.len()),
            Ordering::Relaxed,
        );
        for r in &cands {
            if self.is_dead(&r.id) {
                continue;
            }
            // Private repos answer 404 to anonymous requests, so a miss only counts for the same auth state.
            let auth = if self.auth.read().unwrap().contains_key(&r.id) {
                Auth::Authenticated
            } else {
                Auth::Anonymous
            };
            let miss_key = MissKey::new(&r.id, auth, &rel);
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
    fn get_snapshot(
        &self,
        group: &str,
        artifact: &str,
        version: &str,
        classifier: Option<&str>,
        ext: &str,
        local: &Path,
    ) -> Result<Option<Located>> {
        let dir = snapshot_dir(group, artifact, version);
        for r in &self.candidates(group) {
            if self.is_dead(&r.id) {
                continue;
            }
            let meta = metadata_path(local, &r.id);
            if !self.fetch_metadata(r, &dir, &meta)? {
                continue;
            }
            let Some(value) =
                snapshot_value(&std::fs::read_to_string(&meta)?, version, classifier, ext)
            else {
                continue;
            };
            if self.download(
                r,
                &snapshot_rel(&dir, artifact, &value, classifier, ext),
                local,
                None,
            )? {
                self.index.set_snapshot(
                    &self.rel_of(local),
                    SnapshotStamp {
                        repo: r.id.clone(),
                        value,
                        checked: now_secs(),
                    },
                );
                return Ok(Some(Located {
                    path: local.to_path_buf(),
                    downloaded: true,
                    repo: Some(r.id.clone()),
                }));
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
        let held = if meta.is_file() {
            self.index.validators(&key)
        } else {
            None
        };
        match self.fetch(
            r,
            &format!("{dir}/maven-metadata.xml"),
            meta,
            None,
            held.as_ref(),
        )? {
            Fetched::Got(v) => {
                self.index.set_validators(&key, v);
                self.snap_meta_done
                    .lock()
                    .unwrap()
                    .insert(meta.to_path_buf());
                Ok(true)
            }
            Fetched::NotModified => {
                self.snap_meta_done
                    .lock()
                    .unwrap()
                    .insert(meta.to_path_buf());
                Ok(true)
            }
            Fetched::Missing => Ok(false),
        }
    }

    /// Every version of `group:artifact` the candidate repositories list in their `maven-metadata.xml`, merged.
    /// A listing younger than `snapshot_ttl_hours` is reused without a request (`--refresh` always asks), so a
    /// range floats only when you re-lock. Repositories without metadata for the artifact are skipped.
    pub fn versions(&self, group: &str, artifact: &str) -> Result<Vec<String>> {
        let dir = format!("{}/{artifact}", group.replace('.', "/"));
        let mut all: Vec<String> = vec![];
        for r in &self.candidates(group) {
            if self.is_dead(&r.id) {
                continue;
            }
            let meta = self
                .local
                .join(&dir)
                .join(format!("maven-metadata-{}.xml", r.id));
            if !(self.listing_is_fresh(&meta) || self.fetch_metadata(r, &dir, &meta)?) {
                continue;
            }
            for v in listed_versions(&std::fs::read_to_string(&meta)?) {
                if !all.contains(&v) {
                    all.push(v);
                }
            }
        }
        Ok(all)
    }

    fn listing_is_fresh(&self, meta: &Path) -> bool {
        let age = std::fs::metadata(meta)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        !self.refresh && age.is_some_and(|a| a.as_secs() < self.snapshot_ttl)
    }

    /// A SNAPSHOT that rayvn downloaded earlier is rechecked once its stamp is older than `snapshot_ttl_hours`
    /// (always with `--refresh`). Files rayvn didn't download, such as `mvn install` output, are left alone.
    fn revalidate_snapshot(
        &self,
        group: &str,
        artifact: &str,
        version: &str,
        classifier: Option<&str>,
        ext: &str,
        local: &Path,
    ) -> Result<Option<Located>> {
        let Some(stamp) = self.index.snapshot(&self.rel_of(local)) else {
            return Ok(None);
        };
        if !self.refresh && now_secs().saturating_sub(stamp.checked) < self.snapshot_ttl {
            return Ok(None);
        }
        let Some(r) = self
            .remotes
            .read()
            .unwrap()
            .iter()
            .find(|r| r.id == stamp.repo)
            .cloned()
        else {
            return Ok(None);
        };
        let dir = snapshot_dir(group, artifact, version);
        let meta = metadata_path(local, &r.id);
        let usable = self.fetch_metadata(&r, &dir, &meta)?;
        if !usable {
            return Ok(None);
        }
        let Some(value) =
            snapshot_value(&std::fs::read_to_string(&meta)?, version, classifier, ext)
        else {
            return Ok(None);
        };
        if value != stamp.value
            && self.download(
                &r,
                &snapshot_rel(&dir, artifact, &value, classifier, ext),
                local,
                None,
            )?
        {
            self.index.set_snapshot(
                &self.rel_of(local),
                SnapshotStamp {
                    repo: r.id.clone(),
                    value,
                    checked: now_secs(),
                },
            );
            return Ok(Some(Located {
                path: local.to_path_buf(),
                downloaded: true,
                repo: Some(r.id.clone()),
            }));
        }
        self.index.set_snapshot(
            &self.rel_of(local),
            SnapshotStamp {
                checked: now_secs(),
                ..stamp
            },
        );
        Ok(None)
    }

    /// Fetch from the repo recorded in the lockfile (no probing); falls back to normal lookup.
    pub fn get_locked(
        &self,
        repo: Option<&str>,
        file: &ArtifactFile,
        sha: Option<&str>,
    ) -> Result<Option<Located>> {
        let ArtifactFile {
            group,
            artifact,
            version,
            classifier,
            ext,
        } = *file;
        let rel = Self::relative_path(group, artifact, version, classifier, ext);
        let local = self.local.join(&rel);
        if local.is_file() {
            self.cached.fetch_add(1, Ordering::Relaxed);
            self.progress();
            return Ok(Some(Located {
                path: local,
                downloaded: false,
                repo: self.origin(&rel),
            }));
        }
        if let Some(hit) = self.read_only_hit(&rel, is_snapshot(version)) {
            return Ok(Some(hit));
        }
        let known = repo.and_then(|id| {
            self.remotes
                .read()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned()
        });
        if let Some(r) = known
            && self.download(&r, &rel, &local, sha)?
        {
            return Ok(Some(self.downloaded_from(&r, &rel, local)));
        }
        let loc = self.get(group, artifact, version, classifier, ext)?;
        if let (Some(l), Some(want)) = (&loc, sha)
            && l.downloaded
            && sha256_file(&l.path)? != want
        {
            bail!("{}: sha256 differs from lockfile", l.path.display());
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
        if !self.tty.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut last) = self.last_draw.try_lock() else {
            return;
        };
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

    /// Stops drawing the status line, for good: a single line that rewrites itself cannot share the terminal with
    /// the output of modules built side by side.
    pub fn stop_progress(&self) {
        self.clear_progress();
        self.tty.store(false, Ordering::Relaxed);
    }

    pub fn clear_progress(&self) {
        if self.tty.load(Ordering::Relaxed) {
            eprint!("\r\x1b[2K");
        }
    }

    /// One-line account of where files came from and how many requests were saved.
    pub fn summary(&self) -> String {
        self.clear_progress();
        let n = self.downloaded.load(Ordering::Relaxed);
        let from = self
            .by_repo
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let dl = if n == 0 {
            "nothing downloaded".to_string()
        } else {
            format!(
                "{n} files downloaded ({}: {from})",
                human(self.bytes.load(Ordering::Relaxed))
            )
        };
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

    fn download(
        &self,
        r: &Remote,
        rel: &str,
        dest: &Path,
        expect_sha: Option<&str>,
    ) -> Result<bool> {
        Ok(matches!(
            self.fetch(r, rel, dest, expect_sha, None)?,
            Fetched::Got(_)
        ))
    }

    fn fetch(
        &self,
        r: &Remote,
        rel: &str,
        dest: &Path,
        expect_sha: Option<&str>,
        held: Option<&Validators>,
    ) -> Result<Fetched> {
        let url = format!("{}/{rel}", r.url);
        self.requests.fetch_add(1, Ordering::Relaxed);
        let cred = self.auth.read().unwrap().get(&r.id).cloned();
        let mut req = self.agent.get(&url);
        if let Some(c) = &cred {
            req = req.header(
                reqwest::header::AUTHORIZATION,
                self.authorization(r, &url, c)?,
            );
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
                bail!(
                    "{url}: HTTP {code}; check credentials for repository '{}' (settings.xml <server> or rayvn.toml [[repo]])",
                    r.id
                )
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
        *self
            .by_repo
            .lock()
            .unwrap()
            .entry(r.id.clone())
            .or_default() += 1;
        self.progress();
        Ok(Fetched::Got(validators))
    }

    /// The `Authorization` header for `r`; refuses plain http unless the repository is listed in `allow_http_auth`.
    fn authorization(
        &self,
        r: &Remote,
        url: &str,
        cred: &Cred,
    ) -> Result<reqwest::header::HeaderValue> {
        if url.starts_with("http://") && !self.allow_http_auth.contains(&r.id) {
            bail!(
                "refusing to send credentials for repository '{}' over plain http ({}); use https or add the id to allow_http_auth in rayvn.toml",
                r.id,
                r.url
            );
        }
        let mut v = reqwest::header::HeaderValue::from_str(cred.get(&r.id)?.expose())
            .context("credential is not a valid header value")?;
        v.set_sensitive(true);
        Ok(v)
    }
}

enum Fetched {
    Got(Validators),
    NotModified,
    Missing,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    use super::*;

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
                    Some((_, body)) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        url
    }

    /// A `Repos` over one repository `id` at `url`, caching in `cache`.
    fn repos_for(cache: &Path, id: &str, url: &str) -> Repos {
        let settings = Settings {
            local_repo: Some(cache.to_path_buf()),
            ..Settings::default()
        };
        let config = Config {
            central_second: Some(false),
            default_hints: Some(false),
            ..Config::default()
        };
        Repos::new(
            &[RepoDecl {
                id: id.into(),
                url: url.into(),
            }],
            &settings,
            &config,
            false,
            false,
        )
        .unwrap()
    }

    fn fresh_cache(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rvn-cache-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const LIB: &[(&str, &str)] = &[("/g/lib/1.0/lib-1.0.jar", "jar-bytes")];

    #[test]
    fn warm_cache_hits_still_report_the_repository_they_came_from() {
        let url = serve(LIB);
        let cache = fresh_cache("origin-get");

        let cold = repos_for(&cache, "mine", &url)
            .get("g", "lib", "1.0", None, "jar")
            .unwrap()
            .unwrap();
        assert_eq!(
            (cold.downloaded, cold.repo.as_deref()),
            (true, Some("mine"))
        );

        // A new process: same cache directory, nothing in memory.
        let warm = repos_for(&cache, "mine", &url)
            .get("g", "lib", "1.0", None, "jar")
            .unwrap()
            .unwrap();
        assert_eq!(
            (warm.downloaded, warm.repo.as_deref()),
            (false, Some("mine"))
        );
    }

    #[test]
    fn warm_locked_lookups_report_the_repository_too() {
        let url = serve(LIB);
        let cache = fresh_cache("origin-locked");

        let cold = repos_for(&cache, "mine", &url)
            .get_locked(
                Some("mine"),
                &ArtifactFile {
                    group: "g",
                    artifact: "lib",
                    version: "1.0",
                    classifier: None,
                    ext: "jar",
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(cold.repo.as_deref(), Some("mine"));
        let warm = repos_for(&cache, "mine", &url)
            .get_locked(
                Some("mine"),
                &ArtifactFile {
                    group: "g",
                    artifact: "lib",
                    version: "1.0",
                    classifier: None,
                    ext: "jar",
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            (warm.downloaded, warm.repo.as_deref()),
            (false, Some("mine"))
        );
    }

    #[test]
    fn files_rayvn_did_not_download_have_no_known_origin() {
        let cache = fresh_cache("origin-unknown");
        let jar = cache.join("g/lib/1.0/lib-1.0.jar");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, "x").unwrap();
        let loc = repos_for(&cache, "mine", "http://127.0.0.1:1")
            .get("g", "lib", "1.0", None, "jar")
            .unwrap()
            .unwrap();
        assert_eq!((loc.downloaded, loc.repo), (false, None));
    }
}
