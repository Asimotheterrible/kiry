// b drives sh, tar, zstd and curl, so the only honest test is the real binary

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use kiry_core::pkg::Version;
use kiry_core::{db, install};

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

fn cache(root: &Path, suffix: &str) -> Vec<String> {
    let Ok(rd) = fs::read_dir(root.join("var/kiry/cache")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.ends_with(suffix))
        .collect();
    out.sort();
    out
}

fn artifacts(root: &Path) -> Vec<String> {
    cache(root, ".tar.zst")
}

fn sidecars(root: &Path) -> Vec<String> {
    cache(root, ".meta")
}

// phase 1 starts from a rootfs nobody built either, and the installed database is plain
// text precisely so a package can be written by hand. one busybox is the whole toolchain
// these recipes need: their scripts run echo, mkdir and cp, never a compiler
fn bootstrap(root: &Path) -> bool {
    let bb = PathBuf::from("/usr/bin/busybox");
    if !bb.is_file() {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "no busybox to seed the sandbox toolchain with"
        );
        return false;
    }

    // ldd names the loader too, which walking DT_NEEDED by hand does not
    let out = Command::new("ldd").arg(&bb).output().unwrap();
    let mut files: Vec<(String, PathBuf)> = vec![("usr/bin/busybox".into(), bb.clone())];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let path = match line.split_whitespace().collect::<Vec<_>>()[..] {
            [p, ..] if p.starts_with('/') => p.to_string(),
            [_, "=>", p, ..] if p.starts_with('/') => p.to_string(),
            _ => continue,
        };
        files.push((
            path.trim_start_matches('/').to_string(),
            PathBuf::from(path),
        ));
    }

    let mut manifest = Vec::new();
    for (at, from) in &files {
        let dst = root.join(at);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(from, &dst).unwrap();
        manifest.push(db::Entry {
            mode: 0o755,
            kind: db::Kind::File(kiry_core::sha256(fs::File::open(&dst).unwrap()).unwrap()),
            path: at.clone(),
        });
    }
    // busybox picks its applet out of argv[0], so this is the shell
    std::os::unix::fs::symlink("busybox", root.join("usr/bin/sh")).unwrap();
    manifest.push(db::Entry {
        mode: 0o777,
        kind: db::Kind::Link("busybox".into()),
        path: "usr/bin/sh".into(),
    });

    let provides: Vec<db::Provide> = install::scan(root, &manifest)
        .unwrap()
        .into_iter()
        .filter_map(|(path, s)| {
            let install::Seen::Elf(o) = s else {
                return None;
            };
            Some(db::Provide {
                soname: o.soname?,
                versioned: o.versioned,
                path,
            })
        })
        .collect();
    for target in ["x86_64-musl", "x86_64-gnu"] {
        db::write(
            root,
            &db::Installed {
                name: "busybox".into(),
                target: target.into(),
                version: Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: manifest.clone(),
            },
        )
        .unwrap();
        db::write_provides(root, target, "busybox", &provides).unwrap();
    }

    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/toolchain"), "busybox\n").unwrap();
    true
}

#[test]
fn round_trip_through_install() {
    let at = scratch("round-trip");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

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
    let listed = String::from_utf8_lossy(&o.stdout);
    assert!(
        listed.lines().any(|l| l == "hello 1.0 x86_64-musl"),
        "{listed}"
    );
}

// the one above dies before anything is packed. this one dies inside tar
#[test]
fn a_target_dying_while_it_packs_leaves_no_sidecar() {
    let at = scratch("sidecar");
    let script = format!("{GOOD}[ \"$KIRY_TARGET\" = x86_64-musl ] || rm -rf \"$DESTDIR\"\n");
    let d = recipe(&at, "x86_64-musl x86_64-gnu", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(artifacts(&root).is_empty(), "{:?}", artifacts(&root));
    assert!(sidecars(&root).is_empty(), "{:?}", sidecars(&root));
}

#[test]
fn one_target_failing_cancels_the_other() {
    let at = scratch("atomic");
    let script = format!("{GOOD}[ \"$KIRY_TARGET\" = x86_64-musl ] || exit 1\n");
    let d = recipe(&at, "x86_64-musl x86_64-gnu", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

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
    if !bootstrap(&root) {
        return;
    }

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
    if !bootstrap(&root) {
        return;
    }

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
    if !bootstrap(&root) {
        return;
    }
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
    if !bootstrap(&root) {
        return;
    }

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
    if !bootstrap(&root) {
        return;
    }

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

// a library the loader reaches through a symlink has to land in the sysroot with the
// link, not only the file the link points at. provides records the file carrying the
// soname, and DT_NEEDED almost never names that file
#[test]
fn a_transitive_library_arrives_with_the_link_that_names_it() {
    let at = scratch("linked-dep");
    let root = at.join("root");
    if !bootstrap(&root) {
        return;
    }
    if !have_cc() {
        return;
    }

    // mid is declared by the recipe, so deep is reached one hop further out and is not
    // a direct member of anything
    shared(&at, &root, "deep");
    bare(&root, "mid", &["deep"]);

    let d = recipe(
        &at,
        "x86_64-musl",
        "test -L /usr/lib/libdeep.so.1\ntest -f /usr/lib/libdeep.so.1.2.3\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    fs::write(d.join("depends"), "mid\n").unwrap();

    let out = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "the sysroot is missing the name the loader would open: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn have_cc() -> bool {
    if Command::new("cc")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return true;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "no cc to build a library with a soname"
    );
    false
}

// the real file plus the name that would sit in DT_NEEDED
fn shared(at: &Path, root: &Path, name: &str) {
    let src = at.join(format!("{name}.c"));
    fs::write(&src, "void p(void){}\n").unwrap();
    let real = format!("usr/lib/lib{name}.so.1.2.3");
    let dst = root.join(&real);
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg(format!("-Wl,-soname,lib{name}.so.1"))
        .arg("-o")
        .arg(&dst)
        .arg(&src)
        .status()
        .unwrap()
        .success());

    let link = format!("usr/lib/lib{name}.so.1");
    let _ = fs::remove_file(root.join(&link));
    std::os::unix::fs::symlink(format!("lib{name}.so.1.2.3"), root.join(&link)).unwrap();

    let manifest = vec![
        db::Entry {
            mode: 0o755,
            kind: db::Kind::File(
                kiry_core::sha256(fs::File::open(root.join(&real)).unwrap()).unwrap(),
            ),
            path: real,
        },
        db::Entry {
            mode: 0o777,
            kind: db::Kind::Link(format!("lib{name}.so.1.2.3")),
            path: link,
        },
    ];
    let provides: Vec<db::Provide> = install::scan(root, &manifest)
        .unwrap()
        .into_iter()
        .filter_map(|(path, s)| {
            let install::Seen::Elf(o) = s else {
                return None;
            };
            Some(db::Provide {
                soname: o.soname?,
                versioned: o.versioned,
                path,
            })
        })
        .collect();
    record(root, name, &[], manifest);
    db::write_provides(root, "x86_64-musl", name, &provides).unwrap();
}

fn bare(root: &Path, name: &str, deps: &[&str]) {
    record(root, name, deps, Vec::new());
    db::write_provides(root, "x86_64-musl", name, &[]).unwrap();
}

fn record(root: &Path, name: &str, deps: &[&str], manifest: Vec<db::Entry>) {
    db::write(
        root,
        &db::Installed {
            name: name.to_string(),
            target: "x86_64-musl".into(),
            version: Version::parse("1.0 1").unwrap(),
            depends: deps
                .iter()
                .map(|d| kiry_core::pkg::Dep {
                    name: (*d).to_string(),
                    make: false,
                })
                .collect(),
            manifest,
        },
    )
    .unwrap();
}

// rust ignores SIGPIPE and turns the write error into a panic, so `kiry l | head` used
// to print a backtrace where every other unix tool goes quiet
#[test]
fn a_closed_pipe_is_not_a_panic() {
    let at = scratch("pipe");
    let root = at.join("root");
    if !bootstrap(&root) {
        return;
    }

    // more output than a pipe buffer holds, so the writer is still going when the
    // reader leaves. two lines would race and prove nothing
    for i in 0..2500 {
        db::write(
            &root,
            &db::Installed {
                name: format!("filler{i:04}"),
                target: "x86_64-musl".into(),
                version: Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: Vec::new(),
            },
        )
        .unwrap();
    }

    let mut c = Command::new(KIRY)
        .args(["l", "--root", root.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // the reader walks away before the first line is written
    drop(c.stdout.take());
    let out = c.wait_with_output().unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.status.success(), "{:?}", out.status);
}
