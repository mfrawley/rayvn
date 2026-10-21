//! Reading a single `pom.xml` as written: coordinates, properties, dependencies, profiles and the plugin
//! settings rayvn understands. Inheritance and `${...}` interpolation happen later, in [`crate::model`].

use anyhow::{Context, Result};
use roxmltree::{Document, Node, ParsingOptions};
use std::collections::BTreeMap;

/// Maven coordinates: group, artifact and version.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Gav {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Gav {
    pub fn new(g: &str, a: &str, v: &str) -> Self {
        Gav {
            group: g.into(),
            artifact: a.into(),
            version: v.into(),
        }
    }
}

impl std::fmt::Display for Gav {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.group, self.artifact, self.version)
    }
}

/// The classifier an artifact of `typ` is stored under: `test-jar` is `tests` unless one is given.
/// [Maven: type `test-jar`](https://maven.apache.org/ref/current/maven-core/artifact-handlers.html)
pub fn stored_classifier<'a>(typ: &str, classifier: Option<&'a str>) -> Option<&'a str> {
    classifier.or(if typ == "test-jar" {
        Some("tests")
    } else {
        None
    })
}

/// The file extension of an artifact of `typ`: `bundle`, `maven-plugin` and `test-jar` are all plain jars.
pub fn extension_of(typ: &str) -> &str {
    match typ {
        "bundle" | "maven-plugin" | "test-jar" => "jar",
        other => other,
    }
}

/// A `<dependency>` as written.
/// [Dependency element reference](https://maven.apache.org/pom.html#dependencies)
#[derive(Clone, Debug, Default)]
pub struct Dep {
    pub group: String,
    pub artifact: String,
    /// Absent when managed by `<dependencyManagement>`.
    pub version: Option<String>,
    /// `compile` (default), `provided`, `runtime`, `test`, `system` or `import`; decides which classpaths see it and
    /// whether it is transitive. [Scopes](https://maven.apache.org/guides/introduction/introduction-to-dependency-mechanism.html#dependency-scope)
    pub scope: Option<String>,
    /// Optional dependencies are used by the declaring project but never passed on to its consumers.
    /// [Optional dependencies](https://maven.apache.org/guides/introduction/introduction-to-optional-and-excludes-dependencies.html)
    pub optional: bool,
    /// Selects a variant of the artifact, `<artifact>-<version>-<classifier>.<ext>` (e.g. `tests`, `sources`).
    pub classifier: Option<String>,
    /// `<type>`, default `jar`; it implies extension and classifier (`test-jar` is `jar` with classifier `tests`).
    /// [Artifact handlers](https://maven.apache.org/ref/current/maven-core/artifact-handlers.html)
    pub typ: String,
    /// `(groupId, artifactId)` subtrees removed below this dependency, `*` allowed.
    /// [Exclusions](https://maven.apache.org/guides/introduction/introduction-to-optional-and-excludes-dependencies.html)
    pub exclusions: Vec<(String, String)>,
}

impl Dep {
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.group,
            self.artifact,
            self.typ,
            self.classifier.as_deref().unwrap_or("")
        )
    }
}

/// `<distributionManagement><relocation>`: the artifact moved. Fields left out keep the old value.
/// [Guide to relocation](https://maven.apache.org/guides/mini/guide-relocation.html)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Relocation {
    pub group: Option<String>,
    pub artifact: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RepoDecl {
    pub id: String,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct RawPom {
    pub parent: Option<ParentRef>,
    pub group: Option<String>,
    pub artifact: String,
    pub version: Option<String>,
    pub packaging: String,
    pub props: BTreeMap<String, String>,
    pub managed: Vec<Dep>,
    pub deps: Vec<Dep>,
    pub repos: Vec<RepoDecl>,
    pub modules: Vec<String>,
    pub relocation: Option<Relocation>,
    profiles: Vec<Profile>,
    pub shade: Option<ShadeCfg>,
    /// `maven-compiler-plugin` configuration.
    pub compiler: Option<CompilerCfg>,
    pub surefire: Option<TestCfg>,
    pub failsafe: Option<TestCfg>,
    /// Plugin executions in `<build><plugins>` that rayvn does not run, as `artifactId (goals at phase)`.
    pub unrun: Vec<String>,
}

/// What rayvn understands of `maven-surefire-plugin` (unit tests, phase `test`) and `maven-failsafe-plugin`
/// (integration tests, phases `integration-test` and `verify`). Values are uninterpolated.
/// [Surefire](https://maven.apache.org/surefire/maven-surefire-plugin/test-mojo.html),
/// [Failsafe](https://maven.apache.org/surefire/maven-failsafe-plugin/integration-test-mojo.html)
#[derive(Clone, Debug, Default)]
pub struct TestCfg {
    /// `<includes>`: ant-style globs over test source paths like `**/*Test.java`. Empty means the plugin's defaults
    /// (surefire: `Test*`, `*Test`, `*Tests`, `*TestCase`; failsafe: `IT*`, `*IT`, `*ITCase`).
    /// [Inclusions and exclusions](https://maven.apache.org/surefire/maven-surefire-plugin/examples/inclusion-exclusion.html)
    pub includes: Vec<String>,
    /// `<excludes>`: removed after the includes.
    pub excludes: Vec<String>,
    /// Raw `<skip>`, `<skipTests>`, `<skipITs>`, `<skipExec>` values, usually `${property}` references;
    /// any that interpolates to `true` skips the phase.
    /// [Skipping tests](https://maven.apache.org/surefire/maven-surefire-plugin/examples/skipping-tests.html)
    pub skips: Vec<String>,
    /// `<argLine>`: extra JVM arguments for the forked test JVM; `@{...}` late replacement is not supported.
    /// [Using argLine](https://maven.apache.org/surefire/maven-surefire-plugin/examples/jvm-args.html)
    pub arg_line: Option<String>,
    /// Config rayvn does not implement.
    pub ignored: Vec<String>,
}

fn plugin_config<'a, 'i>(root: Node<'a, 'i>, artifact: &str) -> Option<Option<Node<'a, 'i>>> {
    let plugin = child(root, "build")
        .and_then(|b| child(b, "plugins"))?
        .children()
        .find(|p| p.is_element() && text(*p, "artifactId").as_deref() == Some(artifact))?;
    let exec = child(plugin, "executions").and_then(|e| e.children().find(|x| x.is_element()));
    Some(
        exec.and_then(|e| child(e, "configuration"))
            .or_else(|| child(plugin, "configuration")),
    )
}

/// Plugins whose executions rayvn performs itself (see `docs/plugins.md`), or whose absence changes no build
/// output (`clean`, `install`, `deploy`, `site`).
const HANDLED_PLUGINS: [&str; 10] = [
    "maven-compiler-plugin",
    "maven-resources-plugin",
    "maven-jar-plugin",
    "maven-shade-plugin",
    "maven-surefire-plugin",
    "maven-failsafe-plugin",
    "maven-clean-plugin",
    "maven-install-plugin",
    "maven-deploy-plugin",
    "maven-site-plugin",
];

/// Executions bound to a lifecycle phase by a plugin that rayvn does not run, e.g. `exec-maven-plugin (exec at
/// generate-sources)`. A plugin that is only configured, with no `<execution>`, runs when someone calls its goal
/// by hand (`mvn exec:java`) and is not part of the build; neither is an execution with `<phase>none</phase>`.
fn parse_unrun(root: Node) -> Vec<String> {
    let Some(plugins) = child(root, "build").and_then(|b| child(b, "plugins")) else {
        return vec![];
    };
    let mut out = vec![];
    for plugin in plugins.children().filter(|p| p.is_element()) {
        let Some(artifact) = text(plugin, "artifactId") else {
            continue;
        };
        if HANDLED_PLUGINS.contains(&artifact.as_str()) {
            continue;
        }
        for exec in child(plugin, "executions")
            .into_iter()
            .flat_map(|e| e.children().filter(|x| x.is_element()))
        {
            let phase = text(exec, "phase");
            if phase.as_deref() == Some("none") {
                continue;
            }
            let goals: Vec<String> = child(exec, "goals")
                .map(|g| {
                    g.children()
                        .filter(|c| c.is_element())
                        .map(node_text)
                        .collect()
                })
                .unwrap_or_default();
            let goals = if goals.is_empty() {
                "default goals".to_string()
            } else {
                goals.join(", ")
            };
            out.push(match phase {
                Some(p) => format!("{artifact} ({goals} at {p})"),
                None => format!("{artifact} ({goals})"),
            });
        }
    }
    out
}

fn parse_tests(root: Node, artifact: &str) -> Option<TestCfg> {
    let conf = plugin_config(root, artifact)?;
    let mut cfg = TestCfg::default();
    let Some(conf) = conf else { return Some(cfg) };
    let list = |name: &str| {
        child(conf, name)
            .map(|n| {
                n.children()
                    .filter(|c| c.is_element())
                    .map(node_text)
                    .collect()
            })
            .unwrap_or_default()
    };
    cfg.includes = list("includes");
    cfg.excludes = list("excludes");
    cfg.skips = ["skip", "skipTests", "skipITs", "skipExec"]
        .iter()
        .filter_map(|k| text(conf, k))
        .collect();
    cfg.arg_line = text(conf, "argLine");
    for k in [
        "systemPropertyVariables",
        "environmentVariables",
        "groups",
        "excludedGroups",
        "forkCount",
        "parallel",
        "reuseForks",
        "properties",
        "dependenciesToScan",
        "additionalClasspathElements",
        "test",
    ] {
        if child(conf, k).is_some() {
            cfg.ignored.push(k.to_string());
        }
    }
    Some(cfg)
}

/// What rayvn understands of `maven-compiler-plugin`: the Java level. Values are uninterpolated.
/// [Compiler plugin](https://maven.apache.org/plugins/maven-compiler-plugin/compile-mojo.html)
#[derive(Clone, Debug, Default)]
pub struct CompilerCfg {
    /// `<release>`: passed as `javac --release`.
    pub release: Option<String>,
    /// `<source>`: the language level when there is no `<release>`.
    pub source: Option<String>,
    /// `<target>`: the bytecode level when there is no `<release>`.
    pub target: Option<String>,
}

fn parse_compiler(root: Node) -> Option<CompilerCfg> {
    let conf = plugin_config(root, "maven-compiler-plugin")??;
    Some(CompilerCfg {
        release: text(conf, "release"),
        source: text(conf, "source"),
        target: text(conf, "target"),
    })
}

/// What rayvn understands of `maven-shade-plugin`: fat jar, its name and attachment, services merge,
/// signature excludes, manifest.
/// [Shade plugin](https://maven.apache.org/plugins/maven-shade-plugin/shade-mojo.html)
#[derive(Clone, Debug, Default)]
pub struct ShadeCfg {
    /// `ServicesResourceTransformer`: merge `META-INF/services/*` files instead of keeping only the first.
    /// [Resource transformers](https://maven.apache.org/plugins/maven-shade-plugin/examples/resource-transformers.html)
    pub services: bool,
    /// `ServicesResourceTransformer` of shade 3.4.0 and older keeps every comment and blank line; 3.4.1 and newer
    /// treat them as ordinary lines and drop repeats. Set from the plugin's `<version>`.
    pub services_keep_comments: bool,
    /// `ManifestResourceTransformer` `<mainClass>`: becomes `Main-Class` in the manifest.
    pub main_class: Option<String>,
    /// `ManifestResourceTransformer` `<manifestEntries>`: extra manifest attributes.
    pub manifest: Vec<(String, String)>,
    /// `<filters>`: (artifact pattern like `*:*` or `group:artifact`, ant-style exclude globs).
    /// [Selecting contents](https://maven.apache.org/plugins/maven-shade-plugin/examples/includes-excludes.html)
    pub excludes: Vec<(String, Vec<String>)>,
    /// `<finalName>`, uninterpolated: the shaded jar's file name without `.jar`.
    pub final_name: Option<String>,
    /// `<shadedArtifactAttached>`: keep the plain jar and write the shaded one beside it.
    pub attached: bool,
    /// `<shadedClassifierName>`; Maven's default is `shaded`.
    pub classifier: Option<String>,
    /// Config rayvn does not implement; reported so a fat jar is never silently different.
    pub ignored: Vec<String>,
}

/// True if `version` is a dotted number lower than `than`; anything unparsable is not older.
pub(crate) fn version_older_than(version: &str, than: &str) -> bool {
    let parts = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u32>().ok())
            .collect::<Option<Vec<_>>>()
    };
    matches!((parts(version), parts(than)), (Some(a), Some(b)) if a < b)
}

fn plugin_version(root: Node, artifact: &str) -> Option<String> {
    let plugin = child(root, "build")
        .and_then(|b| child(b, "plugins"))?
        .children()
        .find(|p| p.is_element() && text(*p, "artifactId").as_deref() == Some(artifact))?;
    text(plugin, "version")
}

fn parse_shade(root: Node) -> Option<ShadeCfg> {
    let conf = plugin_config(root, "maven-shade-plugin")?;
    let mut cfg = ShadeCfg {
        services_keep_comments: plugin_version(root, "maven-shade-plugin")
            .is_some_and(|v| version_older_than(&v, "3.4.1")),
        ..Default::default()
    };
    let Some(conf) = conf else { return Some(cfg) };
    for t in child(conf, "transformers")
        .into_iter()
        .flat_map(|t| t.children().filter(|c| c.is_element()))
    {
        match t
            .attribute("implementation")
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
        {
            "ServicesResourceTransformer" => cfg.services = true,
            "ManifestResourceTransformer" => {
                cfg.main_class = text(t, "mainClass");
                if let Some(m) = child(t, "manifestEntries") {
                    cfg.manifest.extend(
                        m.children()
                            .filter(|c| c.is_element())
                            .map(|c| (c.tag_name().name().to_string(), node_text(c))),
                    );
                }
            }
            other => cfg.ignored.push(format!("transformer {other}")),
        }
    }
    for f in child(conf, "filters")
        .into_iter()
        .flat_map(|f| f.children().filter(|c| c.is_element()))
    {
        let art = text(f, "artifact").unwrap_or_else(|| "*:*".into());
        let ex: Vec<String> = child(f, "excludes")
            .map(|e| {
                e.children()
                    .filter(|c| c.is_element())
                    .map(node_text)
                    .collect()
            })
            .unwrap_or_default();
        if child(f, "includes").is_some() {
            cfg.ignored.push("filter <includes>".into());
        }
        cfg.excludes.push((art, ex));
    }
    cfg.final_name = text(conf, "finalName");
    cfg.attached = text(conf, "shadedArtifactAttached").is_some_and(|v| v == "true");
    cfg.classifier = text(conf, "shadedClassifierName");
    for k in ["relocations", "artifactSet", "minimizeJar"] {
        if child(conf, k).is_some() {
            cfg.ignored.push(k.to_string());
        }
    }
    Some(cfg)
}

/// `<profile>` activation conditions that rayvn can evaluate without Maven's CLI context.
#[derive(Clone, Debug, Default)]
struct Activation {
    by_default: bool,
    jdk: Option<String>,
    property: Option<(String, Option<String>)>,
    os: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
struct Profile {
    activation: Option<Activation>,
    props: BTreeMap<String, String>,
    managed: Vec<Dep>,
    deps: Vec<Dep>,
    repos: Vec<RepoDecl>,
}

#[derive(Clone, Debug)]
pub struct ParentRef {
    pub gav: Gav,
    pub relative_path: Option<String>,
}

pub fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

/// All text directly inside `n`, ignoring comments (`<version><!-- x -->1.0</version>`).
fn node_text(n: Node) -> String {
    n.children()
        .filter(|c| c.is_text())
        .filter_map(|c| c.text())
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn text(n: Node, name: &str) -> Option<String> {
    child(n, name).map(node_text).filter(|s| !s.is_empty())
}

fn parse_dep(n: Node) -> Option<Dep> {
    let exclusions = child(n, "exclusions")
        .map(|e| {
            e.children()
                .filter(|c| c.is_element())
                .filter_map(|x| Some((text(x, "groupId")?, text(x, "artifactId")?)))
                .collect()
        })
        .unwrap_or_default();
    Some(Dep {
        group: text(n, "groupId")?,
        artifact: text(n, "artifactId")?,
        version: text(n, "version"),
        scope: text(n, "scope"),
        optional: text(n, "optional").as_deref() == Some("true"),
        classifier: text(n, "classifier"),
        typ: text(n, "type").unwrap_or_else(|| "jar".into()),
        exclusions,
    })
}

fn parse_deps(n: Option<Node>) -> Vec<Dep> {
    n.map(|d| {
        d.children()
            .filter(|c| c.is_element())
            .filter_map(parse_dep)
            .collect()
    })
    .unwrap_or_default()
}

fn parse_props(n: Option<Node>) -> BTreeMap<String, String> {
    n.map(|p| {
        p.children()
            .filter(|c| c.is_element())
            .map(|c| (c.tag_name().name().to_string(), node_text(c)))
            .collect()
    })
    .unwrap_or_default()
}

fn parse_repos(n: Option<Node>) -> Vec<RepoDecl> {
    n.map(|r| {
        r.children()
            .filter(|c| c.is_element())
            .filter_map(|x| {
                Some(RepoDecl {
                    id: text(x, "id")?,
                    url: text(x, "url")?,
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn parse_profile(n: Node) -> Profile {
    let activation = child(n, "activation").map(|a| Activation {
        by_default: text(a, "activeByDefault").as_deref() == Some("true"),
        jdk: text(a, "jdk"),
        property: child(a, "property").and_then(|p| Some((text(p, "name")?, text(p, "value")))),
        os: child(a, "os")
            .map(|o| {
                ["name", "family", "arch", "version"]
                    .iter()
                    .filter_map(|k| Some((k.to_string(), text(o, k)?)))
                    .collect()
            })
            .unwrap_or_default(),
    });
    Profile {
        activation,
        props: parse_props(child(n, "properties")),
        managed: parse_deps(
            child(n, "dependencyManagement").and_then(|d| child(d, "dependencies")),
        ),
        deps: parse_deps(child(n, "dependencies")),
        repos: parse_repos(child(n, "repositories")),
    }
}

/// Activate profiles the way Maven does: any profile whose explicit condition holds; if none do,
/// the `activeByDefault` ones. Conditions rayvn cannot evaluate (`file`, `-P`, `-D`) count as inactive.
fn active(profiles: &[Profile]) -> Vec<&Profile> {
    let explicit: Vec<&Profile> = profiles
        .iter()
        .filter(|p| p.activation.as_ref().is_some_and(explicit_match))
        .collect();
    if !explicit.is_empty() {
        return explicit;
    }
    profiles
        .iter()
        .filter(|p| p.activation.as_ref().is_some_and(|a| a.by_default))
        .collect()
}

fn explicit_match(a: &Activation) -> bool {
    let conditions =
        a.jdk.is_some() as usize + a.property.is_some() as usize + !a.os.is_empty() as usize;
    if conditions == 0 {
        return false;
    }
    a.jdk.as_deref().is_none_or(jdk_matches)
        && a.property.as_ref().is_none_or(property_matches)
        && (a.os.is_empty() || os_matches(&a.os))
}

/// rayvn has no `-D` yet, so no property is ever set: `!name` holds, and so does `name` with a negated value
/// (`!true`, "anything but true", which an unset property is). A plain name or value does not.
fn property_matches((name, value): &(String, Option<String>)) -> bool {
    name.starts_with('!') || value.as_deref().is_some_and(|v| v.starts_with('!'))
}

fn os_matches(os: &[(String, String)]) -> bool {
    let (name, family, arch) = match std::env::consts::OS {
        "macos" => ("mac os x", "mac", "aarch64"),
        "linux" => ("linux", "unix", "amd64"),
        _ => ("windows", "windows", "amd64"),
    };
    let arch = if std::env::consts::ARCH == "x86_64" {
        "amd64"
    } else if std::env::consts::ARCH == "aarch64" {
        "aarch64"
    } else {
        arch
    };
    os.iter().all(|(k, v)| {
        let (neg, v) = v
            .strip_prefix('!')
            .map(|x| (true, x))
            .unwrap_or((false, v.as_str()));
        let hit = match k.as_str() {
            "name" => v.eq_ignore_ascii_case(name),
            "family" => {
                v.eq_ignore_ascii_case(family)
                    || (v.eq_ignore_ascii_case("unix") && family == "mac")
            }
            "arch" => {
                v.eq_ignore_ascii_case(arch) || v.eq_ignore_ascii_case(std::env::consts::ARCH)
            }
            _ => true,
        };
        hit != neg
    })
}

/// Major version of the JDK on PATH / JAVA_HOME, found once per process.
fn java_major() -> Option<u32> {
    static V: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let from_release = std::env::var("JAVA_HOME")
            .ok()
            .and_then(|h| std::fs::read_to_string(format!("{h}/release")).ok())
            .and_then(|r| {
                r.lines().find_map(|l| {
                    l.strip_prefix("JAVA_VERSION=")
                        .map(|v| v.trim_matches('"').to_string())
                })
            });
        let ver = from_release.or_else(|| {
            let out = std::process::Command::new("java")
                .arg("-version")
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&out.stderr).to_string();
            text.split('"').nth(1).map(str::to_string)
        })?;
        let mut parts = ver.split(['.', '-', '_', '+']);
        let first: u32 = parts.next()?.parse().ok()?;
        Some(if first == 1 {
            parts.next()?.parse().ok()?
        } else {
            first
        })
    })
}

/// `<jdk>`: a version prefix (`1.8`, `17`), a negated prefix (`!1.8`), or a range (`[1.8,)`).
fn jdk_matches(spec: &str) -> bool {
    let Some(cur) = java_major() else {
        return false;
    };
    let norm = |v: &str| -> Option<u32> {
        let mut p = v.trim().split('.');
        let a: u32 = p.next()?.parse().ok()?;
        Some(if a == 1 { p.next()?.parse().ok()? } else { a })
    };
    if let Some(neg) = spec.strip_prefix('!') {
        return norm(neg) != Some(cur);
    }
    if spec.starts_with('[') || spec.starts_with('(') {
        let (lo_incl, hi_incl) = (spec.starts_with('['), spec.ends_with(']'));
        let inner = &spec[1..spec.len() - 1];
        let (lo, hi) = inner.split_once(',').unwrap_or((inner, inner));
        let lo_ok = lo.trim().is_empty()
            || norm(lo).is_some_and(|l| if lo_incl { cur >= l } else { cur > l });
        let hi_ok = hi.trim().is_empty()
            || norm(hi).is_some_and(|h| if hi_incl { cur <= h } else { cur < h });
        return lo_ok && hi_ok;
    }
    norm(spec) == Some(cur)
}

/// Parses one POM, applying the profiles that are active without Maven's command-line context.
pub fn parse(xml: &str) -> Result<RawPom> {
    let mut pom = parse_raw(xml)?;
    let doc_profiles = std::mem::take(&mut pom.profiles);
    for p in active(&doc_profiles) {
        pom.props.extend(p.props.clone());
        for d in &p.managed {
            pom.managed.retain(|x| x.key() != d.key());
            pom.managed.push(d.clone());
        }
        for d in &p.deps {
            pom.deps.retain(|x| x.key() != d.key());
            pom.deps.push(d.clone());
        }
        pom.repos.extend(p.repos.clone());
    }
    Ok(pom)
}

fn parse_raw(xml: &str) -> Result<RawPom> {
    let opts = ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = Document::parse_with_options(xml, opts).context("invalid pom xml")?;
    let root = doc.root_element();

    let parent = child(root, "parent").and_then(|p| {
        Some(ParentRef {
            gav: Gav::new(
                &text(p, "groupId")?,
                &text(p, "artifactId")?,
                &text(p, "version")?,
            ),
            relative_path: child(p, "relativePath").map(|r| node_text(r)),
        })
    });

    let props = parse_props(child(root, "properties"));

    let repos = parse_repos(child(root, "repositories"));

    let modules = child(root, "modules")
        .map(|m| {
            m.children()
                .filter(|c| c.is_element())
                .map(node_text)
                .collect()
        })
        .unwrap_or_default();

    Ok(RawPom {
        parent,
        group: text(root, "groupId"),
        artifact: text(root, "artifactId").context("pom has no artifactId")?,
        version: text(root, "version"),
        packaging: text(root, "packaging").unwrap_or_else(|| "jar".into()),
        props,
        managed: parse_deps(
            child(root, "dependencyManagement").and_then(|d| child(d, "dependencies")),
        ),
        deps: parse_deps(child(root, "dependencies")),
        repos,
        modules,
        relocation: child(root, "distributionManagement")
            .and_then(|d| child(d, "relocation"))
            .map(|r| Relocation {
                group: text(r, "groupId"),
                artifact: text(r, "artifactId"),
                version: text(r, "version"),
            }),
        shade: parse_shade(root),
        compiler: parse_compiler(root),
        surefire: parse_tests(root, "maven-surefire-plugin"),
        failsafe: parse_tests(root, "maven-failsafe-plugin"),
        unrun: parse_unrun(root),
        profiles: child(root, "profiles")
            .map(|p| {
                p.children()
                    .filter(|c| c.is_element())
                    .map(parse_profile)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gav_displays_as_colon_separated_coordinates() {
        assert_eq!(
            Gav::new("org.example", "lib", "1.0").to_string(),
            "org.example:lib:1.0"
        );
    }

    #[test]
    fn parse_reads_coordinates_and_dependencies() {
        let pom = parse(
            r#"<project>
                 <groupId>org.example</groupId><artifactId>app</artifactId><version>1.0</version>
                 <dependencies>
                   <dependency><groupId>org.example</groupId><artifactId>lib</artifactId><version>2.0</version></dependency>
                 </dependencies>
               </project>"#,
        )
        .unwrap();
        assert_eq!(pom.artifact, "app");
        assert_eq!(pom.deps.len(), 1);
        assert_eq!(pom.deps[0].version.as_deref(), Some("2.0"));
    }

    #[test]
    fn artifact_types_map_to_extension_and_classifier() {
        for typ in ["bundle", "maven-plugin", "test-jar", "jar"] {
            assert_eq!(extension_of(typ), "jar");
        }
        assert_eq!(extension_of("pom"), "pom");
        assert_eq!(extension_of("zip"), "zip");
        assert_eq!(stored_classifier("test-jar", None), Some("tests"));
        assert_eq!(stored_classifier("test-jar", Some("x")), Some("x"));
        assert_eq!(stored_classifier("jar", None), None);
    }

    fn javafx_style(os_name: &str, platform: &str) -> String {
        format!(
            "<project><artifactId>fx</artifactId><profiles><profile><activation><os><name>{os_name}</name></os>\
             <property><name>javafx.monocle</name><value>!true</value></property></activation>\
             <properties><javafx.platform>{platform}</javafx.platform></properties></profile></profiles></project>"
        )
    }

    fn unrun_of(plugins: &str) -> Vec<String> {
        parse(&format!("<project><artifactId>p</artifactId><build><plugins>{plugins}</plugins></build></project>")).unwrap().unrun
    }

    fn plugin(artifact: &str, executions: &str) -> String {
        format!(
            "<plugin><artifactId>{artifact}</artifactId><executions>{executions}</executions></plugin>"
        )
    }

    #[test]
    fn a_phase_bound_execution_of_a_plugin_rayvn_does_not_run_is_reported() {
        let exec = plugin(
            "exec-maven-plugin",
            "<execution><phase>generate-sources</phase><goals><goal>exec</goal><goal>java</goal></goals></execution>",
        );
        assert_eq!(
            unrun_of(&exec),
            ["exec-maven-plugin (exec, java at generate-sources)"]
        );
    }

    #[test]
    fn an_execution_without_a_phase_or_goals_is_still_reported() {
        assert_eq!(
            unrun_of(&plugin(
                "x-plugin",
                "<execution><goals><goal>g</goal></goals></execution>"
            )),
            ["x-plugin (g)"]
        );
        assert_eq!(
            unrun_of(&plugin(
                "x-plugin",
                "<execution><phase>package</phase></execution>"
            )),
            ["x-plugin (default goals at package)"]
        );
    }

    #[test]
    fn a_plugin_that_is_only_configured_is_not_part_of_the_build() {
        let cfg = "<plugin><artifactId>exec-maven-plugin</artifactId><configuration><mainClass>m.Run</mainClass></configuration></plugin>";
        assert!(unrun_of(cfg).is_empty());
    }

    #[test]
    fn an_execution_bound_to_phase_none_does_not_run() {
        assert!(
            unrun_of(&plugin(
                "x-plugin",
                "<execution><phase>none</phase><goals><goal>g</goal></goals></execution>"
            ))
            .is_empty()
        );
    }

    #[test]
    fn plugins_rayvn_handles_are_not_reported() {
        for p in [
            "maven-shade-plugin",
            "maven-failsafe-plugin",
            "maven-compiler-plugin",
            "maven-install-plugin",
        ] {
            assert!(
                unrun_of(&plugin(
                    p,
                    "<execution><phase>package</phase><goals><goal>g</goal></goals></execution>"
                ))
                .is_empty(),
                "{p}"
            );
        }
    }

    #[test]
    fn an_os_profile_with_a_not_true_property_applies_because_the_property_is_unset() {
        let here = if std::env::consts::OS == "macos" {
            "mac os x"
        } else if std::env::consts::OS == "linux" {
            "linux"
        } else {
            "windows"
        };
        assert_eq!(
            parse(&javafx_style(here, "here"))
                .unwrap()
                .props
                .get("javafx.platform")
                .map(String::as_str),
            Some("here")
        );
        assert!(
            parse(&javafx_style("plan9", "there"))
                .unwrap()
                .props
                .is_empty()
        );
    }

    #[test]
    fn shade_3_4_0_and_older_keep_service_comments() {
        let pom = |v: &str| {
            format!(
                "<project><artifactId>a</artifactId><build><plugins><plugin><artifactId>maven-shade-plugin</artifactId><version>{v}</version></plugin></plugins></build></project>"
            )
        };
        assert!(
            parse(&pom("3.4.0"))
                .unwrap()
                .shade
                .unwrap()
                .services_keep_comments
        );
        assert!(
            parse(&pom("3.2.4"))
                .unwrap()
                .shade
                .unwrap()
                .services_keep_comments
        );
        assert!(
            !parse(&pom("3.4.1"))
                .unwrap()
                .shade
                .unwrap()
                .services_keep_comments
        );
        assert!(
            !parse(&pom("3.10.0"))
                .unwrap()
                .shade
                .unwrap()
                .services_keep_comments
        );
        assert!(
            !parse(&pom("${shade.version}"))
                .unwrap()
                .shade
                .unwrap()
                .services_keep_comments
        );
    }

    #[test]
    fn a_property_that_must_be_set_never_holds_without_dash_d() {
        for (name, value) in [
            ("x", None),
            ("x", Some("true")),
            ("!x", None),
            ("x", Some("!true")),
        ] {
            let held = property_matches(&(name.to_string(), value.map(String::from)));
            assert_eq!(
                held,
                name.starts_with('!') || value.is_some_and(|v| v.starts_with('!')),
                "{name} {value:?}"
            );
        }
    }

    #[test]
    fn a_relocation_is_read_from_distribution_management() {
        let moved = parse("<project><groupId>g</groupId><artifactId>a</artifactId><version>1</version><distributionManagement><relocation><groupId>new</groupId></relocation></distributionManagement></project>").unwrap();
        assert_eq!(
            moved.relocation,
            Some(Relocation {
                group: Some("new".into()),
                ..Default::default()
            })
        );
        let plain = parse(
            "<project><groupId>g</groupId><artifactId>a</artifactId><version>1</version></project>",
        )
        .unwrap();
        assert_eq!(plain.relocation, None);
    }
}
