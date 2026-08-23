// b drives sh, tar, zstd and curl, so the only honest test is the real binary

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const KIRY: &str = env!("CARGO_BIN_EXE_kiry");

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .join(format!("kiry-b-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// a tarball holding one top directory, like every upstream release
fn tarball(at: &Path) -> PathBuf {
    let top = at.join("src/hello-1.0");
    fs::create_dir_all(&top).unwrap();
    fs::write(top.join("greeting"), "hi\n").unwrap();

    let arc = at.join("hello-1.0.tar");
    assert!(Command::new("tar")
        .arg("-cf")
        .arg(&arc)
        .arg("-C")
        .arg(at.join("src"))
        .arg("hello-1.0")
        .status()
        .unwrap()
        .success());
    arc
}

fn recipe(at: &Path, targets: &str, script: &str) -> PathBuf {
    let arc = tarball(at);
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();

    let d = at.join("hello");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.0 1\n").unwrap();
    fs::write(d.join("targets"), format!("{targets}\n")).unwrap();
    fs::write(d.join("sources"), "../hello-1.0.tar\n").unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();
    fs::write(d.join("build"), script).unwrap();
    d
}

const GOOD: &str =
    "echo chatter\nmkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n";

fn kiry(args: &[&str]) -> Output {
    Command::new(KIRY).args(args).output().unwrap()
}

fn artifacts(root: &Path) -> Vec<String> {
    let cache = root.join("var/kiry/cache");
    let Ok(rd) = fs::read_dir(cache) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.ends_with(".tar.zst"))
        .collect();
    out.sort();
    out
}

#[test]
fn round_trip_through_install() {
    let at = scratch("round-trip");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(artifacts(&root), ["hello-1.0-1.x86_64-musl.tar.zst"]);

    let said = String::from_utf8_lossy(&o.stdout);
    assert!(!said.contains("chatter"), "build output leaked: {said}");
    let log = root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log");
    assert!(fs::read_to_string(&log).unwrap().contains("chatter"));

    let arc = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst");
    let meta = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst.meta");
    assert_eq!(fs::read_to_string(meta.join("name")).unwrap(), "hello\n");
    assert_eq!(fs::read_to_string(meta.join("version")).unwrap(), "1.0 1\n");
    assert_eq!(
        fs::read_to_string(meta.join("targets")).unwrap(),
        "x86_64-musl\n"
    );
    assert_eq!(fs::read_to_string(meta.join("hash")).unwrap().len(), 65);

    let o = kiry(&["i", "--root", root.to_str().unwrap(), arc.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read_to_string(root.join("usr/bin/hello")).unwrap(),
        "hi\n"
    );

    let o = kiry(&["l", "--root", root.to_str().unwrap()]);
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        "hello 1.0 x86_64-musl"
    );
}

#[test]
fn one_target_failing_cancels_the_other() {
    let at = scratch("atomic");
    let script = format!("{GOOD}[ \"$KIRY_TARGET\" = x86_64-musl ] || exit 1\n");
    let d = recipe(&at, "x86_64-musl x86_64-gnu", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(artifacts(&root).is_empty(), "{:?}", artifacts(&root));
}

#[test]
fn targets_agree_on_the_hash() {
    let at = scratch("hash");
    let d = recipe(&at, "x86_64-musl x86_64-gnu", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let cache = root.join("var/kiry/cache");
    let musl = fs::read_to_string(cache.join("hello-1.0-1.x86_64-musl.tar.zst.meta/hash")).unwrap();
    let gnu = fs::read_to_string(cache.join("hello-1.0-1.x86_64-gnu.tar.zst.meta/hash")).unwrap();
    assert_eq!(musl, gnu);
}

#[test]
fn a_wrong_checksum_stops_the_build() {
    let at = scratch("checksum");
    let d = recipe(&at, "x86_64-musl", GOOD);
    fs::write(d.join("checksums"), format!("{}\n", "0".repeat(64))).unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("recipe says"));
    assert!(artifacts(&root).is_empty());
}

#[test]
fn fetches_once_and_keeps_it() {
    let at = scratch("fetch");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let arc = at.join("hello-1.0.tar");
    fs::write(
        d.join("sources"),
        format!("https://example/{}\n", "hello-1.0.tar"),
    )
    .unwrap();

    // stands in for curl, and counts how often it ran
    let tally = at.join("tally");
    let fetcher = at.join("fetch.sh");
    fs::write(
        &fetcher,
        format!(
            "#!/bin/sh\necho x >> {}\ncp {} \"$2\"\n",
            tally.display(),
            arc.display()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&fetcher)
        .status()
        .unwrap();

    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let args = ["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()];

    for _ in 0..2 {
        let o = Command::new(KIRY)
            .args(args)
            .env("KIRY_FETCH", format!("{} %u %o", fetcher.display()))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    assert_eq!(fs::read_to_string(&tally).unwrap(), "x\n");
    assert!(root.join("var/kiry/cache/sources/hello-1.0.tar").exists());
}

#[test]
fn a_failed_build_points_at_its_log() {
    let at = scratch("log");
    let d = recipe(&at, "x86_64-musl", "echo wrecked >&2\nexit 3\n");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());

    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("hello-1.0-1.x86_64-musl.log"), "{said}");
    let log = root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log");
    assert!(fs::read_to_string(&log).unwrap().contains("wrecked"));
}

#[test]
fn dash_v_streams_it_instead() {
    let at = scratch("verbose");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&[
        "b",
        "--root",
        root.to_str().unwrap(),
        "-v",
        d.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("chatter"));
}
