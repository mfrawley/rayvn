//! Local staging of built artifacts (JARs, aggregator/parent POMs, attached artifacts)
//! and their checksums (`.sha256`, `.sha1`, `.md5`) into a Maven repository layout.

use crate::model::Effective;
use crate::pom::Gav;
use anyhow::{Context, Result, bail};
use sha1::Digest as _;
use sha2::Digest as _;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Checksums calculated for a staged file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksums {
    pub sha256: String,
    pub sha1: String,
    pub md5: String,
}

/// Computes SHA-256, SHA-1, and MD5 in a single streaming pass over `p`.
pub fn hash_file(p: &Path) -> Result<Checksums> {
    let mut f = File::open(p).with_context(|| format!("reading {}", p.display()))?;
    let mut s256 = sha2::Sha256::new();
    let mut s1 = sha1::Sha1::new();
    let mut m = md5::Context::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        s256.update(&buf[..n]);
        s1.update(&buf[..n]);
        m.consume(&buf[..n]);
    }
    Ok(Checksums {
        sha256: hex(&s256.finalize()),
        sha1: hex(&s1.finalize()),
        md5: format!("{:x}", m.compute()),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `.sha256`, `.sha1`, and `.md5` sidecar files for `file`.
pub fn write_checksum_sidecars(file: &Path, cs: &Checksums) -> Result<()> {
    let write = |ext: &str, content: &str| -> Result<()> {
        let sidecar = file.with_file_name(format!(
            "{}.{ext}",
            file.file_name().unwrap().to_string_lossy()
        ));
        let mut f = File::create(&sidecar)
            .with_context(|| format!("writing checksum {}", sidecar.display()))?;
        f.write_all(content.as_bytes())?;
        Ok(())
    };
    write("sha256", &cs.sha256)?;
    write("sha1", &cs.sha1)?;
    write("md5", &cs.md5)?;
    Ok(())
}

/// Relative directory for a GAV inside a Maven repository layout: `<group>/<artifact>/<version>`.
pub fn gav_rel_dir(gav: &Gav) -> PathBuf {
    let group_dir = gav.group.replace('.', "/");
    PathBuf::from(group_dir)
        .join(&gav.artifact)
        .join(&gav.version)
}

/// Represents one staged artifact and its sidecars.
#[derive(Debug, Clone)]
pub struct StagedArtifact {
    pub file: PathBuf,
    pub size: u64,
    #[allow(dead_code)]
    pub checksums: Checksums,
}

/// Stages `source` into `dest_dir`, writing sidecar checksums. Returns the staged artifact info.
pub fn stage_file(source: &Path, dest_dir: &Path) -> Result<StagedArtifact> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating directory {}", dest_dir.display()))?;
    let name = source.file_name().context("source file has no file name")?;
    let dest = dest_dir.join(name);
    std::fs::copy(source, &dest)
        .with_context(|| format!("staging {} to {}", source.display(), dest.display()))?;
    let checksums = hash_file(&dest)?;
    write_checksum_sidecars(&dest, &checksums)?;
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    Ok(StagedArtifact {
        file: dest,
        size,
        checksums,
    })
}

/// Stages all built artifacts of `module` (POM, JAR, and any shaded/attached JARs) into `stage_dir`.
pub fn stage_module(
    effective: &Effective,
    module_dir: &Path,
    stage_dir: &Path,
) -> Result<Vec<StagedArtifact>> {
    let target = module_dir.join("target");
    let dest_dir = stage_dir.join(gav_rel_dir(&effective.gav));
    let mut staged = vec![];

    // 1. Stage the POM
    let pom_name = format!("{}-{}.pom", effective.gav.artifact, effective.gav.version);
    let pom_file = target.join(&pom_name);
    if !pom_file.is_file() {
        bail!(
            "missing POM artifact {} for module '{}'; run `rvn build` first",
            pom_file.display(),
            effective.gav.artifact
        );
    }
    staged.push(stage_file(&pom_file, &dest_dir)?);

    // 2. Stage the JAR if packaging == "jar"
    if effective.packaging == "jar" {
        let jar_name = format!("{}-{}.jar", effective.gav.artifact, effective.gav.version);
        let jar_file = target.join(&jar_name);
        if !jar_file.is_file() {
            bail!(
                "missing JAR artifact {} for module '{}'; run `rvn build` first",
                jar_file.display(),
                effective.gav.artifact
            );
        }
        staged.push(stage_file(&jar_file, &dest_dir)?);

        // Check for attached or finalName shaded jar if configured
        if let Some(shade) = &effective.shade {
            let shaded_file = crate::build::shaded_path(
                shade,
                &target,
                &effective.gav.artifact,
                &effective.gav.version,
                &effective.props,
            );
            if shaded_file.is_file() && shaded_file != jar_file {
                staged.push(stage_file(&shaded_file, &dest_dir)?);
            }
        }
    }

    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_computes_sha256_sha1_and_md5() {
        let temp = std::env::temp_dir().join(format!("rvn-hash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let file = temp.join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();

        let cs = hash_file(&file).unwrap();
        assert_eq!(
            cs.sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(cs.sha1, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
        assert_eq!(cs.md5, "5eb63bbbe01eeed093cb22bb8f5acdc3");

        write_checksum_sidecars(&file, &cs).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.join("test.txt.sha256")).unwrap(),
            cs.sha256
        );
        assert_eq!(
            std::fs::read_to_string(temp.join("test.txt.sha1")).unwrap(),
            cs.sha1
        );
        assert_eq!(
            std::fs::read_to_string(temp.join("test.txt.md5")).unwrap(),
            cs.md5
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn gav_rel_dir_follows_maven_layout() {
        let gav = Gav::new("io.moia.transport", "core", "1.2.3");
        assert_eq!(
            gav_rel_dir(&gav),
            PathBuf::from("io/moia/transport/core/1.2.3")
        );
    }

    #[test]
    fn stage_module_stages_shaded_fat_jar() {
        use std::collections::BTreeMap;
        let temp = std::env::temp_dir().join(format!("rvn-stage-fat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let target = temp.join("target");
        std::fs::create_dir_all(&target).unwrap();

        std::fs::write(target.join("my-app-1.0.pom"), b"<project/>").unwrap();
        std::fs::write(target.join("my-app-1.0.jar"), b"plain jar").unwrap();
        std::fs::write(target.join("my-app-1.0-fat.jar"), b"fat jar").unwrap();

        let mut props = BTreeMap::new();
        props.insert("artifactId".into(), "my-app".into());
        props.insert("version".into(), "1.0".into());

        let eff = Effective {
            gav: Gav::new("com.example", "my-app", "1.0"),
            parent: None,
            packaging: "jar".into(),
            props,
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: Some(crate::pom::ShadeCfg {
                final_name: Some("${artifactId}-${version}-fat".into()),
                attached: true,
                classifier: Some("shaded".into()),
                ..Default::default()
            }),
            compiler: None,
            surefire: None,
            failsafe: None,
            relocation: None,
            distribution: None,
            unrun: vec![],
        };

        let stage_dir = temp.join("staging");
        let staged = stage_module(&eff, &temp, &stage_dir).unwrap();
        let names: Vec<_> = staged
            .iter()
            .map(|s| s.file.file_name().unwrap().to_str().unwrap().to_string())
            .collect();

        assert_eq!(
            names,
            vec!["my-app-1.0.pom", "my-app-1.0.jar", "my-app-1.0-fat.jar"]
        );
        assert!(
            stage_dir
                .join("com/example/my-app/1.0/my-app-1.0-fat.jar.sha256")
                .is_file()
        );
        let _ = std::fs::remove_dir_all(&temp);
    }
}
