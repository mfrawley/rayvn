//! Credentials for repositories: `config.toml` / `rayvn.toml` sources and `settings.xml` servers, resolved on first use.

use crate::secret::Secret;
use crate::settings::expand_strict;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::collections::HashMap;
use std::sync::OnceLock;

/// rayvn.toml wins over settings.xml `<server>`.
pub(super) fn credentials_for(
    id: &str,
    config_repos: &[crate::settings::RepoCfg],
    servers: &HashMap<String, (String, String)>,
) -> Result<Option<Cred>> {
    for c in config_repos.iter().filter(|c| c.id == id) {
        if let Some(h) = config_auth(c)? {
            return Ok(Some(h));
        }
    }
    if let Some((u, p)) = servers.get(id) {
        if p.starts_with('{') && p.ends_with('}') {
            eprintln!(
                "warning: server '{id}' uses an encrypted password; unsupported, requests will be unauthenticated"
            );
            return Ok(None);
        }
        let (u, p) = (u.clone(), p.clone());
        return Ok(Some(Cred::new(move || {
            Ok(basic(
                &expand_strict(&u).context("settings.xml server username")?,
                &expand_strict(&p).context("settings.xml server password")?,
            ))
        })));
    }
    Ok(None)
}

pub(super) fn basic(user: &str, pass: &str) -> Secret {
    Secret::new(format!(
        "Basic {}",
        STANDARD.encode(format!("{user}:{pass}"))
    ))
}

/// A credential produced on first use (once per process), so cached or locked runs never need it.
pub(super) struct Cred {
    init: Box<dyn Fn() -> Result<Secret> + Send + Sync>,
    cell: OnceLock<std::result::Result<Secret, String>>,
}

impl Cred {
    pub(super) fn new(f: impl Fn() -> Result<Secret> + Send + Sync + 'static) -> Self {
        Cred {
            init: Box::new(f),
            cell: OnceLock::new(),
        }
    }

    pub(super) fn get(&self, repo: &str) -> Result<&Secret> {
        self.cell
            .get_or_init(|| (self.init)().map_err(|e| format!("repository '{repo}': {e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

pub(super) fn env_var(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => bail!("environment variable {name} is not set or empty"),
    }
}

pub(super) fn run_helper(argv: &[String], user: Option<&str>) -> Result<Secret> {
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
    let value = String::from_utf8(out.stdout)
        .with_context(|| format!("credential command '{prog}' printed non-UTF-8"))?;
    let value = value.trim();
    if value.is_empty() {
        bail!("credential command '{prog}' printed nothing");
    }
    Ok(match user {
        Some(u) => basic(u, value),
        None => Secret::new(format!("Bearer {value}")),
    })
}

/// Where a repository's credential comes from, as configured in `config.toml` / `rayvn.toml`.
#[derive(Debug, PartialEq)]
pub(super) enum CredSource {
    BearerEnv(String),
    BasicEnv {
        user_env: String,
        password_env: String,
    },
    BearerCmd(Vec<String>),
    PasswordCmd {
        argv: Vec<String>,
        user: UserName,
    },
}

/// The username that goes with a `password_cmd`.
#[derive(Debug, PartialEq)]
pub(super) enum UserName {
    Literal(String),
    Env(String),
}

/// `None` when the repository configures no credentials; an error when the combination is ambiguous.
pub(super) fn cred_source(c: &crate::settings::RepoCfg) -> Result<Option<CredSource>> {
    let bad = || {
        anyhow::anyhow!(
            "repository '{}': use exactly one of token_env, user_env+password_env, token_cmd, or password_cmd+user",
            c.id
        )
    };
    match (
        &c.token_env,
        &c.user_env,
        &c.password_env,
        &c.token_cmd,
        &c.password_cmd,
    ) {
        (None, None, None, None, None) if c.user.is_none() => Ok(None),
        (Some(t), None, None, None, None) if c.user.is_none() => {
            Ok(Some(CredSource::BearerEnv(t.clone())))
        }
        (None, Some(u), Some(p), None, None) if c.user.is_none() => {
            Ok(Some(CredSource::BasicEnv {
                user_env: u.clone(),
                password_env: p.clone(),
            }))
        }
        (None, None, None, Some(argv), None) if c.user.is_none() => {
            Ok(Some(CredSource::BearerCmd(argv.clone())))
        }
        (None, env, None, None, Some(argv)) => {
            let user = match (&c.user, env) {
                (Some(u), None) => UserName::Literal(u.clone()),
                (None, Some(e)) => UserName::Env(e.clone()),
                _ => return Err(bad()),
            };
            Ok(Some(CredSource::PasswordCmd {
                argv: argv.clone(),
                user,
            }))
        }
        _ => Err(bad()),
    }
}

impl CredSource {
    pub(super) fn into_cred(self) -> Cred {
        match self {
            CredSource::BearerEnv(t) => {
                Cred::new(move || Ok(Secret::new(format!("Bearer {}", env_var(&t)?))))
            }
            CredSource::BasicEnv {
                user_env,
                password_env,
            } => Cred::new(move || Ok(basic(&env_var(&user_env)?, &env_var(&password_env)?))),
            CredSource::BearerCmd(argv) => Cred::new(move || run_helper(&argv, None)),
            CredSource::PasswordCmd { argv, user } => Cred::new(move || {
                let user = match &user {
                    UserName::Literal(u) => u.clone(),
                    UserName::Env(e) => env_var(e)?,
                };
                run_helper(&argv, Some(&user))
            }),
        }
    }
}

pub(super) fn config_auth(c: &crate::settings::RepoCfg) -> Result<Option<Cred>> {
    Ok(cred_source(c)?.map(CredSource::into_cred))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(f: impl FnOnce(&mut crate::settings::RepoCfg)) -> crate::settings::RepoCfg {
        let mut c = crate::settings::RepoCfg {
            id: "r".into(),
            url: "https://r.example".into(),
            user_env: None,
            password_env: None,
            token_env: None,
            token_cmd: None,
            password_cmd: None,
            user: None,
        };
        f(&mut c);
        c
    }

    #[test]
    fn each_credential_form_has_its_own_source() {
        let s = |c| cred_source(&c).unwrap();
        assert_eq!(s(cfg(|_| {})), None);
        assert_eq!(
            s(cfg(|c| c.token_env = Some("T".into()))),
            Some(CredSource::BearerEnv("T".into()))
        );
        assert_eq!(
            s(cfg(|c| {
                c.user_env = Some("U".into());
                c.password_env = Some("P".into());
            })),
            Some(CredSource::BasicEnv {
                user_env: "U".into(),
                password_env: "P".into()
            })
        );
        assert_eq!(
            s(cfg(|c| c.token_cmd = Some(vec!["t".into()]))),
            Some(CredSource::BearerCmd(vec!["t".into()]))
        );
        let with_cmd = |c: &mut crate::settings::RepoCfg| c.password_cmd = Some(vec!["p".into()]);
        assert_eq!(
            s(cfg(|c| {
                with_cmd(c);
                c.user = Some("me".into());
            })),
            Some(CredSource::PasswordCmd {
                argv: vec!["p".into()],
                user: UserName::Literal("me".into())
            })
        );
        assert_eq!(
            s(cfg(|c| {
                with_cmd(c);
                c.user_env = Some("U".into());
            })),
            Some(CredSource::PasswordCmd {
                argv: vec!["p".into()],
                user: UserName::Env("U".into())
            })
        );
    }

    #[test]
    fn ambiguous_or_incomplete_credentials_are_rejected_naming_the_repo() {
        let with_cmd = |c: &mut crate::settings::RepoCfg| c.password_cmd = Some(vec!["p".into()]);
        let bad: Vec<crate::settings::RepoCfg> = vec![
            cfg(|c| {
                c.token_env = Some("T".into());
                c.token_cmd = Some(vec!["t".into()]);
            }),
            cfg(|c| c.user_env = Some("U".into())),
            cfg(|c| c.user = Some("me".into())),
            cfg(with_cmd),
            cfg(|c| {
                with_cmd(c);
                c.user = Some("me".into());
                c.user_env = Some("U".into());
            }),
        ];
        for c in bad {
            let e = cred_source(&c).unwrap_err().to_string();
            assert!(e.contains("repository 'r'"), "{e}");
        }
    }
}
