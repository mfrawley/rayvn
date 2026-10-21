//! Stamps the binary with the git commit, a dirty flag, the build time and a name derived from the commit,
//! exposed as RAYVN_COMMIT, RAYVN_BUILT and RAYVN_NAME.

use std::process::Command;

const ADJECTIVES: [&str; 16] = [
    "Gloomy", "Curious", "Nimble", "Sardonic", "Mournful", "Cunning", "Glossy", "Restless",
    "Pensive", "Mischievous", "Midnight", "Wily", "Austere", "Whimsical", "Stoic", "Raucous",
];
const NOUNS: [&str; 16] = [
    "Quill", "Inkwell", "Lenore", "Chamber", "Bust of Pallas", "Tapping", "Nevermore", "Hatter",
    "Desk", "Lectern", "Parchment", "Cawing", "Shadow", "Casement", "Tea Party", "Manuscript",
];

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Hinnant's civil_from_days).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn utc(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

fn name_for(commit: &str) -> String {
    let n = u64::from_str_radix(commit.trim_end_matches("-dirty"), 16).unwrap_or(0) as usize;
    format!("{} {}", ADJECTIVES[n % 16], NOUNS[n / 16 % 16])
}

fn main() {
    // SOURCE_DATE_EPOCH keeps reproducible builds reproducible.
    let secs = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
    });
    let commit = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(c) if git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) => format!("{c}-dirty"),
        Some(c) => c,
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=RAYVN_COMMIT={commit}");
    println!("cargo:rustc-env=RAYVN_BUILT={}", utc(secs));
    println!("cargo:rustc-env=RAYVN_NAME={}", name_for(&commit));
    for p in ["build.rs", "src", "data", "Cargo.toml", ".git/HEAD", ".git/index"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
