//! `rvn build`: compile the main sources with `javac` against the resolved classpath, then package a jar,
//! shaded into a single fat jar when the POM configures `maven-shade-plugin`.

use crate::model::Effective;
use crate::pom::ShadeCfg;
use std::collections::{BTreeMap, HashSet};
use crate::resolve::Resolved;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;

/// All files below `dir`, sorted; a missing directory has none.
pub(crate) fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    walk_filtered(dir, &|_| false, out)
}

/// Like [`walk`], but entries (files or directories) for which `skip` is true are left out.
pub(crate) fn walk_filtered(dir: &Path, skip: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if skip(&p) {
            continue;
        }
        if p.is_dir() {
            walk_filtered(&p, skip, out)?;
        } else {
            out.push(p);
        }
    }
    out.sort();
    Ok(())
}

/// Compiles `src/main/java` and copies `src/main/resources` into `target/classes`; returns that directory.
pub fn compile_main(root: &Effective, dir: &Path, resolved: &[Resolved]) -> Result<PathBuf> {
    let classes = dir.join("target/classes");
    // javac recompiles everything, so a clean directory costs nothing and keeps deleted sources and resources out of the jar.
    clean_dir(&classes)?;
    compile(root, &dir.join("src/main/java"), &classes, &compile_classpath(resolved))?;
    copy_resources(&dir.join("src/main/resources"), &classes)?;
    Ok(classes)
}

/// Empties `dir` (creating it if needed).
pub(crate) fn clean_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("cleaning {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

pub fn build(root: &Effective, dir: &Path, resolved: &[Resolved]) -> Result<()> {
    let classes = compile_main(root, dir, resolved)?;
    package(root, dir, &classes, resolved)
}

/// Writes the jar from `classes`, shaded as the POM configures, and prints its path.
pub fn package(root: &Effective, dir: &Path, classes: &Path, resolved: &[Resolved]) -> Result<()> {
    let target = dir.join("target");
    let jar_path = target.join(format!("{}-{}.jar", root.gav.artifact, root.gav.version));
    write_jar(classes, &jar_path)?;
    eprintln!("  packaged {} ({})", jar_path.display(), size(&jar_path));
    let mut result = jar_path.clone();
    if let Some(cfg) = &root.shade {
        result = shaded_path(cfg, &target, &root.gav.artifact, &root.gav.version, &root.props);
        if result == jar_path {
            shade_in_place(cfg, &target, &jar_path, resolved, root)?;
        } else {
            shade_into(cfg, &jar_path, &result, resolved, &target)?;
        }
    }
    println!("{}", result.display());
    Ok(())
}

/// Where `maven-shade-plugin` puts the shaded jar: `<finalName>.jar` when configured, else
/// `<artifact>-<version>-<classifier>.jar` when it is attached (classifier defaults to `shaded`), else it replaces
/// the plain jar. Checked against Maven for the first case only: shade 3.4.1 with `finalName` and an attached
/// artifact writes `<finalName>.jar` and leaves the plain jar alone.
fn shaded_path(cfg: &ShadeCfg, target: &Path, artifact: &str, version: &str, props: &BTreeMap<String, String>) -> PathBuf {
    let name = match (&cfg.final_name, cfg.attached) {
        (Some(n), _) => crate::model::interpolate(n, props),
        (None, true) => format!("{artifact}-{version}-{}", cfg.classifier.as_deref().unwrap_or("shaded")),
        (None, false) => format!("{artifact}-{version}"),
    };
    target.join(format!("{name}.jar"))
}

/// Compile-time classpath: everything except test-scoped jars, in resolution order.
fn compile_classpath(resolved: &[Resolved]) -> Vec<String> {
    resolved
        .iter()
        .filter(|r| r.scope != "test")
        .filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string()))
        .collect()
}

pub(crate) fn java_sources(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut sources = vec![];
    walk(dir, &mut sources)?;
    sources.retain(|p| p.extension().is_some_and(|e| e == "java"));
    Ok(sources)
}

/// The Java level, as Maven's compiler plugin decides it: plugin `<configuration>` beats the
/// `maven.compiler.*` properties, and `release` beats `source`/`target`. An unresolved `${...}` is unset.
#[derive(Debug, Default, PartialEq)]
struct JavaLevel {
    release: Option<String>,
    source: Option<String>,
    target: Option<String>,
}

impl JavaLevel {
    fn of(root: &Effective) -> Self {
        let cfg = root.compiler.clone().unwrap_or_default();
        let pick = |plugin: Option<String>, prop: &str| {
            plugin
                .or_else(|| root.props.get(prop).cloned())
                .map(|v| crate::model::interpolate(&v, &root.props))
                .filter(|v| !v.contains("${"))
        };
        JavaLevel {
            release: pick(cfg.release, "maven.compiler.release"),
            source: pick(cfg.source, "maven.compiler.source"),
            target: pick(cfg.target, "maven.compiler.target"),
        }
    }

    fn args(&self) -> Vec<String> {
        match (&self.release, &self.source, &self.target) {
            (Some(r), _, _) => vec!["--release".into(), r.clone()],
            (None, s, t) => s.iter().flat_map(|s| ["-source".to_string(), s.clone()]).chain(t.iter().flat_map(|t| ["-target".to_string(), t.clone()])).collect(),
        }
    }
}

fn javac_command(classes: &Path, level: &JavaLevel, classpath: &[String], sources: &[PathBuf]) -> Command {
    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(classes).args(["-encoding", "UTF-8", "-proc:none", "-g"]).args(level.args());
    if !classpath.is_empty() {
        cmd.arg("-cp").arg(classpath.join(":"));
    }
    cmd.args(sources);
    cmd
}

pub(crate) fn compile(root: &Effective, src_dir: &Path, classes: &Path, classpath: &[String]) -> Result<()> {
    let sources = java_sources(src_dir)?;
    if sources.is_empty() {
        return Ok(());
    }
    let level = JavaLevel::of(root);
    let flags = level.args();
    eprintln!(
        "  compiling {} sources with javac{}{}, {} classpath entries",
        sources.len(),
        if flags.is_empty() { "" } else { " " },
        flags.join(" "),
        classpath.len()
    );
    let status = javac_command(classes, &level, classpath, &sources).status().context("running javac")?;
    if !status.success() {
        bail!("javac failed");
    }
    Ok(())
}

/// Files and directories Maven never copies as resources (plexus "default excludes": editor backups and
/// version-control metadata). [Plexus `AbstractScanner`](https://codehaus-plexus.github.io/plexus-utils/apidocs/org/codehaus/plexus/util/AbstractScanner.html)
fn default_excluded(name: &str) -> bool {
    const DIRS: [&str; 7] = ["CVS", "RCS", "SCCS", ".svn", ".git", ".hg", ".bzr"];
    const FILES: [&str; 10] = [".cvsignore", "vssver.scc", ".DS_Store", ".gitattributes", ".gitignore", ".gitmodules", ".hgignore", ".hgsub", ".hgtags", ".bzrignore"];
    DIRS.contains(&name)
        || FILES.contains(&name)
        || name.ends_with('~')
        || (name.starts_with('#') && name.ends_with('#'))
        || name.starts_with(".#")
        || (name.starts_with('%') && name.ends_with('%'))
        || name.starts_with("._")
}

pub(crate) fn copy_resources(res_dir: &Path, classes: &Path) -> Result<()> {
    let mut resources = vec![];
    walk_filtered(res_dir, &|p| p.file_name().is_some_and(|n| default_excluded(&n.to_string_lossy())), &mut resources)?;
    if !resources.is_empty() {
        eprintln!("  copying {} resources", resources.len());
    }
    for r in &resources {
        let dest = classes.join(r.strip_prefix(res_dir)?);
        std::fs::create_dir_all(dest.parent().context("resource has no parent directory")?)?;
        std::fs::copy(r, dest)?;
    }
    Ok(())
}

fn write_jar(classes: &Path, jar_path: &Path) -> Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(jar_path)?);
    let opts = deflated();
    zip.start_file(MANIFEST, opts)?;
    zip.write_all(b"Manifest-Version: 1.0\r\nCreated-By: rayvn\r\n\r\n")?;
    let mut files = vec![];
    walk(classes, &mut files)?;
    for f in files {
        zip.start_file(entry_name(classes, &f)?, opts)?;
        zip.write_all(&std::fs::read(&f)?)?;
    }
    zip.finish()?;
    Ok(())
}

/// Zip entry name of `file` below `root`, always with `/` separators.
fn entry_name(root: &Path, file: &Path) -> Result<String> {
    Ok(file.strip_prefix(root)?.to_string_lossy().replace('\\', "/"))
}

/// Entry options for reproducible jars: a fixed timestamp (the zip epoch, 1980-01-01) and fixed permissions, so the
/// same inputs give the same bytes whenever and wherever they are built.
fn deflated() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644)
}

/// Replaces `jar_path` with a shaded jar; the plain jar is kept as `original-<name>`.
fn shade_in_place(cfg: &ShadeCfg, target: &Path, jar_path: &Path, resolved: &[Resolved], root: &Effective) -> Result<()> {
    let original = target.join(format!("original-{}-{}.jar", root.gav.artifact, root.gav.version));
    std::fs::rename(jar_path, &original)?;
    shade_into(cfg, &original, jar_path, resolved, target)
}

fn shade_into(cfg: &ShadeCfg, plain: &Path, out: &Path, resolved: &[Resolved], target: &Path) -> Result<()> {
    for i in &cfg.ignored {
        eprintln!("  warning: shade option not supported by rayvn, ignored: {i}");
    }
    let n = shade(cfg, plain, &shaded_dependencies(resolved, target)?, out)?;
    eprintln!("  shaded {} dependency jars into {} ({})", n, out.display(), size(out));
    Ok(())
}

/// `group:artifact` and jar of every compile/runtime dependency, in resolution order. A module of the same build
/// is a class directory; it is packed into `target/rayvn-modules/` first so that it merges like any other jar.
fn shaded_dependencies(resolved: &[Resolved], target: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    for r in resolved.iter().filter(|r| matches!(r.scope.as_str(), "compile" | "runtime")) {
        let Some(path) = r.jar.clone() else { continue };
        let jar = if path.is_dir() {
            let packed = target.join("rayvn-modules").join(format!("{}-{}.jar", r.gav.artifact, r.gav.version));
            std::fs::create_dir_all(packed.parent().context("no parent")?)?;
            write_jar(&path, &packed)?;
            packed
        } else {
            path
        };
        out.push((format!("{}:{}", r.gav.group, r.gav.artifact), jar));
    }
    Ok(out)
}

fn size(p: &Path) -> String {
    let b = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if b >= 1 << 20 { format!("{:.1} MB", b as f64 / 1048576.0) } else { format!("{} KB", b / 1024) }
}

/// Ant-style match: `*` stays inside a path segment, `**` crosses `/`.
pub(crate) fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
                (0..=s.len()).filter(|&i| i == 0 || s[i - 1] == b'/' || rest.is_empty()).any(|i| go(rest, &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pat.as_bytes(), s.as_bytes())
}

/// Shade artifact patterns (`*:*`, `org.foo:*`): `*` matches any run of characters, `:` included.
fn artifact_matches(pat: &str, ga: &str) -> bool {
    match pat.split_once('*') {
        None => pat == ga,
        Some((head, tail)) => ga.strip_prefix(head).is_some_and(|rest| (0..=rest.len()).filter(|&i| rest.is_char_boundary(i)).any(|i| artifact_matches(tail, &rest[i..]))),
    }
}

fn excluded(cfg: &ShadeCfg, ga: &str, name: &str) -> bool {
    cfg.excludes.iter().any(|(art, ex)| artifact_matches(art, ga) && ex.iter().any(|g| glob(g, name) || glob(&format!("{g}/**"), name)))
}

const MANIFEST: &str = "META-INF/MANIFEST.MF";
const SERVICES_DIR: &str = "META-INF/services/";

fn manifest(cfg: &ShadeCfg) -> String {
    let mut mf = String::from("Manifest-Version: 1.0\r\nCreated-By: rayvn\r\n");
    if let Some(m) = &cfg.main_class {
        mf.push_str(&format!("Main-Class: {m}\r\n"));
    }
    for (k, v) in &cfg.manifest {
        mf.push_str(&format!("{k}: {v}\r\n"));
    }
    mf.push_str("\r\n");
    mf
}

fn is_service_file(name: &str) -> bool {
    name.starts_with(SERVICES_DIR) && name.len() > SERVICES_DIR.len()
}

/// Entries that never go into a shaded jar: directories, `INDEX.LIST`, `module-info.class`, anything
/// already taken from an earlier jar, and the dependency's `<excludes>`. `ga` is empty for the project's own classes.
fn skip_entry(cfg: &ShadeCfg, ga: &str, name: &str, seen: &HashSet<String>) -> bool {
    name.ends_with('/') || name == "META-INF/INDEX.LIST" || name == "module-info.class" || seen.contains(name) || (!ga.is_empty() && excluded(cfg, ga, name))
}

/// Appends `content` to `body`: every line not already there, in first-seen order, as Maven's
/// `ServicesResourceTransformer` does. With `keep_comments` (shade 3.4.0 and older) comment and blank lines are
/// always kept; otherwise they are lines like any other and a repeated `#` or blank collapses to one.
fn merge_service_lines(body: &mut Vec<u8>, content: &str, keep_comments: bool) {
    let mut known: HashSet<String> = String::from_utf8_lossy(body).lines().map(str::to_string).collect();
    for line in content.lines() {
        let always = keep_comments && (line.starts_with('#') || line.trim().is_empty());
        if known.insert(line.to_string()) || always {
            body.extend_from_slice(line.as_bytes());
            body.push(b'\n');
        }
    }
}

/// Merge the project's classes and its compile/runtime dependency jars into one jar; first entry wins,
/// like maven-shade-plugin. Entries are copied compressed, without recompression.
fn shade(cfg: &ShadeCfg, own: &Path, deps: &[(String, PathBuf)], out: &Path) -> Result<usize> {
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(File::create(out)?));
    let opts = deflated();
    zip.start_file(MANIFEST, opts)?;
    zip.write_all(manifest(cfg).as_bytes())?;
    let mut seen: HashSet<String> = HashSet::from([MANIFEST.to_string()]);
    let mut services: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    let sources = std::iter::once((String::new(), own.to_path_buf())).chain(deps.iter().cloned());
    for (ga, path) in sources {
        let file = File::open(&path).with_context(|| path.display().to_string())?;
        copy_entries(cfg, &ga, &mut zip::ZipArchive::new(std::io::BufReader::new(file))?, &mut zip, &mut seen, &mut services)?;
    }
    for (name, body) in services {
        zip.start_file(name, opts)?;
        zip.write_all(&body)?;
    }
    zip.finish()?;
    Ok(deps.len())
}

/// Copies the entries of one jar that are not skipped; service files are collected in `services` to be merged.
fn copy_entries<R: std::io::Read + std::io::Seek, W: Write + std::io::Seek>(
    cfg: &ShadeCfg,
    ga: &str,
    archive: &mut zip::ZipArchive<R>,
    zip: &mut zip::ZipWriter<W>,
    seen: &mut HashSet<String>,
    services: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for i in 0..archive.len() {
        let name = archive.by_index_raw(i)?.name()?.to_string();
        if skip_entry(cfg, ga, &name, seen) {
            continue;
        }
        if cfg.services && is_service_file(&name) {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut archive.by_index(i)?, &mut buf)?;
            merge_service_lines(services.entry(name).or_default(), &String::from_utf8_lossy(&buf), cfg.services_keep_comments);
            continue;
        }
        seen.insert(name);
        zip.raw_copy_file_touch(archive.by_index_raw(i)?, zip::DateTime::default(), Some(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ShadeCfg {
        ShadeCfg { main_class: Some("a.Main".into()), manifest: vec![("X-Y".into(), "z".into())], ..Default::default() }
    }

    #[test]
    fn glob_star_stays_in_segment_and_double_star_crosses() {
        assert!(glob("META-INF/*.SF", "META-INF/A.SF"));
        assert!(!glob("META-INF/*.SF", "META-INF/sub/A.SF"));
        assert!(glob("META-INF/**", "META-INF/sub/A.SF"));
        assert!(glob("**/*Test.java", "a/b/FooTest.java"));
        assert!(glob("**/*Test.java", "FooTest.java"));
        assert!(!glob("**/Test*.java", "a/FooTest.java"));
    }

    #[test]
    fn manifest_lists_main_class_then_extra_attributes() {
        assert_eq!(manifest(&cfg()), "Manifest-Version: 1.0\r\nCreated-By: rayvn\r\nMain-Class: a.Main\r\nX-Y: z\r\n\r\n");
    }

    #[test]
    fn service_files_are_recognised_only_below_the_directory() {
        assert!(is_service_file("META-INF/services/a.B"));
        assert!(!is_service_file("META-INF/services/"));
        assert!(!is_service_file("META-INF/MANIFEST.MF"));
    }

    #[test]
    fn skip_entry_drops_directories_descriptors_and_duplicates() {
        let seen = HashSet::from(["a/B.class".to_string()]);
        for name in ["a/", "META-INF/INDEX.LIST", "module-info.class", "a/B.class"] {
            assert!(skip_entry(&cfg(), "g:a", name, &seen), "{name}");
        }
        assert!(!skip_entry(&cfg(), "g:a", "a/C.class", &seen));
    }

    #[test]
    fn artifact_patterns_use_star_for_any_run_including_the_colon() {
        for (pat, ga, ok) in [
            ("*:*", "org.eclipse.emf:common", true),
            ("org.eclipse.*:*", "org.eclipse.emf:common", true),
            ("org.eclipse.emf:*", "org.eclipse.emf:common", true),
            ("org.eclipse.emf:*", "org.eclipse.xsd:common", false),
            ("g:a", "g:a", true),
            ("g:a", "g:ab", false),
            ("*:common", "g:common", true),
        ] {
            assert_eq!(artifact_matches(pat, ga), ok, "{pat} vs {ga}");
        }
    }

    #[test]
    fn a_wildcard_filter_drops_signature_files_from_every_dependency() {
        let cfg = ShadeCfg { excludes: vec![("*:*".into(), vec!["META-INF/*.SF".into(), "META-INF/*.RSA".into()])], ..Default::default() };
        let none = HashSet::new();
        assert!(skip_entry(&cfg, "org.eclipse.emf:common", "META-INF/ECLIPSE_.SF", &none));
        assert!(skip_entry(&cfg, "org.eclipse.emf:common", "META-INF/ECLIPSE_.RSA", &none));
        assert!(!skip_entry(&cfg, "org.eclipse.emf:common", "META-INF/services/x", &none));
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rayvn-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn clean_dir_removes_stale_files_and_creates_missing_directories() {
        let d = scratch("clean");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/old.class"), b"x").unwrap();
        clean_dir(&d).unwrap();
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        clean_dir(&d.join("new/deep")).unwrap();
        assert!(d.join("new/deep").is_dir());
    }

    #[test]
    fn jars_are_reproducible_fixed_time_fixed_permissions_sorted_entries() {
        let d = scratch("jar");
        let classes = d.join("classes");
        std::fs::create_dir_all(classes.join("z")).unwrap();
        std::fs::write(classes.join("z/B.class"), b"b").unwrap();
        std::fs::write(classes.join("A.class"), b"a").unwrap();
        write_jar(&classes, &d.join("1.jar")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        write_jar(&classes, &d.join("2.jar")).unwrap();
        assert_eq!(std::fs::read(d.join("1.jar")).unwrap(), std::fs::read(d.join("2.jar")).unwrap());
        let mut z = zip::ZipArchive::new(File::open(d.join("1.jar")).unwrap()).unwrap();
        let names: Vec<_> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        assert_eq!(names, [MANIFEST, "A.class", "z/B.class"]);
        let e = z.by_index(1).unwrap();
        assert_eq!(e.last_modified(), Some(zip::DateTime::default()));
        assert_eq!(e.unix_mode().map(|m| m & 0o777), Some(0o644));
    }

    #[test]
    fn version_control_and_editor_files_are_not_resources() {
        for n in [".gitignore", ".gitkeep", ".DS_Store", ".git", "a.txt~", "#a#", ".#a", "._a", "CVS"] {
            assert_eq!(default_excluded(n), n != ".gitkeep", "{n}");
        }
    }

    fn merged(keep_comments: bool) -> String {
        let mut body = vec![];
        merge_service_lines(&mut body, "# c\n#\none\n\n#\n", keep_comments);
        merge_service_lines(&mut body, "# c\none\n\ntwo\n", keep_comments);
        String::from_utf8(body).unwrap()
    }

    #[test]
    fn service_lines_merge_as_an_ordered_set_blank_and_comment_lines_included() {
        assert_eq!(merged(false), "# c\n#\none\n\ntwo\n");
    }

    #[test]
    fn older_shade_versions_keep_every_comment_and_blank_line() {
        assert_eq!(merged(true), "# c\n#\none\n\n#\n# c\n\ntwo\n");
    }

    fn shaded(cfg: &ShadeCfg) -> PathBuf {
        let props = BTreeMap::from([("project.version".to_string(), "1.0".to_string())]);
        shaded_path(cfg, Path::new("t"), "app", "1.0", &props)
    }

    #[test]
    fn shaded_jar_replaces_the_plain_jar_by_default() {
        assert_eq!(shaded(&ShadeCfg::default()), PathBuf::from("t/app-1.0.jar"));
    }

    #[test]
    fn attached_shaded_jar_takes_the_classifier_which_defaults_to_shaded() {
        let attached = ShadeCfg { attached: true, ..Default::default() };
        assert_eq!(shaded(&attached), PathBuf::from("t/app-1.0-shaded.jar"));
        let named = ShadeCfg { attached: true, classifier: Some("all".into()), ..Default::default() };
        assert_eq!(shaded(&named), PathBuf::from("t/app-1.0-all.jar"));
    }

    #[test]
    fn final_name_wins_and_is_interpolated() {
        let cfg = ShadeCfg { final_name: Some("x-${project.version}-fat".into()), attached: true, ..Default::default() };
        assert_eq!(shaded(&cfg), PathBuf::from("t/x-1.0-fat.jar"));
    }

    fn level(plugin: Option<(&str, &str, &str)>, props: &[(&str, &str)]) -> JavaLevel {
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let e = Effective {
            gav: crate::pom::Gav::new("g", "a", "1"),
            packaging: "jar".into(),
            props: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: plugin.map(|(r, s, t)| crate::pom::CompilerCfg { release: opt(r), source: opt(s), target: opt(t) }),
            surefire: None,
            failsafe: None,
            relocation: None,
            unrun: vec![],
        };
        JavaLevel::of(&e)
    }

    #[test]
    fn release_beats_source_and_target_and_plugin_beats_properties() {
        assert_eq!(level(None, &[]).args(), Vec::<String>::new());
        assert_eq!(level(None, &[("maven.compiler.source", "11"), ("maven.compiler.target", "11")]).args(), ["-source", "11", "-target", "11"]);
        assert_eq!(level(None, &[("maven.compiler.release", "17"), ("maven.compiler.target", "11")]).args(), ["--release", "17"]);
        assert_eq!(level(Some(("", "21", "21")), &[("maven.compiler.release", "17")]).args(), ["--release", "17"]);
        assert_eq!(level(Some(("", "21", "21")), &[("maven.compiler.target", "11")]).args(), ["-source", "21", "-target", "21"]);
    }

    #[test]
    fn an_unresolved_expression_counts_as_unset_like_in_maven() {
        assert_eq!(level(Some(("${maven.compiler.release}", "21", "21")), &[]).args(), ["-source", "21", "-target", "21"]);
    }

    #[test]
    fn plugin_values_are_interpolated() {
        let l = level(Some(("${rel}", "", "")), &[("rel", "17")]);
        assert_eq!(l.args(), ["--release", "17"]);
    }

    #[test]
    fn javac_command_passes_release_classpath_and_sources() {
        let cmd = javac_command(Path::new("out"), &level(None, &[("maven.compiler.release", "17")]), &["a.jar".into(), "b.jar".into()], &[PathBuf::from("A.java")]);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-d", "out", "-encoding", "UTF-8", "-proc:none", "-g", "--release", "17", "-cp", "a.jar:b.jar", "A.java"]);
    }
}
