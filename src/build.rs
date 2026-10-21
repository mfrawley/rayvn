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

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else {
            out.push(p);
        }
    }
    out.sort();
    Ok(())
}

pub fn build(root: &Effective, dir: &Path, resolved: &[Resolved]) -> Result<()> {
    let target = dir.join("target");
    let classes = target.join("classes");
    std::fs::create_dir_all(&classes)?;

    compile(root, &dir.join("src/main/java"), &classes, &compile_classpath(resolved))?;
    copy_resources(&dir.join("src/main/resources"), &classes)?;

    let jar_path = target.join(format!("{}-{}.jar", root.gav.artifact, root.gav.version));
    write_jar(&classes, &jar_path)?;
    eprintln!("  packaged {} ({})", jar_path.display(), size(&jar_path));
    if let Some(cfg) = &root.shade {
        shade_in_place(root, cfg, &target, &jar_path, resolved)?;
    }
    println!("{}", jar_path.display());
    Ok(())
}

/// Compile-time classpath: everything except test-scoped jars, in resolution order.
fn compile_classpath(resolved: &[Resolved]) -> Vec<String> {
    resolved
        .iter()
        .filter(|r| r.scope != "test")
        .filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string()))
        .collect()
}

fn java_sources(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut sources = vec![];
    walk(dir, &mut sources)?;
    sources.retain(|p| p.extension().is_some_and(|e| e == "java"));
    Ok(sources)
}

fn compiler_release(props: &BTreeMap<String, String>) -> Option<String> {
    props.get("maven.compiler.release").or_else(|| props.get("maven.compiler.target")).cloned()
}

fn javac_command(classes: &Path, release: Option<&str>, classpath: &[String], sources: &[PathBuf]) -> Command {
    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(classes).args(["-encoding", "UTF-8", "-proc:none", "-g"]);
    if let Some(r) = release {
        cmd.arg("--release").arg(r);
    }
    if !classpath.is_empty() {
        cmd.arg("-cp").arg(classpath.join(":"));
    }
    cmd.args(sources);
    cmd
}

fn compile(root: &Effective, src_dir: &Path, classes: &Path, classpath: &[String]) -> Result<()> {
    let sources = java_sources(src_dir)?;
    if sources.is_empty() {
        return Ok(());
    }
    let release = compiler_release(&root.props);
    eprintln!(
        "  compiling {} sources with javac{}, {} classpath entries",
        sources.len(),
        release.as_ref().map(|r| format!(" --release {r}")).unwrap_or_default(),
        classpath.len()
    );
    let status = javac_command(classes, release.as_deref(), classpath, &sources).status().context("running javac")?;
    if !status.success() {
        bail!("javac failed");
    }
    Ok(())
}

fn copy_resources(res_dir: &Path, classes: &Path) -> Result<()> {
    let mut resources = vec![];
    walk(res_dir, &mut resources)?;
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

fn deflated() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
}

/// Replaces `jar_path` with a shaded jar; the plain jar is kept as `original-<name>`.
fn shade_in_place(root: &Effective, cfg: &ShadeCfg, target: &Path, jar_path: &Path, resolved: &[Resolved]) -> Result<()> {
    let original = target.join(format!("original-{}-{}.jar", root.gav.artifact, root.gav.version));
    std::fs::rename(jar_path, &original)?;
    for i in &cfg.ignored {
        eprintln!("  warning: shade option not supported by rayvn, ignored: {i}");
    }
    let n = shade(cfg, &original, &shaded_dependencies(resolved), jar_path)?;
    eprintln!("  shaded {} dependency jars into {} ({})", n, jar_path.display(), size(jar_path));
    Ok(())
}

/// `group:artifact` and jar of every compile/runtime dependency, in resolution order.
fn shaded_dependencies(resolved: &[Resolved]) -> Vec<(String, PathBuf)> {
    resolved
        .iter()
        .filter(|r| matches!(r.scope.as_str(), "compile" | "runtime"))
        .filter_map(|r| Some((format!("{}:{}", r.gav.group, r.gav.artifact), r.jar.clone()?)))
        .collect()
}

fn size(p: &Path) -> String {
    let b = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if b >= 1 << 20 { format!("{:.1} MB", b as f64 / 1048576.0) } else { format!("{} KB", b / 1024) }
}

/// Ant-style match: `*` stays inside a path segment, `**` crosses `/`.
fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pat.as_bytes(), s.as_bytes())
}

fn excluded(cfg: &ShadeCfg, ga: &str, name: &str) -> bool {
    cfg.excludes.iter().any(|(art, ex)| glob(&art.replace('*', "**"), ga) && ex.iter().any(|g| glob(g, name) || glob(&format!("{g}/**"), name)))
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

/// Appends `content` to `body`: blank lines are dropped, comment lines always kept, other lines only if new.
fn merge_service_lines(body: &mut Vec<u8>, content: &str) {
    let mut known: HashSet<String> = String::from_utf8_lossy(body).lines().map(str::to_string).collect();
    for line in content.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()) {
        if line.starts_with('#') || known.insert(line.to_string()) {
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
            merge_service_lines(services.entry(name).or_default(), &String::from_utf8_lossy(&buf));
            continue;
        }
        seen.insert(name);
        zip.raw_copy_file(archive.by_index_raw(i)?)?;
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
    fn service_lines_merge_without_duplicates_or_blank_lines_but_keep_comments() {
        let mut body = b"one\n".to_vec();
        merge_service_lines(&mut body, "one\n\n# c\ntwo  \n  \none\n# c\nthree\n");
        assert_eq!(String::from_utf8(body).unwrap(), "one\n# c\ntwo\n# c\nthree\n");
    }

    #[test]
    fn compiler_release_prefers_release_over_target() {
        let mut p = BTreeMap::new();
        assert_eq!(compiler_release(&p), None);
        p.insert("maven.compiler.target".into(), "11".into());
        assert_eq!(compiler_release(&p).as_deref(), Some("11"));
        p.insert("maven.compiler.release".into(), "17".into());
        assert_eq!(compiler_release(&p).as_deref(), Some("17"));
    }

    #[test]
    fn javac_command_passes_release_classpath_and_sources() {
        let cmd = javac_command(Path::new("out"), Some("17"), &["a.jar".into(), "b.jar".into()], &[PathBuf::from("A.java")]);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-d", "out", "-encoding", "UTF-8", "-proc:none", "-g", "--release", "17", "-cp", "a.jar:b.jar", "A.java"]);
    }
}
