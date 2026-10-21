//! Configuration inputs: Maven's `~/.m2/settings.xml` (mirrors, servers, proxies, local repository) and
//! rayvn's own `~/.rayvn/config.toml` and `rayvn.toml` (repositories, routes, hints, cache options).

use crate::pom::{RepoDecl, child, text};
use anyhow::{Context, Result, bail};
use roxmltree::Document;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set"))
}

/// `$XDG_CACHE_HOME/rayvn/repository`, else `~/.cache/rayvn/repository` (also on macOS, like uv and cargo).
pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".cache"));
    base.join("rayvn/repository")
}

#[derive(Debug, Clone)]
pub struct Mirror {
    pub id: String,
    pub url: String,
    pub mirror_of: String,
}

#[derive(Debug, Default)]
pub struct Settings {
    pub repos: Vec<RepoDecl>,
    pub mirrors: Vec<Mirror>,
    /// Raw (unexpanded) username/password per server id; resolved only for servers actually used.
    pub servers: HashMap<String, (String, String)>,
    pub proxies: Vec<ProxyCfg>,
    pub local_repo: Option<PathBuf>,
}

/// An active `<proxy>`; username/password are raw and expanded only when the proxy is used.
#[derive(Debug, Clone)]
pub struct ProxyCfg {
    pub id: String,
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub non_proxy_hosts: Option<String>,
}

fn expand_with(s: &str, strict: bool) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else { break };
        let key = &rest[i + 2..i + j];
        let val = if let Some(e) = key.strip_prefix("env.") {
            std::env::var(e).ok()
        } else if key == "user.home" {
            Some(home().to_string_lossy().into_owned())
        } else {
            None
        };
        match val {
            Some(v) => out.push_str(&v),
            None if strict => bail!("settings.xml references ${{{key}}} but it is not set"),
            None => out.push_str(&rest[i..=i + j]),
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand(s: &str) -> String {
    expand_with(s, false).expect("lenient expansion cannot fail")
}

/// Expands `${env.X}` / `${user.home}`; errors on anything unresolved. For credentials only.
///
/// ```
/// use rayvn::settings::expand_strict;
/// assert_eq!(expand_strict("plain").unwrap(), "plain");
/// assert!(expand_strict("${env.RAYVN_DOCTEST_SURELY_UNSET}").is_err());
/// ```
pub fn expand_strict(s: &str) -> Result<String> {
    expand_with(s, true)
}

pub fn load() -> Result<(Settings, Vec<PathBuf>)> {
    let path = home().join(".m2/settings.xml");
    if !path.is_file() {
        let local_repo = std::env::var("RAYVN_LOCAL_REPO").ok().map(PathBuf::from);
        return Ok((Settings { local_repo, ..Default::default() }, vec![]));
    }
    let xml = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc = Document::parse(&xml).context("invalid settings.xml")?;
    let root = doc.root_element();
    let mut s = Settings::default();

    s.local_repo = std::env::var("RAYVN_LOCAL_REPO")
        .ok()
        .map(PathBuf::from)
        .or_else(|| text(root, "localRepository").map(|p| PathBuf::from(expand(&p))));

    let active: Vec<String> = child(root, "activeProfiles")
        .map(|a| a.children().filter(|c| c.is_element()).filter_map(|c| c.text().map(|t| t.trim().to_string())).collect())
        .unwrap_or_default();

    if let Some(profiles) = child(root, "profiles") {
        for p in profiles.children().filter(|c| c.is_element()) {
            let id = text(p, "id").unwrap_or_default();
            let default_on = child(p, "activation")
                .and_then(|a| text(a, "activeByDefault"))
                .as_deref()
                == Some("true");
            if !(active.contains(&id) || default_on) {
                continue;
            }
            if let Some(repos) = child(p, "repositories") {
                for r in repos.children().filter(|c| c.is_element()) {
                    if let (Some(id), Some(url)) = (text(r, "id"), text(r, "url")) {
                        s.repos.push(RepoDecl { id, url: expand(&url) });
                    }
                }
            }
        }
    }

    if let Some(ms) = child(root, "mirrors") {
        for m in ms.children().filter(|c| c.is_element()) {
            if let (Some(id), Some(url), Some(of)) = (text(m, "id"), text(m, "url"), text(m, "mirrorOf")) {
                s.mirrors.push(Mirror { id, url: expand(&url), mirror_of: of });
            }
        }
    }

    if let Some(ss) = child(root, "servers") {
        for sv in ss.children().filter(|c| c.is_element()) {
            if let (Some(id), Some(u), Some(p)) = (text(sv, "id"), text(sv, "username"), text(sv, "password")) {
                s.servers.insert(id, (u, p));
            }
        }
    }

    if let Some(ps) = child(root, "proxies") {
        for p in ps.children().filter(|c| c.is_element()) {
            if text(p, "active").as_deref() == Some("false") {
                continue;
            }
            let (Some(host), id) = (text(p, "host"), text(p, "id").unwrap_or_default()) else { continue };
            let port = match text(p, "port") {
                Some(v) => v.parse().with_context(|| format!("proxy '{id}': invalid port '{v}'"))?,
                None => 8080,
            };
            s.proxies.push(ProxyCfg {
                id,
                protocol: text(p, "protocol").unwrap_or_else(|| "http".into()).to_ascii_lowercase(),
                host,
                port,
                username: text(p, "username"),
                password: text(p, "password"),
                non_proxy_hosts: text(p, "nonProxyHosts"),
            });
        }
    }
    Ok((s, vec![path]))
}

impl Mirror {
    pub fn matches(&self, repo_id: &str, url: &str) -> bool {
        let mut hit = false;
        for term in self.mirror_of.split(',').map(str::trim) {
            if let Some(neg) = term.strip_prefix('!') {
                if neg == repo_id {
                    return false;
                }
            } else if term == "*" || term == repo_id {
                hit = true;
            } else if term == "external:*" && !url.contains("localhost") && !url.contains("127.0.0.1") && !url.starts_with("file:") {
                hit = true;
            }
        }
        hit
    }
}

/// Optional rayvn-specific config (`~/.rayvn/config.toml`, then `./rayvn.toml`).
/// Routes restrict which remote repositories are asked for which groupIds.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Repo ids that may receive credentials over plain http.
    #[serde(default)]
    pub allow_http_auth: Vec<String>,
    #[serde(default)]
    pub route: Vec<Route>,
    #[serde(default)]
    pub repo: Vec<RepoCfg>,
    /// Soft routes: matching repositories are asked first, the rest still follow.
    #[serde(default)]
    pub hint: Vec<Hint>,
    /// Set to false to ignore the shipped hint list.
    #[serde(default)]
    pub default_hints: Option<bool>,
    /// Ask Central right after hinted repositories, before the other declared ones (default true).
    #[serde(default)]
    pub central_second: Option<bool>,
    /// How long a 404 is remembered, in hours (default 24; 0 turns the miss cache off).
    #[serde(default)]
    pub miss_ttl_hours: Option<u64>,
    /// How often a SNAPSHOT that rayvn downloaded is revalidated, in hours (default 24; 0 = every run).
    #[serde(default)]
    pub snapshot_ttl_hours: Option<u64>,
    /// `[test]`: caching and sharding of `rvn test` / `rvn verify`.
    #[serde(default)]
    pub test: TestSettings,
    /// Extra cache directories that are read but never written (also `RAYVN_RO_REPO`, `:`-separated).
    #[serde(default)]
    pub read_only_repos: Vec<String>,
}

/// `[test]` in `rayvn.toml`. Every field is optional; a project file overrides the user file.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct TestSettings {
    /// Skip unit tests whose inputs are unchanged since a pass (default true).
    pub cache: Option<bool>,
    /// The same for integration tests (default false: they usually read files rayvn cannot see).
    pub cache_integration: Option<bool>,
    /// Run the selected test classes in this many JVMs at once (default 1).
    pub shards: Option<usize>,
    /// Where results are stored; relative to the project. `--test-cache` and `RAYVN_TEST_CACHE` win over it.
    pub cache_dir: Option<String>,
    /// Ant-style globs, relative to the project, of extra files that can change a test's result.
    #[serde(default)]
    pub inputs: Vec<String>,
}

impl TestSettings {
    /// `self` overridden by `later`: its set fields win, input globs add up.
    pub fn overridden_by(mut self, later: TestSettings) -> TestSettings {
        self.cache = later.cache.or(self.cache);
        self.cache_integration = later.cache_integration.or(self.cache_integration);
        self.shards = later.shards.or(self.shards);
        self.cache_dir = later.cache_dir.or(self.cache_dir);
        self.inputs.extend(later.inputs);
        self
    }
}

/// Extra repository, optionally authenticated from environment variables.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoCfg {
    pub id: String,
    pub url: String,
    /// Names of environment variables holding the credentials (basic: user + password; bearer: token).
    pub user_env: Option<String>,
    pub password_env: Option<String>,
    pub token_env: Option<String>,
    /// Command (argv, no shell) whose stdout is a bearer token. User-level config only.
    pub token_cmd: Option<Vec<String>>,
    /// Command (argv, no shell) whose stdout is the basic-auth password; needs `user` or `user_env`. User-level config only.
    pub password_cmd: Option<Vec<String>>,
    /// Literal username for `password_cmd` (not a secret).
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub repo: String,
    pub groups: Vec<String>,
}

/// A soft route: names a repository by id or URL, and the groupIds it is likely to serve.
#[derive(Debug, Clone, Deserialize)]
pub struct Hint {
    pub repo: Option<String>,
    pub url: Option<String>,
    pub groups: Vec<String>,
}

impl Hint {
    pub fn specificity(&self, group: &str) -> Option<usize> {
        group_specificity(&self.groups, group)
    }

    pub fn names(&self, id: &str, url: &str) -> bool {
        self.repo.as_deref() == Some(id) || self.url.as_deref().is_some_and(|u| u.trim_end_matches('/') == url.trim_end_matches('/'))
    }
}

/// The shipped suggestion list, compiled in.
pub fn default_hints() -> Vec<Hint> {
    #[derive(Deserialize)]
    struct File {
        hint: Vec<Hint>,
    }
    toml::from_str::<File>(include_str!("../data/default-hints.toml")).expect("data/default-hints.toml is valid").hint
}

impl Route {
    /// How specifically this route matches `group` (higher wins); `None` if it doesn't match.
    pub fn specificity(&self, group: &str) -> Option<usize> {
        group_specificity(&self.groups, group)
    }
}

/// `*` matches everything, `a.b.*` matches `a.b` and below, anything else matches exactly. Longer is more specific.
fn group_specificity(patterns: &[String], group: &str) -> Option<usize> {
    patterns
        .iter()
        .filter_map(|p| {
            if p == "*" {
                Some(1)
            } else if let Some(prefix) = p.strip_suffix(".*") {
                (group == prefix || group.starts_with(&format!("{prefix}."))).then(|| 2 + prefix.len())
            } else {
                (p == group).then(|| 10_000 + p.len())
            }
        })
        .max()
}

pub fn load_config(project_dir: &Path) -> Result<(Config, Vec<PathBuf>)> {
    let mut cfg = Config::default();
    let mut inputs = vec![];
    // Project routes take precedence, so they come first.
    let user_cfg = home().join(".rayvn/config.toml");
    // User config first: its credentials win over a project file's CI-oriented env refs.
    for p in [user_cfg.clone(), project_dir.join("rayvn.toml")] {
        if p.is_file() {
            let c: Config = toml::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing {}", p.display()))?;
            // A project file can come from an untrusted checkout, so it must not run commands.
            if p != user_cfg {
                if let Some(r) = c.repo.iter().find(|r| r.token_cmd.is_some() || r.password_cmd.is_some()) {
                    bail!("{}: repository '{}' uses token_cmd/password_cmd, which is only allowed in {}", p.display(), r.id, user_cfg.display());
                }
            }
            cfg.route.extend(c.route);
            cfg.hint.extend(c.hint);
            cfg.default_hints = cfg.default_hints.or(c.default_hints);
            cfg.central_second = cfg.central_second.or(c.central_second);
            cfg.miss_ttl_hours = cfg.miss_ttl_hours.or(c.miss_ttl_hours);
            cfg.snapshot_ttl_hours = cfg.snapshot_ttl_hours.or(c.snapshot_ttl_hours);
            cfg.test = std::mem::take(&mut cfg.test).overridden_by(c.test);
            cfg.read_only_repos.extend(c.read_only_repos);
            cfg.repo.extend(c.repo);
            cfg.allow_http_auth.extend(c.allow_http_auth);
            inputs.push(p);
        }
    }
    Ok((cfg, inputs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_later_file_wins_and_inputs_add_up() {
        let user = TestSettings { cache: Some(true), shards: Some(2), inputs: vec!["a".into()], ..Default::default() };
        let project = TestSettings { cache: Some(false), cache_integration: Some(true), inputs: vec!["b".into()], ..Default::default() };
        let merged = user.overridden_by(project);
        assert_eq!((merged.cache, merged.cache_integration, merged.shards), (Some(false), Some(true), Some(2)));
        assert_eq!(merged.inputs, ["a", "b"]);
    }

    #[test]
    fn test_section_parses_from_toml() {
        let c: Config = toml::from_str("[test]\ncache_integration = true\nshards = 4\ninputs = [\"test/input/**\"]\n").unwrap();
        assert_eq!((c.test.cache, c.test.cache_integration, c.test.shards), (None, Some(true), Some(4)));
        assert_eq!(c.test.inputs, ["test/input/**"]);
    }
}
