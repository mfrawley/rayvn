# Credentials for private repositories

raven never stores secrets. A credential is read from an environment variable or produced by a command, and only when a request to that repository needs it (once per process). Cached and locked runs that need no download never ask.

## Where repositories and credentials are declared

`~/.raven/config.toml` (user) and `raven.toml` (project) take `[[repo]]` entries; `~/.m2/settings.xml` `<server>` entries also work. For the same repository id, `config.toml`/`raven.toml` wins over `settings.xml`. Credentials are sent preemptively (GitHub Packages needs this), and only over https unless the id is listed in `allow_http_auth`.

## `[[repo]]` credential options

Use exactly one combination per repository:

| Option(s) | Sends | Notes |
|---|---|---|
| `token_env = "VAR"` | `Bearer $VAR` | |
| `user_env = "U"` + `password_env = "P"` | Basic | **Both variables must be set and non-empty**; otherwise raven stops with `repository 'ID': environment variable U is not set or empty` |
| `token_cmd = ["prog", "arg"]` | `Bearer <stdout>` | user-level `config.toml` only |
| `password_cmd = ["prog", "arg"]` + `user = "name"` | Basic | user-level `config.toml` only; `user` is a literal, not a secret |
| `password_cmd = [...]` + `user_env = "U"` | Basic | as above, username from the environment |

Commands are an argv list, run without a shell, with stdin closed; stdout is trimmed and must not be empty. Project `raven.toml` may use the `*_env` forms only, so a committed file never executes commands. The user config wins over a project entry with the same id.

```toml
# ~/.raven/config.toml: no variable needs to be set
[[repo]]
id = "github"
url = "https://maven.pkg.github.com/ORG/REPO"
user = "my-github-login"
password_cmd = ["op", "read", "op://Private/github-pat/credential"]
```

```toml
# CI: variables supplied by the runner
[[repo]]
id = "github"
url = "https://maven.pkg.github.com/ORG/REPO"
user_env = "GITHUB_ACTOR"
password_env = "GITHUB_TOKEN"
```

## Common errors

- `environment variable USERNAME is not set or empty`: an entry uses `user_env = "USERNAME"` (or `password_env`) and the variable is missing. macOS and most Linux shells don't set `USERNAME`. Export it for the run, or switch to `user = "..."` with `password_cmd`.
- `settings.xml server username` / `password`: a `${env.X}` reference in a `<server>` is unset. raven fails before sending anything; Maven would send the literal `${env.X}`.
- `use exactly one of ...`: the entry mixes option families.
- `refusing to send credentials ... over plain http`: use https or add the id to `allow_http_auth`.
- Encrypted `settings.xml` passwords (`{...}`) are not supported; requests go unauthenticated with a warning.

## Why a 404 can mean "no credentials"

Private registries often answer 404 to anonymous requests. The miss cache therefore keys on whether the request was authenticated, so fixing credentials takes effect immediately, with no need for `--refresh`.
