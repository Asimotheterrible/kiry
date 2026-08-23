// one tree, three tars, and all three have to extract to the same thing. the target
// system runs busybox, and tar implementations disagree about long names, spaces
// and hardlinks in ways that reach the installed root

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kiry_core::{archive, db};

const TARS: &[(&str, &str)] = &[
    ("gnu", "tar"),
    ("bsdtar", "bsdtar"),
    ("busybox", "busybox tar"),
];

fn have(cmd: &str) -> bool {
    let probe = format!("command -v {} >/dev/null", cmd.split(' ').next().unwrap_or(cmd));
    Command::new("sh")
        .arg("-c")
        .arg(probe)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn build_tree(at: &Path) {
    let long = "a-directory-name-long-enough-to-push-the-whole-path-past-the-hundred-\
                character-field-in-a-tar-header";

    fs::create_dir_all(at.join("usr/bin")).unwrap();
    fs::create_dir_all(at.join("usr/share/with space")).unwrap();
    fs::create_dir_all(at.join(format!("usr/share/doc/{long}"))).unwrap();

    fs::write(at.join("usr/bin/tool"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(at.join("usr/bin/tool"), perms(0o755)).unwrap();

    fs::write(at.join("usr/share/with space/file with spaces.txt"), b"spaces\n").unwrap();
    fs::write(at.join(format!("usr/share/doc/{long}/readme")), b"long\n").unwrap();
    fs::write(at.join("usr/share/empty"), b"").unwrap();

    std::os::unix::fs::symlink("tool", at.join("usr/bin/link")).unwrap();
    std::os::unix::fs::symlink(
        format!("../share/doc/{long}/readme"),
        at.join("usr/bin/faraway"),
    )
    .unwrap();

    // no hardlink here. which of the two names ends up as the link depends on
    // readdir order, not on the tar, so it would fail on some filesystems for a
    // reason that is not a bug
}

fn perms(mode: u32) -> fs::Permissions {
    <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode)
}

fn extract_with(tar: &str, src: &Path, at: &Path, name: &str) -> Vec<db::Entry> {
    let arc = at.join(format!("{name}.tar.zst"));
    let root = at.join(format!("{name}-root"));
    fs::create_dir_all(&root).unwrap();

    let sh = format!(
        "cd {} && {tar} cf - . | zstd -q -o {}",
        src.display(),
        arc.display()
    );
    let ok = Command::new("sh").arg("-c").arg(&sh).status().unwrap();
    assert!(ok.success(), "{tar} failed to build the corpus");

    let mut m = archive::extract(&root, &arc).unwrap();
    m.sort_by(|a, b| a.path.cmp(&b.path));
    m
}

#[test]
fn every_tar_extracts_to_the_same_tree() {
    let missing: Vec<&str> = TARS
        .iter()
        .map(|(_, c)| *c)
        .chain(["zstd"])
        .filter(|c| !have(c))
        .collect();

    if !missing.is_empty() {
        // skipping would certify a package manager that cannot install a normal
        // tarball, which is how the last attempt shipped two bugs
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "missing {missing:?}. install them, or set KIRY_TEST_ALLOW_SKIP=1"
        );
        return;
    }

    let d = tmp("corpus");
    let src = d.join("src");
    build_tree(&src);

    let mut seen: Vec<(&str, Vec<db::Entry>)> = Vec::new();
    for (name, tar) in TARS {
        seen.push((name, extract_with(tar, &src, &d, name)));
    }

    let (first_name, first) = &seen[0];
    for (name, m) in &seen[1..] {
        assert_eq!(
            first, m,
            "{first_name} and {name} disagree about what the tree is"
        );
    }

    let paths: Vec<&str> = first.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"usr/bin/tool"));
    assert!(paths.iter().any(|p| p.contains("file with spaces.txt")));
    assert!(paths.iter().any(|p| p.len() > 100));
}
