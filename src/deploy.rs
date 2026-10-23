//! `rvn deploy`: uploads staged artifacts and checksums to a remote Maven repository via HTTP PUT.

use crate::cli::Args;
use crate::model::Effective;
use crate::repos::auth::Cred;
use crate::repos::{HostLimiter, host_of};
use crate::settings::{Config, Settings};
use crate::version::is_snapshot;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The target remote repository for deployment.
#[derive(Debug, Clone)]
pub struct TargetRepo {
    pub id: String,
    pub url: String,
}

/// Resolves the destination repository from `--alt-repo`, `rayvn.toml [deploy]`, or POM `<distributionManagement>`.
pub fn resolve_target_repo(
    args: &Args,
    config: &Config,
    root_effective: &Effective,
) -> Result<TargetRepo> {
    // 1. --alt-repo flag
    if let Some(alt) = &args.alt_repo {
        if let Some((id, url)) = alt.split_once("::") {
            return Ok(TargetRepo {
                id: id.trim().to_string(),
                url: url.trim().trim_end_matches('/').to_string(),
            });
        } else {
            let url = alt.trim().trim_end_matches('/').to_string();
            let host = host_of(&url);
            return Ok(TargetRepo { id: host, url });
        }
    }

    // 2. rayvn.toml [deploy]
    if let Some(deploy) = &config.deploy
        && let Some(url) = &deploy.url
    {
        let id = deploy.id.clone().unwrap_or_else(|| host_of(url));
        return Ok(TargetRepo {
            id,
            url: url.trim_end_matches('/').to_string(),
        });
    }

    // 3. POM <distributionManagement>
    if let Some(dist) = &root_effective.distribution {
        let is_snap = is_snapshot(&root_effective.gav.version);
        let repo = if is_snap && dist.snapshot_repository.is_some() {
            dist.snapshot_repository.as_ref()
        } else {
            dist.repository
                .as_ref()
                .or(dist.snapshot_repository.as_ref())
        };
        if let Some(r) = repo {
            return Ok(TargetRepo {
                id: r.id.clone(),
                url: r.url.trim_end_matches('/').to_string(),
            });
        }
    }

    bail!(
        "no deployment repository specified; configure `<distributionManagement>` in pom.xml, `[deploy]` in rayvn.toml, or pass `--alt-repo <id::url>`"
    )
}

/// Resolves credentials for `target`: checks `RAYVN_DEPLOY_*` env vars, `config.deploy`, `config.repo`, then `settings.xml`.
pub fn resolve_deploy_auth(
    target: &TargetRepo,
    config: &Config,
    settings: &Settings,
) -> Result<Option<Cred>> {
    // 1. Direct env vars RAYVN_DEPLOY_USER & RAYVN_DEPLOY_PASSWORD
    if let (Ok(u), Ok(p)) = (
        std::env::var("RAYVN_DEPLOY_USER"),
        std::env::var("RAYVN_DEPLOY_PASSWORD"),
    ) {
        return Ok(Some(Cred::new(move || {
            Ok(crate::repos::auth::basic(&u, &p))
        })));
    }

    // 2. config.deploy env references
    if let Some(deploy) = &config.deploy {
        if let (Some(u_var), Some(p_var)) = (&deploy.user_env, &deploy.password_env)
            && let (Ok(u), Ok(p)) = (std::env::var(u_var), std::env::var(p_var))
        {
            return Ok(Some(Cred::new(move || {
                Ok(crate::repos::auth::basic(&u, &p))
            })));
        }
        if let Some(t_var) = &deploy.token_env
            && let Ok(token) = std::env::var(t_var)
        {
            return Ok(Some(Cred::new(move || {
                Ok(crate::secret::Secret::new(format!("Bearer {token}")))
            })));
        }
        if let (Some(u), Some(p_var)) = (&deploy.user, &deploy.password_env)
            && let Ok(p) = std::env::var(p_var)
        {
            let u = u.clone();
            return Ok(Some(Cred::new(move || {
                Ok(crate::repos::auth::basic(&u, &p))
            })));
        }
    }

    // 3. Fall back to repository credentials (rayvn.toml [[repo]] or settings.xml <server>)
    crate::repos::credentials_for(&target.id, &config.repo, &settings.servers)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("jar") => "application/java-archive",
        Some("pom") | Some("xml") => "application/xml",
        Some("sha1") | Some("sha256") | Some("md5") | Some("asc") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Uploads all files inside `stage_dir` to `target`.
pub fn upload_staged(
    stage_dir: &Path,
    target: &TargetRepo,
    auth: Option<&Cred>,
    limiter: &HostLimiter,
) -> Result<()> {
    let mut files = vec![];
    crate::build::walk(stage_dir, &mut files)?;
    if files.is_empty() {
        bail!("staging directory {} is empty", stage_dir.display());
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(32)
        .user_agent(concat!("rayvn/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client for deployment")?;

    let host = host_of(&target.url);
    eprintln!(
        "rvn: deploying {} files to {} ({})",
        files.len(),
        target.url,
        target.id
    );

    let auth_header = match auth {
        Some(c) => Some(
            reqwest::header::HeaderValue::from_str(c.get(&target.id)?.expose())
                .context("invalid authorization header")?,
        ),
        None => None,
    };

    let uploaded = AtomicUsize::new(0);

    for file in &files {
        let rel = file
            .strip_prefix(stage_dir)
            .with_context(|| format!("stripping prefix from {}", file.display()))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let remote_url = format!("{}/{}", target.url, rel_str);

        let body = std::fs::read(file)
            .with_context(|| format!("reading file for upload: {}", file.display()))?;
        let ctype = content_type(file);

        let _guard = limiter.acquire(&host);

        let mut last_err = None;

        for attempt in 1..=3 {
            let mut req = client
                .put(&remote_url)
                .header(reqwest::header::CONTENT_LENGTH, body.len())
                .header(reqwest::header::CONTENT_TYPE, ctype)
                .body(body.clone());

            if let Some(h) = &auth_header {
                req = req.header(reqwest::header::AUTHORIZATION, h.clone());
            }

            match req.send() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if attempt < 3
                        && (status == 429 || status == 502 || status == 503 || status == 504)
                    {
                        std::thread::sleep(Duration::from_millis(300 * attempt as u64));
                        continue;
                    }
                    if resp.status().is_success() {
                        uploaded.fetch_add(1, Ordering::Relaxed);
                        eprintln!("  uploaded {} (HTTP {status})", rel_str);
                        last_err = None;
                        break;
                    } else if status == 401 || status == 403 {
                        bail!(
                            "HTTP {status} deploying {rel_str} to {}; check credentials for server id '{}'",
                            target.url,
                            target.id
                        );
                    } else if status == 409 {
                        bail!(
                            "HTTP 409 Conflict deploying {rel_str} to {}; artifact already exists on remote repository",
                            target.url
                        );
                    } else {
                        bail!("HTTP {status} deploying {rel_str} to {}", target.url);
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 3 {
                        std::thread::sleep(Duration::from_millis(300 * attempt as u64));
                    }
                }
            }
        }

        if let Some(e) = last_err {
            bail!("failed to deploy {rel_str} to {}: {e:#}", target.url);
        }
    }

    eprintln!(
        "rvn: deployed {} files successfully to {}",
        uploaded.load(Ordering::Relaxed),
        target.url
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn alt_repo_parses_with_or_without_id() {
        let mut args = Args {
            cmd: crate::cli::Cmd::Deploy,
            pom_path: PathBuf::from("pom.xml"),
            locked: false,
            refresh: false,
            cold: false,
            test: false,
            rerun: false,
            test_cache: None,
            reports_dir: None,
            module: None,
            only: false,
            set_version: None,
            stage_dir: None,
            dry_run: false,
            alt_repo: Some("my-nexus::https://nexus.example.com/maven".into()),
            jobs: None,
        };
        let cfg = Config::default();
        let eff = Effective {
            gav: crate::pom::Gav::new("g", "a", "1"),
            parent: None,
            packaging: "jar".into(),
            props: Default::default(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: None,
            unrun: vec![],
            surefire: None,
            failsafe: None,
            relocation: None,
            distribution: None,
        };
        let t1 = resolve_target_repo(&args, &cfg, &eff).unwrap();
        assert_eq!(t1.id, "my-nexus");
        assert_eq!(t1.url, "https://nexus.example.com/maven");

        args.alt_repo = Some("https://repo.example.com/releases/".into());
        let t2 = resolve_target_repo(&args, &cfg, &eff).unwrap();
        assert_eq!(t2.id, "repo.example.com");
        assert_eq!(t2.url, "https://repo.example.com/releases");
    }

    #[test]
    fn distribution_management_snapshot_vs_release() {
        let args = Args {
            cmd: crate::cli::Cmd::Deploy,
            pom_path: PathBuf::from("pom.xml"),
            locked: false,
            refresh: false,
            cold: false,
            test: false,
            rerun: false,
            test_cache: None,
            reports_dir: None,
            module: None,
            only: false,
            set_version: None,
            stage_dir: None,
            dry_run: false,
            alt_repo: None,
            jobs: None,
        };
        let cfg = Config::default();
        let mut eff = Effective {
            gav: crate::pom::Gav::new("g", "a", "1.0-SNAPSHOT"),
            parent: None,
            packaging: "jar".into(),
            props: Default::default(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: None,
            unrun: vec![],
            surefire: None,
            failsafe: None,
            relocation: None,
            distribution: Some(crate::pom::DistributionManagement {
                repository: Some(crate::pom::RepoDecl {
                    id: "releases".into(),
                    url: "https://repo.example.com/releases".into(),
                }),
                snapshot_repository: Some(crate::pom::RepoDecl {
                    id: "snapshots".into(),
                    url: "https://repo.example.com/snapshots".into(),
                }),
            }),
        };
        let snap = resolve_target_repo(&args, &cfg, &eff).unwrap();
        assert_eq!(snap.id, "snapshots");
        assert_eq!(snap.url, "https://repo.example.com/snapshots");

        eff.gav.version = "1.0.0".into();
        let rel = resolve_target_repo(&args, &cfg, &eff).unwrap();
        assert_eq!(rel.id, "releases");
        assert_eq!(rel.url, "https://repo.example.com/releases");
    }

    #[test]
    fn upload_staged_sends_put_requests() {
        use std::collections::HashMap;
        use std::io::{BufRead, BufReader, Write};
        use std::sync::{Arc, Mutex};

        let uploaded = Arc::new(Mutex::new(Vec::new()));
        let up = uploaded.clone();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap_or(0);
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[0] == "PUT" {
                    up.lock().unwrap().push(parts[1].to_string());
                }
                let mut stream = &stream;
                let reply =
                    "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(reply.as_bytes());
            }
        });

        let temp = std::env::temp_dir().join(format!("rvn-deploy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("com/example/1.0")).unwrap();
        std::fs::write(temp.join("com/example/1.0/lib-1.0.jar"), b"jar bytes").unwrap();

        let target = TargetRepo {
            id: "test".into(),
            url,
        };
        let limiter = HostLimiter::new(4, HashMap::new());
        upload_staged(&temp, &target, None, &limiter).unwrap();

        let paths = uploaded.lock().unwrap().clone();
        assert_eq!(paths, vec!["/com/example/1.0/lib-1.0.jar"]);
        let _ = std::fs::remove_dir_all(&temp);
    }
}
