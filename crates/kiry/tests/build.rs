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

// a transitive dependency is declared, so it is not ambience -- autoconf is not autoconf
// without the perl that runs it. only what a configure script reads to decide a feature
// is there stays behind the direct/transitive line
#[test]
fn a_transitive_dependency_keeps_everything_but_its_headers() {
    let at = scratch("transitive");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    // deep: a tool nobody declares directly, carrying a binary and a header
    let deep = recipe(
        &at,
        "x86_64-musl",
        "mkdir -p \"$DESTDIR/usr/bin\" \"$DESTDIR/usr/include\" \"$DESTDIR/usr/lib/pkgconfig\"\n\
         printf '#!/bin/sh\\necho deep-ran\\n' > \"$DESTDIR/usr/bin/deeptool\"\n\
         chmod 755 \"$DESTDIR/usr/bin/deeptool\"\n\
         echo 'int deep;' > \"$DESTDIR/usr/include/deep.h\"\n\
         echo 'Name: deep' > \"$DESTDIR/usr/lib/pkgconfig/deep.pc\"\n",
    );
    fs::write(deep.join("name"), "deep\n").ok();
    let deep2 = deep.parent().unwrap().join("deep");
    let _ = fs::rename(&deep, &deep2);
    let o = kiry(&[
        "b",
        "--root",
        root.to_str().unwrap(),
        deep2.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let art = cache(&root, ".tar.zst");
    let a = format!("{}/var/kiry/cache/{}", root.display(), art[0]);
    assert!(kiry(&["i", "--root", root.to_str().unwrap(), &a])
        .status
        .success());

    // mid depends on deep at runtime; top declares only mid
    let mid = deep2.parent().unwrap().join("mid");
    fs::create_dir_all(&mid).unwrap();
    fs::write(mid.join("version"), "1 0\n").unwrap();
    fs::write(mid.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(
        mid.join("depends"),
        format!("{}\n", deep2.file_name().unwrap().to_str().unwrap()),
    )
    .unwrap();
    fs::write(mid.join("sources"), "").unwrap();
    fs::write(mid.join("checksums"), "").unwrap();
    fs::write(
        mid.join("build"),
        "mkdir -p \"$DESTDIR/usr/bin\"\necho mid > \"$DESTDIR/usr/bin/midtool\"\n",
    )
    .unwrap();
    let o = kiry(&["b", "--root", root.to_str().unwrap(), mid.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let mida = cache(&root, ".tar.zst")
        .into_iter()
        .find(|n| n.starts_with("mid-"))
        .unwrap();
    let a = format!("{}/var/kiry/cache/{mida}", root.display());
    assert!(kiry(&["i", "--root", root.to_str().unwrap(), &a])
        .status
        .success());

    let top = mid.parent().unwrap().join("top");
    fs::create_dir_all(&top).unwrap();
    fs::write(top.join("version"), "1 0\n").unwrap();
    fs::write(top.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(top.join("depends"), "mid make\n").unwrap();
    fs::write(top.join("sources"), "").unwrap();
    fs::write(top.join("checksums"), "").unwrap();
    fs::write(
        top.join("build"),
        "mkdir -p \"$DESTDIR/usr/bin\"\n\
         deeptool > /dev/null || { echo NO-DEEPTOOL >&2; exit 1; }\n\
         test ! -e /usr/include/deep.h || { echo HEADER-LEAKED >&2; exit 1; }\n\
         test ! -e /usr/lib/pkgconfig/deep.pc || { echo PC-LEAKED >&2; exit 1; }\n\
         echo ok > \"$DESTDIR/usr/bin/toptool\"\n",
    )
    .unwrap();
    let o = kiry(&["b", "--root", root.to_str().unwrap(), top.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// --root defaults to /, which is the running system. every command that writes refuses
// it outright rather than relying on the caller to always pass --root
#[test]
fn writing_to_slash_is_refused() {
    for args in [
        vec!["b", "/nonexistent"],
        vec!["i", "/nonexistent.tar.zst"],
        vec!["r", "nonexistent"],
        vec!["rebuild"],
    ] {
        let mut a = args.clone();
        a.extend(["--root", "/"]);
        let o = kiry(&a);
        assert!(!o.status.success(), "{args:?} was allowed");
        let said = String::from_utf8_lossy(&o.stderr);
        assert!(said.contains("refusing to write to /"), "{args:?}: {said}");
        assert!(said.contains("KIRY_ROOT_REALLY"), "{args:?}: {said}");
    }

    // with no --root at all it is the same root, so the guard has to catch that too
    let o = kiry(&["rebuild"]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("refusing to write to /"),
        "an unqualified rebuild was allowed"
    );

    // reads are not writes: doctor and l still answer for the running system
    for args in [vec!["l", "--root", "/"], vec!["doctor", "--root", "/"]] {
        let o = kiry(&args);
        assert!(
            !String::from_utf8_lossy(&o.stderr).contains("refusing"),
            "{args:?} was refused"
        );
    }
}

// abuild exports the triple and 1700 recipes read it. most only pass it to configure,
// which guesses right anyway, but LLVM_HOST_TRIPLE and clang/$CHOST.cfg take it as a
// name -- an unset one is a wrong answer that builds and installs
#[test]
fn a_build_is_told_which_triple_it_is() {
    let at = scratch("triple");
    let d = recipe(
        &at,
        "x86_64-musl",
        "mkdir -p \"$DESTDIR/usr/bin\"\necho \"$CBUILD $CHOST $CTARGET $CARCH\" > \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let art = cache(&root, ".tar.zst");
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "zstd -dc {}/var/kiry/cache/{} | tar -xOf - ./usr/bin/hello",
            root.display(),
            art[0]
        ))
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        said.trim(),
        "x86_64-unknown-linux-musl x86_64-unknown-linux-musl x86_64-unknown-linux-musl x86_64",
        "{said}"
    );
}

// the cache is one directory for every package, and plenty of urls end in download or
// v1.2.tar.gz. named, two of those are two files instead of whichever arrived first
#[test]
fn a_named_source_lands_under_its_name() {
    let at = scratch("named");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let arc = at.join("hello-1.0.tar");
    fs::write(
        d.join("sources"),
        "hello-1.0.tar::https://example/archive/refs/tags/v1.0\n",
    )
    .unwrap();

    let fetcher = at.join("fetch.sh");
    fs::write(
        &fetcher,
        format!(
            "#!/bin/sh\ntest \"$1\" = https://example/archive/refs/tags/v1.0\ncp {} \"$2\"\n",
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
    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_FETCH", format!("{} %u %o", fetcher.display()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(root.join("var/kiry/cache/sources/hello-1.0.tar").exists());
    assert!(!root.join("var/kiry/cache/sources/v1.0").exists());
}

// a name with a slash falls back to the basename and cannot reach out of the cache, but
// .. has no slash. it names the cache itself, and the build then fails hashing a
// directory, which says nothing about the line that caused it
#[test]
fn a_name_that_is_not_a_name_says_so() {
    let at = scratch("badname");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    for bad in ["..::https://example/x", "::https://example/x"] {
        fs::write(d.join("sources"), format!("{bad}\n")).unwrap();
        let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
        assert!(!o.status.success());
        let said = String::from_utf8_lossy(&o.stderr);
        assert!(said.contains("no file name in that"), "{bad}: {said}");
    }
    assert!(artifacts(&root).is_empty());
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

// a compiler links against whatever the sysroot holds. these recipes say the same thing
// with test and cp, which is all a busybox toolchain has, and linking against what is
// there is the whole of what rebuild depends on
fn lib(at: &Path, soname: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{soname}.c"));
    fs::write(&src, "void p(void){}\n").unwrap();
    let out = at.join(soname);
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap()
        .success());
    out
}

fn app(at: &Path, name: &str, against: &Path) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{name}.c"));
    fs::write(&src, "void p(void);\nvoid _start(void){p();}\n").unwrap();
    let out = at.join(name);
    assert!(Command::new("cc")
        .args(["-nostdlib", "-Wl,-rpath,$ORIGIN/../lib64"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(against)
        .status()
        .unwrap()
        .success());
    out
}

fn place(root: &Path, name: &str, files: &[(&str, &Path)]) {
    let mut manifest = Vec::new();
    for (path, from) in files {
        let dst = root.join(path);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&dst);
        fs::copy(from, &dst).unwrap();
        manifest.push(db::Entry {
            mode: 0o755,
            kind: db::Kind::File(kiry_core::sha256(fs::File::open(&dst).unwrap()).unwrap()),
            path: (*path).to_string(),
        });
    }
    db::write(
        root,
        &db::Installed {
            name: name.into(),
            target: "x86_64-gnu".into(),
            version: Version::parse("1.0 1").unwrap(),
            depends: Vec::new(),
            manifest: manifest.clone(),
        },
    )
    .unwrap();
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
    db::write_provides(root, "x86_64-gnu", name, &provides).unwrap();
}

// doctor names a path, rebuild has to get from there to a recipe and back to a working
// root without being told anything else
#[test]
fn rebuild_recompiles_what_the_break_names() {
    let at = scratch("rebuild");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    for d in ["installed", "provides"] {
        let _ = fs::remove_dir_all(root.join(format!("usr/lib/kiry/db/{d}/x86_64-musl")));
    }

    let one = lib(&at.join("v1"), "libp.so.1");
    let two = lib(&at.join("v2"), "libp.so.2");
    let src = at.join("src/app-1.0");
    fs::create_dir_all(&src).unwrap();
    fs::copy(app(&at.join("a1"), "app-1", &one), src.join("app-1")).unwrap();
    fs::copy(app(&at.join("a2"), "app-2", &two), src.join("app-2")).unwrap();

    let arc = at.join("app-1.0.tar");
    assert!(Command::new("tar")
        .arg("-cf")
        .arg(&arc)
        .arg("-C")
        .arg(at.join("src"))
        .arg("app-1.0")
        .status()
        .unwrap()
        .success());

    let repo = at.join("repo");
    let d = repo.join("app");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.0 1\n").unwrap();
    fs::write(d.join("targets"), "x86_64-gnu\n").unwrap();
    fs::write(d.join("sources"), "../../app-1.0.tar\n").unwrap();
    fs::write(
        d.join("checksums"),
        format!(
            "{}\n",
            kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap()
        ),
    )
    .unwrap();
    fs::write(d.join("depends"), "libp\n").unwrap();
    fs::write(
        d.join("build"),
        "mkdir -p \"$DESTDIR/usr/bin\"\n\
         if [ -e /usr/lib64/libp.so.2 ]; then cp app-2 \"$DESTDIR/usr/bin/app\"\n\
         else cp app-1 \"$DESTDIR/usr/bin/app\"; fi\n",
    )
    .unwrap();
    let bare = at.join("bare");
    let half = at.join("half");
    fs::create_dir_all(&bare).unwrap();
    fs::create_dir_all(half.join("app")).unwrap();
    fs::write(
        root.join("etc/kiry/repos"),
        format!(
            "# where to look\n\n{}\n{}\n{}\n",
            bare.display(),
            half.display(),
            repo.display()
        ),
    )
    .unwrap();

    let r = root.to_str().unwrap();
    place(&root, "libp", &[("usr/lib64/libp.so.1", &one)]);
    let o = kiry(&["b", "--root", r, d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let arc = root.join("var/kiry/cache/app-1.0-1.x86_64-gnu.tar.zst");
    let o = kiry(&["i", "--root", r, arc.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let first = kiry(&["doctor", "--root", r]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );

    // the soname moves under it, which is the break the whole engine exists for
    place(&root, "libp", &[("usr/lib64/libp.so.2", &two)]);
    let _ = fs::remove_file(root.join("usr/lib64/libp.so.1"));
    let out = String::from_utf8_lossy(&kiry(&["doctor", "--root", r]).stdout).into_owned();
    assert!(
        out.contains("usr/bin/app x86_64-gnu unresolved libp.so.1"),
        "{out}"
    );

    let o = kiry(&["rebuild", "--root", r]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("app 1.0 x86_64-gnu rebuilt"), "{said}");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let after = kiry(&["doctor", "--root", r]);
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stdout)
    );
}

// two broken consumers where alpha links beta, so the order the dependency asks for is
// the reverse of the order they are found in. that is the whole point: a package cannot
// compile against a dependency that has not been rebuilt and reinstalled yet
fn two_consumers(at: &Path, root: &Path, deps: &str) -> PathBuf {
    let one = lib(&at.join("v1"), "libp.so.1");
    let two = lib(&at.join("v2"), "libp.so.2");
    let arc = at.join("empty.tar");
    let src = at.join("src/empty-1.0");
    fs::create_dir_all(&src).unwrap();
    assert!(Command::new("tar")
        .arg("-cf")
        .arg(&arc)
        .arg("-C")
        .arg(at.join("src"))
        .arg("empty-1.0")
        .status()
        .unwrap()
        .success());
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();

    let repo = at.join("repo");
    for (name, dep) in [("alpha", "beta"), ("beta", deps)] {
        let d = repo.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("version"), "1.0 1\n").unwrap();
        fs::write(d.join("targets"), "x86_64-gnu\n").unwrap();
        fs::write(d.join("sources"), "../../empty.tar\n").unwrap();
        fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();
        fs::write(d.join("depends"), format!("{dep}\n")).unwrap();
        fs::write(d.join("build"), ":\n").unwrap();
    }
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();

    place(root, "libp", &[("usr/lib64/libp.so.1", &one)]);
    place(
        root,
        "alpha",
        &[("usr/bin/alpha", &app(&at.join("a"), "alpha", &one))],
    );
    place(
        root,
        "beta",
        &[("usr/bin/beta", &app(&at.join("b"), "beta", &one))],
    );
    // the soname moves and both consumers are left naming one that is gone
    place(root, "libp", &[("usr/lib64/libp.so.2", &two)]);
    let _ = fs::remove_file(root.join("usr/lib64/libp.so.1"));
    two
}

fn one_target_root(at: &Path) -> PathBuf {
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    assert!(bootstrap(&root));
    for d in ["installed", "provides"] {
        let _ = fs::remove_dir_all(root.join(format!("usr/lib/kiry/db/{d}/x86_64-musl")));
    }
    root
}

#[test]
fn a_rebuild_waits_for_what_it_links() {
    let at = scratch("order");
    if !bootstrap(&at.join("probe")) {
        return;
    }
    let root = one_target_root(&at);
    two_consumers(&at, &root, "libp");

    let o = kiry(&["rebuild", "--root", root.to_str().unwrap(), "-n"]);
    let said = String::from_utf8_lossy(&o.stdout);
    let a = said.find("alpha").unwrap_or(0);
    let b = said.find("beta").unwrap_or(usize::MAX);
    assert!(b < a, "alpha links beta and came first: {said}");
}

// a cycle cannot be resolved by ordering, and guessing at one is how a package manager
// hangs instead of saying what is wrong
#[test]
fn a_rebuild_cycle_says_who_is_in_it() {
    let at = scratch("cycle");
    if !bootstrap(&at.join("probe")) {
        return;
    }
    let root = one_target_root(&at);
    two_consumers(&at, &root, "alpha");

    let o = kiry(&["rebuild", "--root", root.to_str().unwrap(), "-n"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "a cycle went through");
    assert!(said.contains("alpha") && said.contains("beta"), "{said}");
}

fn lib_with(at: &Path, soname: &str, body: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{soname}.c"));
    fs::write(&src, body).unwrap();
    let out = at.join(soname);
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap()
        .success());
    out
}

fn app_calling(at: &Path, name: &str, sym: &str, against: &Path) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{name}.c"));
    fs::write(
        &src,
        format!("void {sym}(void);\nvoid _start(void){{{sym}();}}\n"),
    )
    .unwrap();
    let out = at.join(name);
    assert!(Command::new("cc")
        .args(["-nostdlib", "-Wl,-rpath,$ORIGIN/../lib64"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(against)
        .status()
        .unwrap()
        .success());
    out
}

fn archive(at: &Path, name: &str, files: &[(&str, &Path)]) -> PathBuf {
    let src = at.join(format!("{name}-stage"));
    let _ = fs::remove_dir_all(&src);
    for (path, from) in files {
        let dst = src.join(path);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(from, &dst).unwrap();
    }
    let arc = at.join(format!("{name}.tar.zst"));
    let _ = fs::remove_file(&arc);
    assert!(Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {} && tar cf - . | zstd -q -o {}",
            src.display(),
            arc.display()
        ))
        .status()
        .unwrap()
        .success());
    let meta = at.join(format!("{name}.tar.zst.meta"));
    let _ = fs::remove_dir_all(&meta);
    fs::create_dir_all(&meta).unwrap();
    fs::write(meta.join("name"), "foo\n").unwrap();
    fs::write(meta.join("version"), "1.0 1\n").unwrap();
    fs::write(meta.join("targets"), "x86_64-gnu\n").unwrap();
    fs::write(meta.join("depends"), "").unwrap();
    arc
}

fn queued(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("usr/lib/kiry/db/queue"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

// two consumers of one library, one calling p and one calling q. dropping q resolves
// fine for both, so nothing is broken yet and no check can see it. the queue is the
// only thing that knows, and it has to name the caller of q and not the other one
#[test]
fn only_the_consumer_that_used_what_left_is_queued() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-filter");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let both = lib_with(
        &at.join("v1"),
        "libp.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );
    let gone = lib_with(&at.join("v2"), "libp.so.1", "void p(void){}\n");

    let first = archive(&at, "foo-1", &[("usr/lib64/libp.so.1", &both)]);
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    place(
        &root,
        "usep",
        &[(
            "usr/bin/usep",
            &app_calling(&at.join("a"), "usep", "p", &both),
        )],
    );
    place(
        &root,
        "useq",
        &[(
            "usr/bin/useq",
            &app_calling(&at.join("b"), "useq", "q", &both),
        )],
    );

    let second = archive(&at, "foo-2", &[("usr/lib64/libp.so.1", &gone)]);
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| l.ends_with(" useq")),
        "useq calls q and is not queued: {q:?}"
    );
    assert!(
        !q.iter().any(|l| l.ends_with(" usep")),
        "usep never called q and was queued anyway: {q:?}"
    );
}

// a library that only gained a symbol guarantees everything it used to. the queue stays
// empty, which is the cutoff that keeps a patch bump from rebuilding the world
#[test]
fn a_library_that_only_grew_queues_nobody() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-cutoff");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let one = lib_with(&at.join("v1"), "libp.so.1", "void p(void){}\n");
    let two = lib_with(
        &at.join("v2"),
        "libp.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );

    let first = archive(&at, "foo-1", &[("usr/lib64/libp.so.1", &one)]);
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    place(
        &root,
        "usep",
        &[(
            "usr/bin/usep",
            &app_calling(&at.join("a"), "usep", "p", &one),
        )],
    );

    let second = archive(&at, "foo-2", &[("usr/lib64/libp.so.1", &two)]);
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(queued(&root).is_empty(), "{:?}", queued(&root));
}

// a shared library, not an executable. linking a data symbol into an executable gets a
// copy relocation and the symbol comes out defined there, which is a fixture artefact
// and not how a real consumer of one looks
fn touching(at: &Path, name: &str, against: &Path) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{name}.c"));
    fs::write(&src, "extern char t[];\nvoid u(void){t[0]=1;}\n").unwrap();
    let out = at.join(name);
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg(format!("-Wl,-soname,{name}"))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(against)
        .status()
        .unwrap()
        .success());
    out
}

// an object growing leaves every name in place, so a binary rebuilt against the new one
// still names it. the package that ships the library was rebuilt with it and is done;
// queueing it would put it straight back in line to rebuild itself forever. a consumer
// in another package is the one that still has to catch up
#[test]
fn a_package_does_not_queue_itself_for_its_own_library() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-self");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let small = lib_with(&at.join("v1"), "libp.so.1", "char t[8];\nvoid p(void){}\n");
    let big = lib_with(&at.join("v2"), "libp.so.1", "char t[16];\nvoid p(void){}\n");

    let first = archive(
        &at,
        "foo-1",
        &[
            ("usr/lib64/libp.so.1", &small),
            (
                "usr/lib64/libtool.so.1",
                &touching(&at.join("t1"), "libtool.so.1", &small),
            ),
        ],
    );
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    place(
        &root,
        "other",
        &[(
            "usr/lib64/libother.so.1",
            &touching(&at.join("o1"), "libother.so.1", &small),
        )],
    );

    let second = archive(
        &at,
        "foo-2",
        &[
            ("usr/lib64/libp.so.1", &big),
            (
                "usr/lib64/libtool.so.1",
                &touching(&at.join("t2"), "libtool.so.1", &big),
            ),
        ],
    );
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| l.ends_with(" other")),
        "other still links the old layout and is not queued: {q:?}"
    );
    assert!(
        !q.iter().any(|l| l.ends_with(" foo")),
        "foo ships the library and was rebuilt with it: {q:?}"
    );
}
