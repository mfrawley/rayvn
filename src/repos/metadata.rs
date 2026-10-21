//! Maven repository layout for SNAPSHOTs and `maven-metadata.xml`: where files live and which build is newest.

use std::path::{Path, PathBuf};

pub(super) fn snapshot_dir(group: &str, artifact: &str, version: &str) -> String {
    format!("{}/{artifact}/{version}", group.replace('.', "/"))
}

pub(super) fn snapshot_rel(
    dir: &str,
    artifact: &str,
    value: &str,
    classifier: Option<&str>,
    ext: &str,
) -> String {
    let c = classifier.map(|c| format!("-{c}")).unwrap_or_default();
    format!("{dir}/{artifact}-{value}{c}.{ext}")
}

pub(super) fn metadata_path(local: &Path, repo_id: &str) -> PathBuf {
    local
        .parent()
        .unwrap()
        .join(format!("maven-metadata-{repo_id}.xml"))
}

/// The `<versions>` of a `maven-metadata.xml`, in the order listed; empty if the file is not valid metadata.
pub(super) fn listed_versions(xml: &str) -> Vec<String> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return vec![];
    };
    doc.descendants()
        .find(|n| n.has_tag_name("versions"))
        .map(|vs| {
            vs.children()
                .filter(|c| c.has_tag_name("version"))
                .filter_map(|c| c.text())
                .map(|t| t.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The timestamped version of a SNAPSHOT file listed in a repository's `maven-metadata.xml`.
pub(super) fn snapshot_value(
    xml: &str,
    version: &str,
    classifier: Option<&str>,
    ext: &str,
) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let text = |n: roxmltree::Node, name: &str| {
        n.children()
            .find(|c| c.has_tag_name(name))
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
    };
    // Some repositories list every historical build; the newest `<updated>` wins (later entries win ties), as in Maven.
    let newest = doc
        .descendants()
        .filter(|n| n.has_tag_name("snapshotVersion"))
        .filter(|sv| {
            text(*sv, "extension").as_deref() == Some(ext)
                && text(*sv, "classifier").as_deref() == classifier
        })
        .max_by_key(|sv| text(*sv, "updated").unwrap_or_default());
    if let Some(value) = newest.and_then(|sv| text(sv, "value")) {
        return Some(value);
    }
    let snap = doc.descendants().find(|n| n.has_tag_name("snapshot"))?;
    let (ts, build) = (text(snap, "timestamp")?, text(snap, "buildNumber")?);
    Some(version.trim_end_matches("SNAPSHOT").to_string() + &format!("{ts}-{build}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_maven_metadata() {
        let xml = "<metadata><groupId>g</groupId><versioning><latest>2</latest><versions><version>1.0</version><version> 1.5 </version><version>2</version></versions></versioning></metadata>";
        assert_eq!(listed_versions(xml), ["1.0", "1.5", "2"]);
        assert!(listed_versions("<metadata/>").is_empty());
        assert!(listed_versions("not xml").is_empty());
    }

    #[test]
    fn snapshot_value_picks_the_newest_listed_build() {
        let xml = "<metadata><versioning><snapshot><timestamp>20240724.090251</timestamp><buildNumber>12</buildNumber></snapshot><snapshotVersions>\
            <snapshotVersion><extension>jar</extension><value>v-20220210.121839-1</value><updated>20220210121843</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><value>v-20240724.090251-12</value><updated>20240724090441</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><value>v-20230929.101120-10</value><updated>20230929101207</updated></snapshotVersion>\
            <snapshotVersion><extension>jar</extension><classifier>sources</classifier><value>v-1</value><updated>1</updated></snapshotVersion>\
            </snapshotVersions></versioning></metadata>";
        assert_eq!(
            snapshot_value(xml, "v-SNAPSHOT", None, "jar").as_deref(),
            Some("v-20240724.090251-12")
        );
        assert_eq!(
            snapshot_value(xml, "v-SNAPSHOT", Some("sources"), "jar").as_deref(),
            Some("v-1")
        );
        assert_eq!(
            snapshot_value(xml, "v-SNAPSHOT", None, "pom").as_deref(),
            Some("v-20240724.090251-12")
        );
    }
}
