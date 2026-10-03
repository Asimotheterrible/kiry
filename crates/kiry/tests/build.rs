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
fn interp(p: &Path) -> Option<String> {
    let b = fs::read(p).ok()?;
    let word = |at: usize| -> usize {
        let mut v = 0usize;
        for i in (0..8).rev() {
            v = (v << 8) | *b.get(at + i).unwrap_or(&0) as usize;
        }
        v
    };
    let half = |at: usize| -> usize {
        (*b.get(at + 1).unwrap_or(&0) as usize) << 8 | *b.get(at).unwrap_or(&0) as usize
    };
    let (off, size, num) = (word(0x20), half(0x36), half(0x38));
    for i in 0..num {
        let h = off + i * size;
        if half(h) != 3 {
            continue;
        }
        let (at, len) = (word(h + 0x08), word(h + 0x20));
        let s = b.get(at..at + len)?;
        return String::from_utf8(s.split(|c| *c == 0).next()?.to_vec()).ok();
    }
    None
}

fn bootstrap(root: &Path) -> bool {
    let bb = PathBuf::from("/usr/bin/busybox");
    if !bb.is_file() {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "no busybox to seed the sandbox toolchain with"
        );
        return false;
    }

    // not ldd. it is glibc's on a root with the gnu tier installed and it answers
    // "not found" for every musl binary, so the sandbox came out with no libc in it
    let o = kiry_core::elf::read(&bb).unwrap();
    let mut want: Vec<String> = o.needed.clone();
    if let Some(i) = interp(&bb) {
        want.push(i);
    }

    let mut files: Vec<(String, PathBuf)> = vec![("usr/bin/busybox".into(), bb.clone())];
    for n in want {
        // the loader arrives by absolute path and a DT_NEEDED by name, and on musl they
        // are the same file twice -- placed under both, because PT_INTERP is the kernel's
        // lookup and neither one covers the other
        let at = match n.strip_prefix('/') {
            Some(a) => a.to_string(),
            None => ["usr/lib", "usr/lib64", "lib", "lib64"]
                .iter()
                .map(|d| format!("{d}/{n}"))
                .find(|a| Path::new("/").join(a).is_file())
                .unwrap_or_else(|| panic!("nothing provides {n}, needed by busybox")),
        };
        files.push((at.clone(), PathBuf::from("/").join(&at)));
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
    // busybox picks its applet out of argv[0], so these are the tools. named one by
    // one rather than left to the standalone shell, which reads /proc/self/exe and so
    // depends on what the sandbox mounted
    let applets = Command::new("busybox").arg("--list").output().unwrap();
    let applets: Vec<&str> = std::str::from_utf8(&applets.stdout)
        .unwrap()
        .lines()
        .collect();
    for a in [
        "sh", "mkdir", "cp", "ln", "rm", "mv", "cat", "echo", "printf", "chmod", "find",
        "head", "install", "patch", "tar", "dd", "true", "false", "sed", "touch", "grep",
        "sort", "wc", "xargs", "nproc", "tr", "sleep",
    ] {
        let at = format!("usr/bin/{a}");
        // this busybox is built without patch and the closure is the whole of what a
        // build can see, so a symlink to an applet that is not there is a tool missing
        if !applets.contains(&a) {
            let real = PathBuf::from(format!("/usr/bin/{a}"));
            assert!(real.is_file(), "busybox has no {a} applet and nothing else does");
            fs::copy(&real, root.join(&at)).unwrap();
            manifest.push(db::Entry {
                mode: 0o755,
                kind: db::Kind::File(
                    kiry_core::sha256(fs::File::open(root.join(&at)).unwrap()).unwrap(),
                ),
                path: at,
            });
            continue;
        }
        std::os::unix::fs::symlink("busybox", root.join(&at)).unwrap();
        manifest.push(db::Entry {
            mode: 0o777,
            kind: db::Kind::Link("busybox".into()),
            path: at,
        });
    }

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
    // musl only. these are musl binaries and /usr/lib is not on the gnu search path, so
    // a gnu record for them is a claim doctor is right to reject
    for target in ["x86_64-musl"] {
        db::write(
            root,
            &db::Installed {
                name: "busybox".into(),
                target: target.into(),
                version: Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: manifest.clone(),
                hash: String::new(),
                users: Vec::new(),
                flags: Vec::new(),
            },
        )
        .unwrap();
        db::write_provides(root, target, "busybox", &provides).unwrap();
    }

    // every closure gets the target's libc without a recipe naming it, and an empty
    // record is enough here -- the libc file came along with busybox above
    for (t, n) in [
        ("x86_64-musl", "musl"),
        ("x86_64-gnu", "glibc"),
        ("x86_64-gnu", "gcc-runtime"),
        // a make dep, so it comes from KIRY_HOST and not the target. the gnu fixtures
        // set that to x86_64-gnu, so both spellings have to be here
        ("x86_64-musl", "gcc-stage1"),
        ("x86_64-gnu", "gcc-stage1"),
    ] {
        db::write(
            root,
            &db::Installed {
                name: n.into(),
                target: t.into(),
                version: Version::parse("1.0 1").unwrap(),
                depends: Vec::new(),
                manifest: Vec::new(),
                hash: String::new(),
                users: Vec::new(),
                flags: Vec::new(),
            },
        )
        .unwrap();
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

#[test]
fn an_openrc_service_never_reaches_the_artifact() {
    let at = scratch("openrc");
    let script = format!(
        "{GOOD}mkdir -p \"$DESTDIR/etc/init.d\" \"$DESTDIR/etc/conf.d\"\n\
         printf '#!/sbin/openrc-run\\n' > \"$DESTDIR/etc/init.d/hello\"\n\
         echo x > \"$DESTDIR/etc/conf.d/hello\"\n"
    );
    let d = recipe(&at, "x86_64-musl", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("dropped etc/init.d etc/conf.d"));

    let arc = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst");
    let o = Command::new("sh")
        .arg("-c")
        .arg(format!("zstd -dc {} | tar tf -", arc.display()))
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&o.stdout);
    assert!(listed.contains("usr/bin/hello"), "{listed}");
    assert!(!listed.contains("etc/init.d") && !listed.contains("etc/conf.d"), "{listed}");
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
    // the rule is "no kiry owns /", so on a machine that is itself a kiry root the
    // right answer is that / goes through. asserted both ways rather than skipped,
    // because it is the same rule either way
    let owned = Path::new("/usr/lib/kiry/db/installed").is_dir();

    // none of these three reach a write: they die on input that is not there. rebuild
    // is deliberately not among them, since on an owned / it would really start building
    for args in [
        vec!["b", "/nonexistent"],
        vec!["i", "/nonexistent.tar.zst"],
        vec!["r", "nonexistent"],
    ] {
        let mut a = args.clone();
        a.extend(["--root", "/"]);
        let o = kiry(&a);
        assert!(!o.status.success(), "{args:?} was allowed");
        let said = String::from_utf8_lossy(&o.stderr);
        assert_eq!(
            said.contains("refusing to write to /"),
            !owned,
            "{args:?}: {said}"
        );
        if !owned {
            assert!(said.contains("KIRY_ROOT_REALLY"), "{args:?}: {said}");
        }
    }

    if !owned {
        // with no --root at all it is the same root, so the guard has to catch that too
        let o = kiry(&["rebuild"]);
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("refusing to write to /"),
            "an unqualified rebuild was allowed"
        );
    }

    // reads are not writes: doctor and l still answer for the running system
    for args in [vec!["l", "--root", "/"], vec!["doctor", "--root", "/"]] {
        let o = kiry(&args);
        assert!(
            !String::from_utf8_lossy(&o.stderr).contains("refusing"),
            "{args:?} was refused"
        );
    }
}

// default_prepare decides what is a patch from the name a source line gives it, not from
// the url. named readline83-001.patch::.../readline83-001 the old rule saw no .patch at
// the end and skipped it, and looked for it under the url's tail besides
#[test]
fn a_patch_named_through_a_url_still_gets_applied() {
    let at = scratch("patchname");
    let d = recipe(&at, "x86_64-musl", GOOD);
    // the build fails unless the patch landed, so the assert is the exit status
    // shaped like a converted recipe, because default_prepare is what is under test
    fs::write(
        d.join("build"),
        ". /usr/share/kiry/lib.sh\n\
         srcdir=/src\n\
         source=\"../hello-1.0.tar fixup.patch::https://example/raw/fixup\"\n\
         default_prepare\n\
         test -f applied || { echo PATCH-NOT-APPLIED >&2; exit 1; }\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp applied \"$DESTDIR/usr/bin/hello\"\n",
    )
    .unwrap();
    let patch = at.join("thepatch");
    fs::write(
        &patch,
        "--- /dev/null\n+++ b/applied\n@@ -0,0 +1 @@\n+yes\n",
    )
    .unwrap();

    let arc = at.join("hello-1.0.tar");
    fs::write(
        d.join("sources"),
        "../hello-1.0.tar\nfixup.patch::https://example/raw/fixup\n",
    )
    .unwrap();
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();
    let psum = kiry_core::sha256(fs::File::open(&patch).unwrap()).unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n{psum}\n")).unwrap();

    let fetcher = at.join("fetch.sh");
    fs::write(
        &fetcher,
        format!("#!/bin/sh\ncp {} \"$2\"\n", patch.display()),
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
    // the patch has to be in /src under the name the line gave it
    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_FETCH", format!("{} %u %o", fetcher.display()))
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{said}");
    assert!(
        !said.contains("PATCH-NOT-APPLIED"),
        "the patch was never applied: {said}"
    );
    assert!(root.join("var/kiry/cache/sources/fixup.patch").exists());
}

// a build that allocates without bound takes the machine down rather than itself, and
// the process the oom killer picks is whatever else was running. binutils' configure
// probe for ada did exactly that
#[test]
fn a_build_cannot_allocate_past_the_cap() {
    let at = scratch("memcap");
    // the allocation succeeding is what fails the build, so the assert is the exit status
    let d = recipe(
        &at,
        "x86_64-musl",
        "mkdir -p \"$DESTDIR/usr/bin\"\n\
         if dd if=/dev/zero of=/dev/null bs=256M count=1 2>/dev/null; then\n\
         \techo CAP-DID-NOT-BITE >&2; exit 1\n\
         fi\n\
         cp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_MEM", "64")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(!said.contains("CAP-DID-NOT-BITE"), "{said}");
    assert!(o.status.success(), "{said}");
}

// on the target system kiry runs as root, where tar restores the uids the archive
// carries. the sandbox maps one id, so anything owned by another one cannot be chowned
// inside it and patch fails setting a group it cannot name
#[test]
fn a_source_tree_is_owned_by_whoever_unpacks_it() {
    let at = scratch("owner");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    // a real extraction and not --help, which busybox exits 1 from whether or not it
    // understood the flag. this is the same tar and the same argument order kiry uses
    let into = at.join("probe");
    fs::create_dir_all(&into).unwrap();
    let out = Command::new("tar")
        .arg("--no-same-owner")
        .arg("-xf")
        .arg(at.join("hello-1.0.tar"))
        .arg("-C")
        .arg(&into)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tar rejects --no-same-owner: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(into.join("hello-1.0/greeting").is_file());
}

// build is this machine and host is what the output runs on. a gnu package built on a
// musl machine is the one place they differ, and told they matched, gcc's configure read
// musl's headers as glibc's and compiled a call to mallinfo2 that is not there
#[test]
fn a_cross_build_is_told_the_two_triples_apart() {
    let at = scratch("crosstriple");
    let d = recipe(
        &at,
        "x86_64-gnu",
        // usr/lib64 and not usr/bin: the gnu tier is a library layer and trim() drops
        // every program out of it, so a probe left in usr/bin never reaches the archive
        "mkdir -p \"$DESTDIR/usr/lib64\"\necho \"$CBUILD $CHOST\" > \"$DESTDIR/usr/lib64/hello\"\n",
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
            "zstd -dc {}/var/kiry/cache/{} | tar -xOf - ./usr/lib64/hello",
            root.display(),
            art[0]
        ))
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        said.trim(),
        "x86_64-unknown-linux-musl x86_64-unknown-linux-gnu",
        "{said}"
    );
}

// cmake's GNUInstallDirs defaults to lib64 on any 64-bit linux it does not recognise,
// and it recognises alpine by a file this tree deliberately does not ship. a musl
// package landing in usr/lib64 is in the gnu tier's directory
#[test]
fn a_cmake_build_is_told_which_libdir_the_target_uses() {
    for (target, want) in [("x86_64-musl", "lib"), ("x86_64-gnu", "lib64")] {
        let at = scratch(&format!("cmakelibdir-{target}"));
        let d = recipe(
            &at,
            target,
            // the libdir it is being told about is also the one place trim() keeps on
            // both targets, so the probe lands where it can be read back
            "mkdir -p \"$DESTDIR$KIRY_LIBDIR\"\ncat \"$CMAKE_TOOLCHAIN_FILE\" > \"$DESTDIR$KIRY_LIBDIR/hello\"\n",
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
                "zstd -dc {}/var/kiry/cache/{} | tar -xOf - ./usr/{want}/hello",
                root.display(),
                art[0]
            ))
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            said.contains(&format!(
                "set(CMAKE_INSTALL_LIBDIR \"{want}\" CACHE STRING \"\" FORCE)"
            )),
            "{target} got {said}"
        );
        // GNUInstallDirs runs set_property(CACHE) over every one of these, so a plain
        // set is an error rather than an override -- libjpeg-turbo bundles a fork of it
        for var in ["LIBDIR", "INCLUDEDIR", "DATAROOTDIR"] {
            let line = said
                .lines()
                .find(|l| l.starts_with(&format!("set(CMAKE_INSTALL_{var} ")))
                .unwrap_or_else(|| panic!("{target} never set {var}: {said}"));
            assert!(
                line.contains("CACHE STRING") && line.contains("FORCE"),
                "{target} does not settle {var} itself: {line}"
            );
        }
        // and the half that keeps a recipe's own untyped -D relative. without it cmake
        // resolves lib against the build dir and the package installs into its own tree
        assert!(
            said.contains("set_property(CACHE ${_d} PROPERTY TYPE STRING)"),
            "{target} never settles the type of a -D it was handed: {said}"
        );
        // cmake gives a shared library -fPIC and the object library beside it nothing,
        // which is one -fPIE translation unit in a thinlto -shared link. this gives a
        // library and an object library -fPIC and an executable -fPIE, which is what
        // -fPIC in the global CFLAGS would not have done
        assert!(
            said.contains("set(CMAKE_POSITION_INDEPENDENT_CODE ON CACHE BOOL \"\" FORCE)"),
            "{target} builds an object library without pic: {said}"
        );
        // nothing runs the suite, so compiling it is the whole of what it costs
        assert!(
            said.contains("set(BUILD_TESTING OFF CACHE BOOL \"\" FORCE)"),
            "{target} builds its tests: {said}"
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
                    host: false,
                    only: None,
                })
                .collect(),
            manifest,
            hash: String::new(),
            users: Vec::new(),
            flags: Vec::new(),
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
                hash: String::new(),
                users: Vec::new(),
                flags: Vec::new(),
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
            hash: String::new(),
            users: Vec::new(),
            flags: Vec::new(),
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
        // usr/lib64 and not usr/bin: the gnu tier is a library layer, so trim() drops
        // every program out of a gnu artifact and a consumer left in usr/bin is never
        // installed to be found broken
        "mkdir -p \"$DESTDIR/usr/lib64\"\n\
         if [ -e /usr/lib64/libp.so.2 ]; then cp app-2 \"$DESTDIR/usr/lib64/app\"\n\
         else cp app-1 \"$DESTDIR/usr/lib64/app\"; fi\n",
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
    assert!(kiry(&["doctor", "--root", r]).status.success());

    // the soname moves under it, which is the break the whole engine exists for
    place(&root, "libp", &[("usr/lib64/libp.so.2", &two)]);
    let _ = fs::remove_file(root.join("usr/lib64/libp.so.1"));
    let out = String::from_utf8_lossy(&kiry(&["doctor", "--root", r]).stdout)
        .into_owned();
    assert!(
        out.contains("usr/lib64/app x86_64-gnu unresolved libp.so.1"),
        "{out}"
    );

    let o = kiry(&["rebuild", "--root", r]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("app 1.0 x86_64-gnu rebuilt"),
        "out: {said}\nerr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!said.contains("same bytes"), "app-2 replaced app-1 and was called the same: {said}");
    let log = fs::read_to_string(root.join("var/kiry/log/rebuilds")).unwrap();
    assert!(log.starts_with("app x86_64-gnu doctor changed "), "{log}");

    assert!(kiry(&["doctor", "--root", r]).status.success());
}

// run under a pty with a size, which is what a terminal is. busybox script, and rows as
// well as columns: a pty with no rows reads as no size at all
fn on_tty(cols: u32, cmd: &str) -> String {
    let o = Command::new("script")
        .args(["-qc", &format!("stty rows 40 cols {cols}; {cmd}"), "/dev/null"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).replace('\r', "")
}

// the art is for a person at a terminal wide enough to hold it beside the numbers, and
// nothing that reads kiry's output ever sees it
#[test]
fn stats_puts_the_art_beside_itself_on_a_wide_terminal_only() {
    let at = scratch("art");
    let root = at.join("root");
    fs::create_dir_all(root.join("usr/share/kiry/art")).unwrap();
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("usr/share/kiry/art/idle"), "(=^.^=)\n").unwrap();
    fs::write(root.join("usr/share/kiry/quotes"), "# one per line\npresent day, present time\n").unwrap();
    let cmd = format!("{KIRY} stats --root {}", root.display());

    let wide = on_tty(120, &cmd);
    assert!(wide.lines().any(|l| l.starts_with("(=^.^=)\x1b[0m  ")), "{wide}");
    assert!(wide.contains("\"present day, present time\""), "{wide}");
    let narrow = on_tty(60, &cmd);
    assert!(!narrow.contains("(=^.^=)") && !narrow.contains("present day"), "{narrow}");
    // piped from a terminal: /dev/tty is still there to ask for a width
    let piped = on_tty(120, &format!("{cmd} | cat"));
    assert!(!piped.contains("(=^.^=)") && !piped.contains('\x1b'), "{piped}");

    fs::write(root.join("etc/kiry/config"), "KIRY_ART off\nKIRY_QUOTES off\n").unwrap();
    let off = on_tty(120, &cmd);
    assert!(!off.contains("(=^.^=)") && !off.contains("present day"), "{off}");
}

// the title says which package a batch is on and goes back to what it was after. the
// bell is the byte after the title comes back, since the title ends in one too
#[test]
fn a_batch_names_its_package_in_the_title_and_rings_when_it_fails() {
    let at = scratch("title");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let good = recipe(&at.join("good"), "x86_64-musl", GOOD);
    let r = root.display();
    let ok = on_tty(120, &format!("{KIRY} b --root {r} {}", good.display()));
    assert!(ok.contains("\x1b[22;0t") && ok.contains("\x1b]2;kiry 1/1 hello\x07"), "{ok:?}");
    let (_, after) = ok.rsplit_once("\x1b[23;0t").unwrap();
    assert!(!after.contains('\x07'), "a short clean batch rang: {ok:?}");

    let bad = recipe(&at.join("bad"), "x86_64-musl", "exit 1\n");
    let failed = on_tty(120, &format!("{KIRY} b --root {r} {}", bad.display()));
    let (_, after) = failed.rsplit_once("\x1b[23;0t").unwrap();
    assert!(after.contains('\x07'), "a failed batch did not ring: {failed:?}");

    let piped = kiry(&["b", "--root", root.to_str().unwrap(), good.to_str().unwrap()]);
    assert!(!String::from_utf8_lossy(&piped.stdout).contains('\x1b'));
}

// a pipe is read by something, so it gets a line per state change, single spaced, not
// one escape byte. a terminal gets colour, and one line rewritten in
// place for the build that is running
#[test]
fn a_pipe_gets_plain_lines_and_a_terminal_gets_colour_and_a_live_line() {
    let at = scratch("colour");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let good = recipe(&at, "x86_64-musl", GOOD);
    let r = root.display();

    let piped = kiry(&["b", "--root", root.to_str().unwrap(), good.to_str().unwrap()]);
    assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));
    let out = String::from_utf8_lossy(&piped.stdout);
    let all = format!("{out}{}", String::from_utf8_lossy(&piped.stderr));
    assert!(!all.contains('\x1b') && !all.contains('\r'), "{all:?}");
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("hello 1.0 x86_64-musl building -"), "{out}");
    assert!(lines.any(|l| l.starts_with("hello 1.0 x86_64-musl ok ")), "{out}");

    let cmd = format!("env -u NO_COLOR {KIRY} b --root {r} {}", good.display());
    let tty = on_tty(120, &cmd);
    assert!(tty.contains("\x1b[32mok"), "{tty:?}");
    assert!(tty.contains("\x1b[36mx86_64-musl"), "{tty:?}");
    assert!(tty.contains("\x1b[K[1/1] hello 1.0 x86_64-musl  building  "), "{tty:?}");
    // the second time round there is a time to go by
    assert!(tty.contains(" / ~"), "{tty:?}");
    assert!(!tty.contains("building -"), "{tty:?}");

    // any value at all, empty included
    let plain = on_tty(120, &format!("NO_COLOR= {KIRY} b --root {r} {}", good.display()));
    assert!(plain.contains("x86_64-musl  ok"), "{plain:?}");
    assert!(!plain.contains("\x1b[0m") && !plain.contains("\x1b[32m"), "{plain:?}");
}

// an upgrade says which component moved: 1.0 installed and 1.1 built is the last digit
// bright and the rest dim. a package nothing installed yet is dim all through, and the
// colour is the only difference between that and a pipe
#[test]
fn the_component_an_upgrade_moved_is_bright() {
    let Some((at, root, repo)) = workshop("upgrade-colour") else {
        return;
    };
    buildable(&at, &repo, "tool", "");
    buildable(&at, &repo, "other", "");
    let r = root.to_str().unwrap();
    let o = kiry(&["i", "--root", r, "tool"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    fs::write(repo.join("tool/version"), "1.1 1\n").unwrap();

    let tty = on_tty(120, &format!("env -u NO_COLOR {KIRY} b --root {r} tool"));
    let ok = tty.lines().find(|l| l.contains("\x1b[32mok")).unwrap_or_else(|| panic!("{tty:?}"));
    assert!(ok.contains("\x1b[2m1.\x1b[0m\x1b[1m1\x1b[0m"), "{ok:?}");

    let tty = on_tty(120, &format!("env -u NO_COLOR {KIRY} b --root {r} other"));
    let ok = tty.lines().find(|l| l.contains("\x1b[32mok")).unwrap_or_else(|| panic!("{tty:?}"));
    assert!(ok.contains("\x1b[2m1.0\x1b[0m") && !ok.contains("\x1b[1m"), "{ok:?}");

    // built again under the same key, the repro note follows the row it belongs to
    let again = on_tty(120, &format!("env -u NO_COLOR {KIRY} b --root {r} tool"));
    let ok = again.lines().find(|l| l.contains("\x1b[32mok")).unwrap_or_else(|| panic!("{again:?}"));
    assert!(ok.contains("\x1b[1m1\x1b[0m") && ok.ends_with("  same bytes"), "{ok:?}");

    // NO_COLOR at a terminal keeps the columns and loses only the brightness
    let plain = on_tty(120, &format!("NO_COLOR= {KIRY} b --root {r} tool"));
    assert!(plain.contains("tool  1.1  x86_64-musl  ok"), "{plain:?}");
    assert!(!plain.contains("\x1b[1m") && !plain.contains("\x1b[2m"), "{plain:?}");

    let o = kiry(&["b", "--root", r, "tool"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.lines().any(|l| l.starts_with("tool 1.1 x86_64-musl ok ")), "{out}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

// a package the cache answers prints the same columns and colours as one that built, an
// install that builds nothing included, and a pipe still reads name version target cached
#[test]
fn a_cached_line_has_the_columns_and_colours_of_a_built_one() {
    let Some((at, root, repo)) = workshop("cached-colour") else {
        return;
    };
    buildable(&at, &repo, "a", "");
    buildable(&at, &repo, "longername", "");
    buildable(&at, &repo, "c", "");
    buildable(&at, &repo, "top", "c\n");
    let r = root.to_str().unwrap();
    let o = kiry(&["b", "--root", r, "a", "longername", "c"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let plain = |l: &str| {
        let mut esc = false;
        l.chars()
            .filter(|&c| {
                let keep = !esc && c != '\x1b';
                esc = if esc { !c.is_ascii_alphabetic() } else { c == '\x1b' };
                keep
            })
            .collect::<String>()
    };

    // every member cached, so no batch starts and the widths come from the set alone
    let tty = on_tty(120, &format!("env -u NO_COLOR {KIRY} i --root {r} a longername"));
    let rows: Vec<&str> = tty.lines().filter(|l| l.contains("cached")).collect();
    assert_eq!(rows.len(), 2, "{tty:?}");
    for l in &rows {
        assert!(l.contains("\x1b[97m") && l.contains("\x1b[36mx86_64-musl"), "{l:?}");
    }
    let at_target: Vec<Option<usize>> = rows.iter().map(|l| plain(l).find("x86_64-musl")).collect();
    assert_eq!(at_target[0], at_target[1], "{tty:?}");

    // a cached member of a batch that still builds something
    let tty = on_tty(120, &format!("env -u NO_COLOR {KIRY} i --root {r} top"));
    let row = tty.lines().find(|l| l.contains("cached")).unwrap_or_else(|| panic!("{tty:?}"));
    assert!(row.contains("\x1b[97mc") && row.contains("\x1b[36mx86_64-musl"), "{row:?}");

    let o = kiry(&["r", "--root", r, "a"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = kiry(&["i", "--root", r, "a"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.lines().any(|l| l == "a 1.0 x86_64-musl cached"), "{out}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

// the time that goes yellow is the one the batch spent itself on. a build on its own has
// nothing to be slow next to, however long it took
#[test]
fn the_slowest_tenth_of_a_batch_is_yellow() {
    let Some((at, root, repo)) = workshop("slow-decile") else {
        return;
    };
    buildable(&at, &repo, "fast", "");
    buildable(&at, &repo, "slow", "");
    let b = repo.join("slow/build");
    fs::write(&b, format!("sleep 2\n{}", fs::read_to_string(&b).unwrap())).unwrap();
    let r = root.to_str().unwrap();

    let tty = on_tty(120, &format!("env -u NO_COLOR {KIRY} b --root {r} fast slow"));
    let ok = |name: &str| {
        tty.lines()
            .find(|l| l.contains(name) && l.contains("\x1b[32mok"))
            .unwrap_or_else(|| panic!("{tty:?}"))
            .to_string()
    };
    // the time is the only yellow a result line has
    assert!(ok("slow").contains("\x1b[33m"), "{tty:?}");
    assert!(!ok("fast").contains("\x1b[33m"), "{tty:?}");

    let alone = on_tty(120, &format!("env -u NO_COLOR {KIRY} b --root {r} slow"));
    let row = alone.lines().find(|l| l.contains("\x1b[32mok")).unwrap_or_else(|| panic!("{alone:?}"));
    assert!(!row.contains("\x1b[33m"), "{alone:?}");

    // a pipe has the same time in words, which is all the yellow was saying
    let o = kiry(&["b", "--root", r, "fast"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.lines().any(|l| l.starts_with("fast 1.0 x86_64-musl ok ") && l.ends_with('s')), "{out}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

// -q is for a script or a cron job that only wants to hear about trouble. the art is a
// person's, and a person who asked for quiet does not get it either
#[test]
fn dash_q_says_only_what_failed() {
    let at = scratch("quiet");
    let root = at.join("root");
    fs::create_dir_all(root.join("usr/share/kiry/art")).unwrap();
    if !bootstrap(&root) {
        return;
    }
    fs::write(root.join("usr/share/kiry/art/ok"), "(=^.^=)\n").unwrap();
    let r = root.to_str().unwrap();
    let good = recipe(&at.join("good"), "x86_64-musl", GOOD);
    let bad = recipe(&at.join("bad"), "x86_64-musl", "echo 'x.c:1:1: error: no' >&2\nexit 1\n");

    let o = kiry(&["b", "-q", "--root", r, good.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "");
    let tty = on_tty(120, &format!("{KIRY} b -q --root {r} {}", good.display()));
    assert!(!tty.contains("(=^.^=)") && !tty.contains(" ok"), "{tty:?}");
    let loud = on_tty(120, &format!("{KIRY} b --root {r} {}", good.display()));
    assert!(loud.contains("(=^.^=)"), "the art is not there to be hidden: {loud:?}");

    let o = kiry(&["b", "-q", "--root", r, bad.to_str().unwrap()]);
    assert!(!o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.starts_with("hello 1.0 x86_64-musl failed "), "{out}");
    assert!(out.contains("  x.c:1:1: error: no"), "{out}");
    assert!(!out.contains("building"), "{out}");
    // the log is written at every level
    let log = fs::read_to_string(root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log")).unwrap();
    assert!(log.contains("error: no"), "{log}");

    for args in [&["i", "-q", "-n", "--root", r, good.to_str().unwrap()][..], &["rebuild", "-q", "-n", "--root", r]] {
        let o = kiry(args);
        assert!(!String::from_utf8_lossy(&o.stderr).contains("-q"), "{args:?}: {o:?}");
    }
}

// the line under a failure is the first thing that complained, with the colour a
// compiler put on it taken off. make's sign-off only says that something above it failed
const WRECK: &str = "printf 'src/a.c:1:1: \\033[1;31merror:\\033[0m use of undeclared identifier bar\\n' >&2\n\
    echo '1 error generated.' >&2\n\
    echo 'make: *** [Makefile:3: all] Error 1' >&2\n";

#[test]
fn a_failure_says_where_and_quotes_the_first_real_error() {
    let at = scratch("first-error");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let d = recipe(&at, "x86_64-musl", &format!("{WRECK}exit 2\n"));

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    let block: Vec<&str> = out.lines().skip_while(|l| !l.contains(" failed ")).collect();
    let log = format!("  log    {}", root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log").display());
    let first = "src/a.c:1:1: error: use of undeclared identifier bar";
    assert_eq!(
        block[1..],
        ["  phase  compile", "  rule   none matched", log.as_str(), &format!("  {first}")],
        "{out}"
    );

    // cut to the terminal rather than wrapped under the next line
    let tty = on_tty(40, &format!("{KIRY} b --root {} {}", root.display(), d.display()));
    let cut = format!("  {}", &first[..38]);
    assert!(tty.lines().any(|l| l == cut), "{tty:?}");
}

// the ladder's note is read months later with the log long gone, so it gets the same
// line and not make's
#[test]
fn a_rung_note_quotes_the_first_real_error() {
    let at = scratch("first-error-rung");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!("case \"$CFLAGS\" in\n*-O3*) {{\n{WRECK}}}; exit 2 ;;\nesac\n{GOOD}"),
    );
    config(&root, None, "OPT -O3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let pkg = fs::read_to_string(root.join("etc/kiry/pkg/hello")).unwrap();
    assert!(pkg.contains("#   src/a.c:1:1: error: use of undeclared identifier bar"), "{pkg}");
    assert!(!pkg.contains("Error 1"), "{pkg}");
}

// a recovered failure reads as one block: the row, where it died, the rule and what it
// does, and the retry with what it wrote where. then the retry's own row. nothing else
// in between, the build's own error line included, which says what the block already did
#[test]
fn a_recovered_failure_is_one_block_ending_in_its_retry() {
    let at = scratch("retry-block");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "case \"$CFLAGS\" in\n\
             *-flto*) echo 'ld.lld: error: Not a valid object file' >&2; exit 1 ;;\n\
             esac\n{GOOD}"
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let out = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    let i = lines.iter().position(|l| l.starts_with("hello 1.0 x86_64-musl failed ")).expect(&out);
    let log = format!("  log    {}", root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log").display());
    let retry = format!("  retry  1/3  filter-lto in {}", d.join("filter").display());
    assert_eq!(
        lines[i + 1..i + 6],
        [
            "  phase  link",
            "  rule   (Not a valid object file|invalid bitcode) -> filter-lto",
            log.as_str(),
            "  ld.lld: error: Not a valid object file",
            retry.as_str(),
        ],
        "{out}"
    );
    let ok = lines.iter().position(|l| l.starts_with("hello 1.0 x86_64-musl ok ")).expect(&out);
    assert!(ok > i + 5, "{out}");
    assert!(!out.contains("build failed"), "{out}");
}

// a rung is a retry too, and says which rung of how many and what it wrote where
#[test]
fn each_ladder_rung_says_it_is_retrying() {
    let at = scratch("retry-rung");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!("case \"$CFLAGS\" in\n*-O3*) echo 'foo.c:3:9: error: displeased' >&2; exit 1 ;;\nesac\n{GOOD}"),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let out = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    let pkg = root.join("etc/kiry/pkg/hello");
    let rungs = [
        format!("  retry  rung 1/4  filter-lto in {}", d.join("filter").display()),
        format!("  retry  rung 2/4  OPT -O2 in {}", pkg.display()),
    ];
    for r in &rungs {
        let at = lines.iter().position(|l| l == r).unwrap_or_else(|| panic!("no {r:?} in {out}"));
        assert_eq!(lines[at - 1], "  foo.c:3:9: error: displeased", "{out}");
    }
    assert!(!out.contains("build failed"), "{out}");
}

// curl redraws its meter with \r on stderr, which in a pipe is junk in someone's log
#[test]
fn a_fetch_in_a_pipe_draws_no_meter() {
    let at = scratch("meter");
    let d = recipe(&at, "x86_64-musl", GOOD);
    fs::write(d.join("sources"), format!("file://{}\n", at.join("hello-1.0.tar").display())).unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_FETCH", "curl -fL -o %o %u")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(root.join("var/kiry/cache/sources/hello-1.0.tar").exists());
    assert!(!o.stderr.contains(&b'\r'), "{}", String::from_utf8_lossy(&o.stderr));
}

// a rebuild that comes out as the bytes it replaced was not needed, and the log is what
// says so across storms. a queue row the build cannot change anything about is that case
#[test]
fn a_rebuild_that_changed_nothing_is_logged_as_the_same() {
    let at = scratch("rebuild-same");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let d = recipe(&at.join("repo"), "x86_64-musl", GOOD);
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", at.join("repo").display())).unwrap();
    let r = root.to_str().unwrap();
    let o = kiry(&["b", "--root", r, d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let arc = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst");
    assert!(kiry(&["i", "--root", r, arc.to_str().unwrap()]).status.success());
    fs::write(root.join("usr/lib/kiry/db/queue"), "x86_64-musl libx.so.1 hello\n").unwrap();

    let o = kiry(&["rebuild", "--root", r]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(said.contains("hello 1.0 x86_64-musl rebuilt  to the same bytes it replaced"), "{said}");
    let log = fs::read_to_string(root.join("var/kiry/log/rebuilds")).unwrap();
    assert!(log.starts_with("hello x86_64-musl libx.so.1 same "), "{log}");
    let o = kiry(&["stats", "--root", r]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("rebuilt 1, 1 to the same bytes"));
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

// the package is the third field, and a line now carries the symbols after it
fn queued_pkg(l: &str) -> &str {
    l.split(' ').nth(2).unwrap_or("")
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
        q.iter().any(|l| queued_pkg(l) == "useq"),
        "useq calls q and is not queued: {q:?}"
    );
    assert!(
        !q.iter().any(|l| queued_pkg(l) == "usep"),
        "usep never called q and was queued anyway: {q:?}"
    );
}

// a soname bump leaves the old library preserved, so the consumer still loads and no
// check sees anything wrong. the queue is what gets it rebuilt and the old copy released
#[test]
fn a_soname_bump_queues_what_still_names_the_old_one() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-bump");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let one = lib_with(&at.join("v1"), "libp.so.1", "void p(void){}\n");
    let two = lib_with(&at.join("v2"), "libp.so.2", "void p(void){}\n");
    let first = archive(&at, "foo-1", &[("usr/lib64/libp.so.1", &one)]);
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    place(&root, "usep", &[("usr/bin/usep", &app_calling(&at.join("a"), "usep", "p", &one))]);

    let second = archive(&at, "foo-2", &[("usr/lib64/libp.so.2", &two)]);
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("preserved libp.so.1"));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| queued_pkg(l) == "usep" && l.contains("libp.so.1")),
        "usep still names libp.so.1 and is not queued: {q:?}"
    );
}

// dlsym reaches a library by a string no undefined set records, so a consumer that can
// call it is rebuilt whatever it links against by name
#[test]
fn a_consumer_that_can_dlsym_is_queued_whatever_it_imports() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-dlsym");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let both = lib_with(&at.join("v1"), "libp.so.1", "void p(void){}\nvoid q(void){}\n");
    let gone = lib_with(&at.join("v2"), "libp.so.1", "void p(void){}\n");
    let dl = lib_with(&at.join("dl"), "libdl.so.2", "void dlsym(void){}\n");
    let first = archive(&at, "foo-1", &[("usr/lib64/libp.so.1", &both)]);
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let a = at.join("a");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("u.c"), "void p(void);\nvoid dlsym(void);\nvoid _start(void){p();dlsym();}\n").unwrap();
    assert!(Command::new("cc")
        .args(["-nostdlib", "-Wl,-rpath,$ORIGIN/../lib64", "-o"])
        .arg(a.join("usedl"))
        .arg(a.join("u.c"))
        .arg(&both)
        .arg(&dl)
        .status()
        .unwrap()
        .success());
    place(
        &root,
        "usedl",
        &[("usr/bin/usedl", &a.join("usedl")), ("usr/lib64/libdl.so.2", &dl)],
    );
    place(&root, "usep", &[("usr/bin/usep", &app_calling(&at.join("b"), "usep", "p", &both))]);

    let second = archive(&at, "foo-2", &[("usr/lib64/libp.so.1", &gone)]);
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| queued_pkg(l) == "usedl"),
        "usedl can dlsym and is not queued: {q:?}"
    );
    assert!(
        !q.iter().any(|l| queued_pkg(l) == "usep"),
        "usep never called q and was queued anyway: {q:?}"
    );
}

// a config file the admin changed is theirs. a library that does not match its manifest
// is a broken install, and preserving it would hide the break behind a working-looking one
#[test]
fn an_edited_config_survives_and_a_library_does_not() {
    let at = scratch("edited");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let src = at.join("src");
    fs::create_dir_all(&src).unwrap();
    let put = |n: &str, b: &str| -> PathBuf {
        let p = src.join(n);
        fs::write(&p, b).unwrap();
        p
    };

    let one = archive(
        &at,
        "cfg-1",
        &[
            ("etc/thing.conf", &put("c1", "shipped\n")),
            ("usr/lib/thing.so", &put("l1", "one\n")),
        ],
    );
    assert!(kiry(&["i", "--root", r, one.to_str().unwrap()])
        .status
        .success());

    fs::write(root.join("etc/thing.conf"), b"mine\n").unwrap();
    fs::write(root.join("usr/lib/thing.so"), b"tampered\n").unwrap();

    let two = archive(
        &at,
        "cfg-2",
        &[
            ("etc/thing.conf", &put("c2", "shipped2\n")),
            ("usr/lib/thing.so", &put("l2", "two\n")),
        ],
    );
    let o = kiry(&["i", "--root", r, "--force", two.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    assert_eq!(
        fs::read(root.join("etc/thing.conf")).unwrap(),
        b"mine\n",
        "the edit was overwritten"
    );
    assert_eq!(
        fs::read(root.join("usr/lib/thing.so")).unwrap(),
        b"two\n",
        "a library was kept instead of replaced"
    );

    // the config that was not written is the one nobody can see, and a line that only
    // says it was kept leaves you to go and find the artifact yourself
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("kept /etc/thing.conf"), "{said}");
    assert!(said.contains(two.to_str().unwrap()), "{said}");
}

// putting the symbol back is the other way a break ends, and the entry has to go with it
#[test]
fn a_symbol_that_comes_back_clears_the_entry() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-heal");
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
    assert!(kiry(&["i", "--root", r, first.to_str().unwrap()])
        .status
        .success());

    place(
        &root,
        "useq",
        &[(
            "usr/bin/useq",
            &app_calling(&at.join("b"), "useq", "q", &both),
        )],
    );

    let second = archive(&at, "foo-2", &[("usr/lib64/libp.so.1", &gone)]);
    assert!(kiry(&["i", "--root", r, "--force", second.to_str().unwrap()])
        .status
        .success());
    assert!(
        queued(&root).iter().any(|l| queued_pkg(l) == "useq"),
        "useq was never queued: {:?}",
        queued(&root)
    );

    let third = archive(&at, "foo-3", &[("usr/lib64/libp.so.1", &both)]);
    assert!(kiry(&["i", "--root", r, "--force", third.to_str().unwrap()])
        .status
        .success());

    let o = kiry(&["rebuild", "--root", r, "-n"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        queued(&root).is_empty(),
        "q came back and useq is still queued: {:?}",
        queued(&root)
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
        q.iter().any(|l| queued_pkg(l) == "other"),
        "other still links the old layout and is not queued: {q:?}"
    );
    assert!(
        !q.iter().any(|l| queued_pkg(l) == "foo"),
        "foo ships the library and was rebuilt with it: {q:?}"
    );
}

fn config(root: &Path, name: Option<&str>, body: &str) {
    let at = match name {
        Some(n) => root.join("etc/kiry/pkg").join(n),
        None => root.join("etc/kiry/config"),
    };
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    fs::write(at, body).unwrap();
}

fn resolved(root: &Path, name: &str) -> String {
    let o = kiry(&["flags", "--root", root.to_str().unwrap(), name]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

// the whole of subsystem 3 in one assertion. compile() named every variable it wanted
// and CFLAGS was not among them, so every meson package in the tree was built at -O0
// and nothing anywhere reported it
#[test]
fn a_build_is_handed_the_flags_the_config_resolves() {
    let at = scratch("cflags");
    let d = recipe(
        &at,
        "x86_64-musl",
        "mkdir -p \"$DESTDIR/usr/bin\"\necho \"$CFLAGS|$LDFLAGS\" > \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nLTO thin\n");

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
    let (c, ld) = said.trim().split_once('|').unwrap();
    assert_eq!(c, "-O3 -march=znver3 -flto=thin -g0", "{said}");
    assert_eq!(ld, "-flto=thin -Wl,--thinlto-jobs=8 -Wl,-O2", "{said}");
}

// rustc reads none of CFLAGS, so a rust package was building for a generic x86-64 while
// every c package around it got znver3
#[test]
fn a_rust_build_is_handed_the_machine_it_runs_on() {
    let at = scratch("rustflags");
    let d = recipe(
        &at,
        "x86_64-musl",
        "mkdir -p \"$DESTDIR/usr/bin\"\necho \"$RUSTFLAGS\" > \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nLTO thin\n");

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
    assert_eq!(said.trim(), "-C target-cpu=znver3 -C opt-level=2", "{said}");
}

// lto is the one thing not mapped across: cargo decides embed-bitcode from the profile
// and rustc refuses that with -C lto, so a crate would stop building
#[test]
fn no_lto_reaches_rustflags() {
    let at = scratch("rustnolto");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nLTO full\n");
    // flags answers for a package it knows of, and this one has nothing of its own
    config(&root, Some("anything"), "");
    let said = resolved(&root, "anything");
    assert!(said.contains("resolved RUSTFLAGS -C target-cpu=znver3 -C opt-level=2\n"), "{said}");
    assert!(said.contains("resolved CFLAGS -O2 -march=znver3 -flto=full -g0"), "{said}");
}

// same last-wins rule the c flags have, and kiry's own package leans on it to keep the
// opt-level its release profile asks for
#[test]
fn a_package_takes_the_rust_opt_level_back() {
    let at = scratch("rustopt");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\n");
    config(&root, Some("hello"), "RUSTFLAGS -C opt-level=3\n");
    let said = resolved(&root, "hello");
    assert!(
        said.contains("resolved RUSTFLAGS -C target-cpu=znver3 -C opt-level=2 -C opt-level=3\n"),
        "{said}"
    );
}

// what the sidecar carries is what the record ends up holding, which is the only
// reason a later resolve has something to differ from
#[test]
fn an_artifact_records_what_it_was_built_with() {
    let at = scratch("flagmeta");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "CFLAGS_MARCH znver3\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let meta = root
        .join("var/kiry/cache")
        .join(&sidecars(&root)[0])
        .join("flags");
    let said = fs::read_to_string(meta).unwrap();
    assert!(said.contains("CFLAGS -O2 -march=znver3 -g0"), "{said}");
}

// a knob is the package's to replace and CFLAGS is the package's to add to. taking
// something back out of the global set is what a recipe's filter file is for
#[test]
fn a_package_replaces_a_knob_and_appends_to_the_rest() {
    let root = scratch("override");
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nCFLAGS -pipe\n");
    config(&root, Some("glibc"), "OPT -O2\nLTO none\nCFLAGS -fno-lto\n");

    let said = resolved(&root, "glibc");
    assert!(
        said.contains("resolved CFLAGS -O2 -march=znver3 -g0 -pipe -fno-lto"),
        "{said}"
    );
    // where each line came from, or a filter that looks wrong in six months has no
    // reason beside it
    assert!(said.contains("/etc/kiry/config OPT -O3"), "{said}");
    assert!(said.contains("/etc/kiry/pkg/glibc OPT -O2"), "{said}");
}

// a global flag turned off for one package, without that package restating the set.
// the off is kept, because off is the choice that takes a dependency out, and only the
// flags foot's gentoo entry declares count for foot
#[test]
fn a_later_minus_takes_a_flag_back_off() {
    let root = scratch("useflags");
    let d = root.join("var/db/kiry/extra/foot");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.23.1 1\n").unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("build"), "true\n").unwrap();
    fs::write(d.join("gentoo"), "# gui-apps/foot-1.23.1\nIUSE=x11 +wayland vulkan\n").unwrap();
    config(&root, None, "flags x11 wayland vulkan gtk\n");
    config(&root, Some("foot"), "flags -x11\n");

    assert!(
        resolved(&root, "foot").contains("resolved FLAGS -x11 vulkan wayland\n"),
        "{}",
        resolved(&root, "foot")
    );
}

// silence on a typo would mean a flag that never applied and nothing saying so
#[test]
fn a_setting_that_is_not_one_is_refused_by_name() {
    let root = scratch("typo");
    config(&root, None, "OPT -O2\nCFLAG -pipe\n");

    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "mesa"]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("/etc/kiry/config:2"), "{said}");
    assert!(said.contains("CFLAG is not a setting"), "{said}");
}

// the jobs cap is a thinlto thing, and lld warns at anything else
#[test]
fn thinlto_jobs_only_appear_under_thin() {
    let root = scratch("ltojobs");
    config(&root, None, "LTO full\n");
    config(&root, Some("mesa"), "");
    let said = resolved(&root, "mesa");
    assert!(said.contains("resolved LDFLAGS -flto=full -Wl,-O2"), "{said}");
    assert!(!said.contains("thinlto-jobs"), "{said}");
}

// editing the config is what fills this queue, and nothing else would notice
#[test]
fn a_record_that_no_longer_resolves_is_queued() {
    let root = scratch("flagdiff");
    record(&root, "mesa", &[], Vec::new());
    db::write(
        &root,
        &db::Installed {
            name: "wlroots".into(),
            target: "x86_64-musl".into(),
            version: Version::parse("1.0 1").unwrap(),
            depends: Vec::new(),
            manifest: Vec::new(),
            hash: String::new(),
            users: Vec::new(),
            flags: vec!["CFLAGS -O2 -g0".into()],
        },
    )
    .unwrap();

    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "--queue"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    // one was never recorded, the other was and no longer matches
    assert!(said.contains("mesa x86_64-musl unrecorded"), "{said}");
    assert!(said.contains("wlroots x86_64-musl stale"), "{said}");

    let q = fs::read_to_string(db::queue(&root)).unwrap();
    assert!(q.contains("x86_64-musl flags mesa"), "{q}");
    assert!(q.contains("x86_64-musl flags wlroots"), "{q}");
}

// a package built with exactly what resolves now has nothing to rebuild for
#[test]
fn a_record_that_still_resolves_is_left_alone() {
    let root = scratch("flagsame");
    db::write(
        &root,
        &db::Installed {
            name: "foot".into(),
            target: "x86_64-musl".into(),
            version: Version::parse("1.0 1").unwrap(),
            depends: Vec::new(),
            manifest: Vec::new(),
            hash: String::new(),
            users: Vec::new(),
            flags: vec![
                "CFLAGS -O2 -g0".into(),
                "CXXFLAGS -O2 -g0".into(),
                "LDFLAGS -Wl,-O2".into(),
                "RUSTFLAGS -C opt-level=2".into(),
            ],
        },
    )
    .unwrap();

    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "--queue"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "");
    assert!(!db::queue(&root).exists());
}

// the queue only ever grew: a package rebuilt into agreement stayed listed, so after a
// drain the queue still named everything it had started with
#[test]
fn a_package_that_resolves_clean_leaves_the_queue() {
    let root = scratch("flagdrop");
    let rec = |flags: Vec<String>| db::Installed {
        name: "foot".into(),
        target: "x86_64-musl".into(),
        version: Version::parse("1.0 1").unwrap(),
        depends: Vec::new(),
        manifest: Vec::new(),
        hash: String::new(),
        users: Vec::new(),
        flags,
    };

    db::write(&root, &rec(vec!["CFLAGS -O1".into()])).unwrap();
    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "--queue"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let q = fs::read_to_string(db::queue(&root)).unwrap();
    assert!(q.contains("x86_64-musl flags foot"), "{q}");

    // what a rebuild would leave behind
    db::write(
        &root,
        &rec(vec![
            "CFLAGS -O2 -g0".into(),
            "CXXFLAGS -O2 -g0".into(),
            "LDFLAGS -Wl,-O2".into(),
            "RUSTFLAGS -C opt-level=2".into(),
        ]),
    )
    .unwrap();
    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "--queue"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!db::queue(&root).exists(), "{:?}", fs::read_to_string(db::queue(&root)));
}

// a soname row is someone else's, and regenerating the flags rows must not take it out
#[test]
fn regenerating_the_flags_rows_leaves_a_soname_row_alone() {
    let root = scratch("flagkeep");
    db::write_queue(
        &root,
        &[db::Queued {
            target: "x86_64-musl".into(),
            name: "mpv".into(),
            soname: "libavcodec.so.62".into(),
            changed: vec!["av_frame_get".into()],
        }],
    )
    .unwrap();
    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "--queue"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let q = fs::read_to_string(db::queue(&root)).unwrap();
    assert!(q.contains("x86_64-musl libavcodec.so.62 mpv av_frame_get"), "{q}");
}

// recipe() finds a package by a build script, so the filter needs one beside it
fn filtered(root: &Path, at: &Path, name: &str, body: &str) {
    let repo = at.join("repo");
    let d = repo.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("build"), GOOD).unwrap();
    fs::write(d.join("filter"), body).unwrap();
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();
}

// the point of the whole subsystem: the recipe takes lto back out, and the -O3 the
// config raised globally still arrives
#[test]
fn a_filter_is_subtractive_and_not_an_override() {
    let root = scratch("filterlto");
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nLTO thin\n");
    filtered(&root, &root, "mesa", "filter-lto  # oomed at 16 jobs, 2026-09-09\n");

    let said = resolved(&root, "mesa");
    assert!(
        said.contains("resolved CFLAGS -O3 -march=znver3 -g0"),
        "{said}"
    );
    assert!(said.contains("resolved LDFLAGS -Wl,-O2"), "{said}");
    assert!(!said.contains("flto"), "{said}");
    assert!(!said.contains("thinlto-jobs"), "{said}");
    // the reason stays on the line and out of the flags
    assert!(!said.contains("oomed"), "{said}");
}

#[test]
fn a_filter_verb_that_is_not_one_is_refused_by_name() {
    let root = scratch("filtertypo");
    filtered(&root, &root, "mesa", "filter-lto\nfliter-flags -O2\n");

    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "mesa"]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("filter:2"), "{said}");
    assert!(said.contains("fliter-flags is not a flag-o-matic verb"), "{said}");
}

#[test]
fn the_other_verbs_reach_the_flags_they_name() {
    let root = scratch("filterverbs");
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nCFLAGS -fomit-frame-pointer\n");
    filtered(
        &root,
        &root,
        "thing",
        "replace-flags -O3 -O2\nfilter-flags -march=*\nappend-flags -fPIC\n",
    );

    let said = resolved(&root, "thing");
    assert!(
        said.contains("resolved CFLAGS -O2 -g0 -fomit-frame-pointer -fPIC"),
        "{said}"
    );
}

// only the ones that decide how fast and for what survive
#[test]
fn strip_flags_keeps_what_decides_code_generation() {
    let root = scratch("filterstrip");
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nCFLAGS -fomit-frame-pointer -DNDEBUG\n");
    filtered(&root, &root, "thing", "strip-flags\n");

    let said = resolved(&root, "thing");
    assert!(said.contains("resolved CFLAGS -O2 -march=znver3 -g0\n"), "{said}");
}

// the declarative half cannot help a build that only finds out at configure time, so
// the same verbs have to be reachable from the script
#[test]
fn a_build_reaches_flag_o_matic_through_lib_sh() {
    let at = scratch("libsh");
    let d = recipe(
        &at,
        "x86_64-musl",
        ". /usr/share/kiry/lib.sh\n\
         filter-lto\n\
         replace-flags -O2 -Os\n\
         mkdir -p \"$DESTDIR/usr/bin\"\n\
         echo \"$CFLAGS|$LDFLAGS\" > \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nLTO thin\n");

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
    // test-flags is the one verb that asks the compiler, and this fixture's closure is
    // busybox with no compiler in it, so it is exercised against ash directly instead
    let mut f = said.trim().split('|');
    assert_eq!(f.next(), Some("-Os -march=znver3 -g0"), "{said}");
    assert_eq!(f.next(), Some("-Wl,-O2"), "{said}");
}

// the loop, end to end: a build that fails with something the table knows, the fix
// written where it belongs, and the retry that then works
#[test]
fn a_failure_the_table_knows_is_fixed_and_rebuilt() {
    let at = scratch("recover");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-flto*) echo 'ld.lld: error: Not a valid object file' >&2; exit 1 ;;\n\
         esac\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("filter-lto"), "{f}");
    // the rule that fired, the line that matched and the date, beside the fix
    assert!(f.contains("Not a valid object file"), "{f}");
    assert!(f.contains("ld.lld: error:"), "{f}");
    assert!(f.contains("link phase") || f.contains("link failed"), "{f}");
}

// nothing in the table matches, so the only thing left that knows anything is the ladder
// the diagnostic shape is the part that earns it a rung: a compiler ran and complained
#[test]
fn a_failure_nothing_matches_walks_down_the_ladder() {
    let at = scratch("ladder");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-O3*) echo 'foo.c:3:9: error: the build is displeased' >&2; exit 1 ;;\n\
         esac\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let pkg = fs::read_to_string(root.join("etc/kiry/pkg/hello")).unwrap();
    assert!(pkg.contains("OPT -O2"), "{pkg}");
    assert!(pkg.contains("rung"), "{pkg}");
    // the failure goes beside the rung, so it can be judged after the log is gone
    assert!(pkg.contains("the build is displeased"), "{pkg}");
}

// a box whose config already says -O2 gets nothing from OPT -O2, and a build spent
// finding that out is a full build of a result already known -- hours, for mesa
#[test]
fn a_rung_that_changes_no_flag_costs_no_build() {
    let at = scratch("noop-rung");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-march=*) echo 'foo.c:3:9: error: not on this cpu' >&2; exit 1 ;;\n\
         esac\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    // the first try, filter-lto, then the march rung that fixes it
    let tries = said.lines().filter(|l| l.contains(" building ")).count();
    assert_eq!(tries, 3, "{said}");
    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("filter-flags -march=*"), "{f}");
    assert!(
        !root.join("etc/kiry/pkg/hello").exists(),
        "a rung that changed nothing was left behind"
    );
}

// every rung and it still fails, so none of them is a fix. left behind they read as one,
// which is what retroarch was found carrying
#[test]
fn a_recovery_that_ends_stuck_takes_its_rungs_back_out() {
    let at = scratch("stuck-rungs");
    let d = recipe(&at, "x86_64-musl", "echo 'foo.c:3:9: error: no flag helps' >&2\nexit 1\n");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nLTO thin\n");
    fs::create_dir_all(root.join("etc/kiry/pkg")).unwrap();
    fs::write(root.join("etc/kiry/pkg/hello"), "CFLAGS -fmine\n").unwrap();

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("the ladder ran out"), "{err}");
    assert_eq!(fs::read_to_string(root.join("etc/kiry/pkg/hello")).unwrap(), "CFLAGS -fmine\n");
    // the stuck note stays, and it is the only thing there
    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("stuck"), "{f}");
    assert!(f.lines().all(|l| l.trim().is_empty() || l.starts_with('#')), "{f}");
}

// the ladder ends on the smallest set of flags, not on -pipe, which only moves where the
// compiler keeps its temporaries and so can only ever spend the build
#[test]
fn the_last_rung_is_the_smallest_set_of_flags() {
    let at = scratch("strip-rung");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-fbad-idea*) echo 'foo.c:3:9: error: that flag again' >&2; exit 1 ;;\n\
         esac\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nLTO thin\n");
    config(&root, Some("hello"), "CFLAGS -fbad-idea\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("strip-flags"), "{f}");
}

// the sandbox has no compiler, and a crash on demand is not something a real clang
// can be asked for per flag. this one does what clang does on a crash when its
// arguments match, and otherwise writes down what it was handed
fn fake_clang(when: &str, then: &str) -> String {
    format!(
        "mkdir -p /src/bin\n\
         printf '%s\\n' '#!/bin/sh' 'case \" $* \" in {when}) {then} ;; esac' \
         'echo \"$*\" >> /src/args' >/src/bin/clang\n\
         chmod +x /src/bin/clang\n\
         export PATH=/src/bin:$PATH\n"
    )
}

const CRASH: &str =
    "echo \"PLEASE submit a bug report to https://github.com/llvm/llvm-project/issues/\" >&2; exit 1";

fn tries(o: &Output) -> usize {
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| l.contains(" building "))
        .count()
}

// a crash is answered on the file it happened to, in place. the build system never sees
// it, so nothing that already compiled goes again, and the rest of the package keeps
// every flag it had
#[test]
fn a_crash_steps_the_one_file_down_in_place() {
    let at = scratch("inflight");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "{}kirycc $CFLAGS -c x.c -o x.o\n\
             grep -q march /src/args && exit 1\n{GOOD}",
            fake_clang("*-march=*", CRASH)
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nCFLAGS_MARCH znver3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert_eq!(tries(&o), 1, "{said}");
    assert!(said.contains("x.o crashed clang, again at CFLAGS_MARCH"), "{said}");
    // the config already says -O2, so that rung is the same command twice
    assert!(!said.contains("again at OPT -O2"), "{said}");
    assert!(!d.join("filter").exists());
    assert!(!root.join("etc/kiry/pkg/hello").exists());
}

// the ladder in place ends where recover()'s does, on the smallest set of flags, and it
// can only take out what kiry put in: $CFLAGS is how it tells the two apart
#[test]
fn the_last_rung_in_place_is_the_smallest_set_of_flags() {
    let at = scratch("inflight-strip");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "{}kirycc $CFLAGS -DKEEP -c x.c -o x.o\n\
             grep -q bad-idea /src/args && exit 1\n\
             grep -q KEEP /src/args || exit 1\n{GOOD}",
            fake_clang("*-fbad-idea*", CRASH)
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O2\nLTO thin\n");
    config(&root, Some("hello"), "CFLAGS -fbad-idea\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("again at strip-flags"));
}

// stdin is read once. a retry would compile nothing and could well succeed at it
#[test]
fn a_compile_fed_on_stdin_gets_one_go() {
    let at = scratch("inflight-stdin");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "{}echo 'int x;' | kirycc $CFLAGS -x c -c - -o x.o\n{GOOD}",
            fake_clang("*-march=*", CRASH)
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "CFLAGS_MARCH znver3\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stdout).contains("again at"));
}

// the sandbox has no network, so the crates come in before it starts. this cargo leaves
// a file where CARGO_HOME says, and the build has to find it there and be told offline
#[test]
fn a_cargo_lock_is_fetched_outside_and_the_build_finds_it_offline() {
    let at = scratch("cargo");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "[ \"$CARGO_NET_OFFLINE\" = true ] || exit 1\n\
             [ -e \"$CARGO_HOME/registry/fetched\" ] || exit 1\n{GOOD}"
        ),
    );
    fs::write(at.join("src/hello-1.0/Cargo.lock"), "version = 4\n").unwrap();
    let arc = tarball(&at);
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();

    let bin = at.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("cargo"),
        format!(
            "#!/bin/sh\necho \"$*\" >> {}\nmkdir -p \"$CARGO_HOME/registry\"\n\
             touch \"$CARGO_HOME/registry/fetched\"\n",
            at.join("args").display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("cargo"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let args = fs::read_to_string(at.join("args")).unwrap();
    assert!(args.starts_with("fetch --locked --manifest-path "), "{args}");
    assert!(args.trim_end().ends_with("hello-1.0/Cargo.toml"), "{args}");
}

// go.sum is go's lock, and the modules come in the same way the crates do. this go
// writes where GOMODCACHE says, from the directory the go.sum is in
#[test]
fn a_go_sum_is_fetched_outside_and_the_build_finds_it_offline() {
    let at = scratch("gomod");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "[ \"$GOPROXY\" = off ] && [ \"$GOTOOLCHAIN\" = local ] || exit 1\n\
             [ -e \"$GOMODCACHE/cache/fetched\" ] || exit 1\n{GOOD}"
        ),
    );
    fs::write(at.join("src/hello-1.0/go.sum"), "").unwrap();
    let arc = tarball(&at);
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();

    let bin = at.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("go"),
        format!(
            "#!/bin/sh\necho \"$* $(basename \"$PWD\") $GOTOOLCHAIN\" >> {}\n\
             mkdir -p \"$GOMODCACHE/cache\"\ntouch \"$GOMODCACHE/cache/fetched\"\n",
            at.join("args").display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("go"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let args = fs::read_to_string(at.join("args")).unwrap();
    assert_eq!(args, "mod download hello-1.0 local\n");
}

// clang gives a language it has no frontend for to gcc, and gcc is a link to clang, so
// an ada probe forked until something killed the build. this clang hands over the way
// the real one does, and this gcc only says it ran
const HANDOFF: &str =
    "g=gcc; p=; for a do [ \"$p\" = -ccc-gcc-name ] && g=$a; p=$a; done; exec \"$g\" \"$@\"";

#[test]
fn a_language_clang_hands_to_gcc_does_not_come_back_to_clang() {
    let at = scratch("adaloop");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "{}printf '%s\\n' '#!/bin/sh' 'echo gcc >> /src/ran; exit 1' >/src/bin/gcc\n\
             chmod +x /src/bin/gcc\n\
             kirycc -c conftest.adb -o x.o && exit 1\n\
             [ -e /src/ran ] && exit 1\n{GOOD}",
            fake_clang("*.adb*", HANDOFF)
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// the wrapper already tried every rung on that file, so a restart would only walk the
// same ladder over the whole package. the table's crash row included
#[test]
fn a_file_that_crashes_on_every_rung_is_stuck_after_one_build() {
    let at = scratch("inflight-stuck");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!("{}kirycc $CFLAGS -c x.c -o x.o\n{GOOD}", fake_clang("*", CRASH)),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nCFLAGS_MARCH znver3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("x.o crashed clang at every rung"), "{err}");
    assert_eq!(tries(&o), 1, "{}", String::from_utf8_lossy(&o.stdout));
    assert!(!root.join("etc/kiry/pkg/hello").exists());
    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("stuck") && !f.contains("rung "), "{f}");
}

// the machine running out is not the compiler's fault, and half the thinlto jobs link
// the same code. a relink rather than a rebuild
#[test]
fn a_killed_link_goes_again_with_half_the_lto_jobs() {
    let at = scratch("inflight-oom");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "{}kirycc $LDFLAGS -o x x.o\n\
             grep -q thinlto-jobs=4 /src/args || exit 1\n{GOOD}",
            fake_clang("*--thinlto-jobs=8*", "kill -9 $$")
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "LTO thin\nKIRY_THINLTO_JOBS 8\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert_eq!(tries(&o), 1, "{said}");
    assert!(said.contains("x was killed, again with half the lto jobs"), "{said}");
}

// lld against a thinlto cache, as far as kiry can see it: each module's object is opened
// out of the dir when it is there, and written to a temp name and renamed in when not
const LLD: &str = "mkdir -p /src/bin\n\
cat > /src/bin/clang <<'X'\n\
#!/bin/sh\n\
for a do case $a in -Wl,--thinlto-cache-dir=*) d=${a#*=} ;; esac; done\n\
[ -n \"$d\" ] || exit 0\n\
for k in a b; do\n\
\tif [ -e \"$d/llvmcache-$k\" ]; then cat \"$d/llvmcache-$k\" >/dev/null\n\
\telse echo o > \"$d/Thin-$k.tmp.o\" && mv \"$d/Thin-$k.tmp.o\" \"$d/llvmcache-$k\"; fi\n\
done\n\
X\n\
chmod +x /src/bin/clang\n\
export PATH=/src/bin:$PATH\n\
[ -z \"$KIRY_LTO_CACHE\" ] || exit 1\n\
kirycc $LDFLAGS -o x x.o\n";

// the objects a link made last time come back out of the cache the next time, and the
// rate says so on the ok row and in stats, a pipe getting the same words a tty does
#[test]
fn a_second_build_takes_its_thinlto_objects_from_the_cache() {
    let at = scratch("lto-cache");
    let d = recipe(&at, "x86_64-musl", &format!("{LLD}{GOOD}"));
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let (r, dir) = (root.to_str().unwrap(), d.to_str().unwrap());

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let ok = said.lines().find(|l| l.contains(" ok ")).unwrap_or_default();
    assert!(ok.ends_with(" thinlto 0%") || ok.contains(" thinlto 0% "), "{said}");

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let ok = said.lines().find(|l| l.contains(" ok ")).unwrap_or_default();
    assert!(ok.contains(" thinlto 100%"), "{said}");
    assert!(!ok.contains('\x1b'), "{ok}");

    let log = fs::read_to_string(root.join("var/kiry/log/thinlto")).unwrap();
    let rows: Vec<Vec<&str>> = log.lines().map(|l| l.split(' ').collect()).collect();
    assert_eq!(rows.len(), 2, "{log}");
    assert_eq!(&rows[0][..4], ["hello", "x86_64-musl", "0", "2"], "{log}");
    assert_eq!(&rows[1][..4], ["hello", "x86_64-musl", "2", "0"], "{log}");

    let o = kiry(&["stats", "--root", r]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("thinlto 50% of 4 modules"), "{said}");
}

// a link that runs twice in one build, the way kirycc relinks after a kill, reads back
// what the first run wrote. those objects were made by this build, not saved for it
#[test]
fn a_build_does_not_count_its_own_thinlto_objects_as_hits() {
    let at = scratch("lto-twice");
    let d = recipe(&at, "x86_64-musl", &format!("{LLD}kirycc $LDFLAGS -o x x.o\n{GOOD}"));
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains(" thinlto 0%"), "{said}");
}

// an entry only ever comes back for the same package under the same llvm, so gc takes
// the rest: a directory for another llvm, and one for a package nothing builds any more
#[test]
fn gc_drops_thinlto_caches_nothing_can_read_again() {
    let Some((at, root, repo)) = workshop("lto-gc") else {
        return;
    };
    buildable(&at, &repo, "kept", "");
    let lto = root.join("var/kiry/lto");
    for d in ["kept/none", "kept/23.1.1-2", "gone/none"] {
        fs::create_dir_all(lto.join(d)).unwrap();
        fs::write(lto.join(d).join("llvmcache-a"), "o").unwrap();
    }

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(lto.join("kept/none/llvmcache-a").is_file(), "the live cache went");
    assert!(!lto.join("kept/23.1.1-2").exists(), "another llvm's cache stayed");
    assert!(!lto.join("gone").exists(), "a cache for no recipe stayed");
}

const RUNG: &str = "# 2026-09-12 rung 1 after compile failed on x86_64-musl\nfilter-lto\n";
const TABLE: &str =
    "# 2026-09-20 link failed, recompile with -fPIC\n#   recompile with -fPIC\nappend-flags -fPIC\n";

// a rung nothing needs any more goes, and the fix the table wrote off a line it matched
// stays where it is
#[test]
fn bisect_takes_back_a_rung_nothing_needs_any_more() {
    let at = scratch("bisect-stale");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "LTO thin\n");
    fs::write(d.join("filter"), format!("{TABLE}{RUNG}")).unwrap();

    let o = kiry(&["bisect-flags", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(tries(&o), 1);
    assert_eq!(fs::read_to_string(d.join("filter")).unwrap(), TABLE);

    // and once there is no rung left there is nothing to build
    let o = kiry(&["bisect-flags", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(tries(&o), 0);
    assert!(String::from_utf8_lossy(&o.stdout).contains("hello carries no rung"));
}

// one still needed comes back, with today's note and the line that failed beside it
// rather than the old one that only knew the phase
#[test]
fn bisect_puts_back_a_rung_that_is_still_needed() {
    let at = scratch("bisect-needed");
    let d = recipe(
        &at,
        "x86_64-musl",
        &format!(
            "case \"$CFLAGS\" in\n\
             *-flto*) echo 'foo.c:1:1: error: lto again' >&2; exit 1 ;;\n\
             esac\n{GOOD}"
        ),
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "LTO thin\n");
    fs::write(d.join("filter"), RUNG).unwrap();

    let o = kiry(&["bisect-flags", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(tries(&o), 2);
    let f = fs::read_to_string(d.join("filter")).unwrap();
    assert!(f.contains("filter-lto") && f.contains("lto again"), "{f}");
    assert!(!f.contains("2026-09-12"), "{f}");
}

// stuck from the full flags says nothing about the rungs, so the package goes back to
// exactly what it built with
#[test]
fn a_bisect_that_gets_stuck_leaves_the_package_as_it_was() {
    let at = scratch("bisect-stuck");
    let d = recipe(&at, "x86_64-musl", "echo 'curl: could not resolve host' >&2\nexit 1\n");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    fs::write(d.join("filter"), format!("{TABLE}{RUNG}")).unwrap();

    let o = kiry(&["bisect-flags", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    assert_eq!(fs::read_to_string(d.join("filter")).unwrap(), format!("{TABLE}{RUNG}"));
}

// no flag change can reach a portability bug, so it says so instead of burning retries
#[test]
fn a_failure_no_flag_can_fix_says_so_at_once() {
    let at = scratch("stuck");
    let d = recipe(
        &at,
        "x86_64-musl",
        "echo \"ld.lld: error: undefined reference to \\`__isoc99_sscanf'\" >&2\nexit 1\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("stuck, musl-portability"), "{said}");
    // and nothing was written to the package's settings, since no setting would help
    assert!(!root.join("etc/kiry/pkg/hello").exists());
}

// a tool that is not installed is not a flag. the ladder used to spend all five rungs
// finding that out and leave four settings and a filter line behind for someone to undo
#[test]
fn a_failure_before_anything_compiled_leaves_the_flags_alone() {
    let at = scratch("nocompile");
    // the sign-off after the real complaint is what mozbuild does, and the last line is
    // the wrong one to quote
    let d = recipe(
        &at,
        "x86_64-musl",
        "echo '/build: line 3: unzip: not found' >&2\n\
         echo 'Streaming resource usage profile to: /src/obj/profile.json' >&2\nexit 1\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("nothing compiled"), "{said}");
    // and it hands over the line worth reading, so the real reason costs no log opening
    assert!(said.contains("unzip: not found"), "{said}");
    assert!(!root.join("etc/kiry/pkg/hello").exists(), "wrote a setting");
    assert!(!d.join("filter").exists(), "wrote a filter");
}

// every retry truncates the log, so without keeping one the failure that started the
// recovery is gone by the time the rung that answered it is up for judgement
#[test]
fn the_attempt_that_started_a_recovery_keeps_its_log() {
    let at = scratch("firstlog");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-O3*) echo 'foo.c:3:9: error: the build is displeased' >&2; exit 1 ;;\n\
         esac\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hello\"\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    config(&root, None, "OPT -O3\nLTO thin\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), "--recover", d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let logs = root.join("var/kiry/log");
    let name = fs::read_dir(&logs)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.ends_with(".first.log"))
        .unwrap_or_else(|| panic!("no first.log in {}", logs.display()));
    let kept = fs::read_to_string(logs.join(&name)).unwrap();
    assert!(kept.contains("the build is displeased"), "{kept}");
    // and the live log is the attempt that worked, so the two say different things
    let live = fs::read_to_string(logs.join(name.replace(".first.log", ".log"))).unwrap();
    assert!(!live.contains("the build is displeased"), "{live}");
}

// the cycle above, with one member saying how to start it. the stand-in is built first
// and the same package is still built properly afterwards
#[test]
fn a_cycle_a_bootstrap_file_declares_can_be_planned() {
    let at = scratch("bootcycle");
    if !bootstrap(&at.join("probe")) {
        return;
    }
    let root = one_target_root(&at);
    two_consumers(&at, &root, "alpha");
    fs::write(at.join("repo/alpha/bootstrap"), ":\n").unwrap();

    let o = kiry(&["rebuild", "--root", root.to_str().unwrap(), "-n"]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines[0], "alpha x86_64-gnu would bootstrap", "{said}");
    // beta can be built once the stand-in is in place, and alpha properly after it
    assert!(lines.contains(&"beta x86_64-gnu would rebuild"), "{said}");
    assert!(lines.contains(&"alpha x86_64-gnu would rebuild"), "{said}");
}

#[test]
fn why_shows_the_shortest_path_that_pulls_a_package_in() {
    let root = scratch("why");
    bare(&root, "app", &["mid"]);
    bare(&root, "mid", &["leaf"]);
    bare(&root, "leaf", &[]);
    bare(&root, "loner", &[]);

    let o = kiry(&["why", "--root", root.to_str().unwrap(), "leaf"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("x86_64-musl mid leaf"), "{said}");
    assert!(said.contains("x86_64-musl app mid leaf"), "{said}");

    let o = kiry(&["why", "--root", root.to_str().unwrap(), "loner"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("nothing depends on it"),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    // a name nothing knows is an error, not an empty answer
    assert!(!kiry(&["why", "--root", root.to_str().unwrap(), "absent"]).status.success());
}

// l answers for what is installed, and nothing answered for what is merely on offer
#[test]
fn search_finds_a_recipe_that_is_not_installed() {
    let at = scratch("search");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    bare(&root, "here", &[]);
    let repo = at.join("repo");
    for n in ["here", "elsewhere", "unrelated"] {
        let d = repo.join(n);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("build"), ":\n").unwrap();
        fs::write(d.join("version"), "2.1 1\n").unwrap();
    }
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();

    let o = kiry(&["search", "--root", root.to_str().unwrap(), "here"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("here 2.1 repo installed"), "{said}");
    assert!(said.contains("elsewhere 2.1 repo -"), "{said}");
    assert!(!said.contains("unrelated"), "{said}");
}

#[test]
fn log_prints_the_newest_build_log_for_a_package() {
    let root = scratch("logcmd");
    let d = root.join("var/kiry/log");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("hello-1.0-1.x86_64-musl.log"), "older\n").unwrap();
    fs::write(d.join("other-1.0-1.x86_64-musl.log"), "not this one\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(d.join("hello-2.0-1.x86_64-musl.log"), "newest\n").unwrap();

    let o = kiry(&["log", "--root", root.to_str().unwrap(), "hello"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("newest"), "{said}");
    assert!(!said.contains("older"), "{said}");
    assert!(!kiry(&["log", "--root", root.to_str().unwrap(), "absent"]).status.success());
}

#[test]
fn stats_counts_what_is_there() {
    let at = scratch("stats");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    bare(&root, "one", &[]);
    bare(&root, "two", &[]);
    let cache = root.join("var/kiry/cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("one-1.0-1.x86_64-musl.tar.zst"), vec![0u8; 4096]).unwrap();

    let o = kiry(&["stats", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("installed x86_64-musl 2"), "{said}");
    assert!(said.contains("cache 1 artifacts"), "{said}");
    assert!(said.contains("queued 0"), "{said}");
}

// busybox's xz decoder caps the dictionary it will allocate, so a source packed with a
// big one comes back as corrupt however right its checksum is. rustc's tarball uses
// 128 MiB and this is the smallest thing that reproduces it -- the payload is three bytes
#[test]
fn a_source_packed_with_a_big_dictionary_still_unpacks() {
    let at = scratch("bigdict");
    let d = recipe(&at, "x86_64-musl", GOOD);

    let xz = at.join("hello-1.0.tar.xz");
    let out = fs::File::create(&xz).unwrap();
    assert!(Command::new("xz")
        .arg("-c")
        .arg("--check=none")
        .arg("--lzma2=dict=128MiB")
        .arg(at.join("hello-1.0.tar"))
        .stdout(out)
        .status()
        .unwrap()
        .success());
    let sum = kiry_core::sha256(fs::File::open(&xz).unwrap()).unwrap();
    fs::write(d.join("sources"), "../hello-1.0.tar.xz\n").unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();

    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// the version a recipe carries moves by hand, so nothing in the tree said which had been
// left behind. these drive the real aports layout because the layout is the input
fn aport(root: &Path, repo: &str, name: &str, body: &str) {
    let d = root.join("var/kiry/aports").join(repo).join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("APKBUILD"), body).unwrap();
}

fn offer(repo: &Path, name: &str, version: &str) {
    let d = repo.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("build"), ":\n").unwrap();
    fs::write(d.join("version"), format!("{version} 1\n")).unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
}

// the recipes these ask about are ones the tree keeps whether or not they are installed,
// which is core. extra is a catalogue and a bare ahead only answers for what is
// installed out of it -- listed anyway, because a promotion of something new lands there
fn tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let at = scratch(name);
    let root = at.join("root");
    let (repo, catalogue, testing) = (at.join("core"), at.join("extra"), at.join("testing"));
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::create_dir_all(&testing).unwrap();
    fs::create_dir_all(&catalogue).unwrap();
    fs::write(
        root.join("etc/kiry/repos"),
        format!(
            "{}\n{}\n{}\n",
            repo.display(),
            catalogue.display(),
            testing.display()
        ),
    )
    .unwrap();
    (root, repo, testing)
}

fn ahead_of(root: &Path) -> String {
    let o = kiry(&["ahead", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn ahead_orders_what_it_can_and_says_nothing_where_it_cannot() {
    let (root, repo, _) = tree("ahead");
    // a digit run is a number, so 08 and 8 are the same release and a trailing zero
    // is not a bump. rc and a letter suffix are what scheme will have to describe
    for (n, ours, theirs) in [
        ("level", "1.2.3", "1.2.3"),
        ("padded", "2026.8.19", "2026.08.19"),
        ("trailing", "1.2", "1.2.0"),
        ("older", "1.2.3", "1.2.4"),
        ("newer", "9.0.1", "8.1.2"),
        ("grew", "1.2", "1.2.1"),
        ("letters", "2026c", "2026d"),
        ("candidate", "1.2", "1.2rc1"),
    ] {
        offer(&repo, n, ours);
        aport(&root, "main", n, &format!("pkgver={theirs}\n"));
    }

    let said = ahead_of(&root);
    for (n, status) in [
        ("level", "ok"),
        ("padded", "ok"),
        ("trailing", "ok"),
        ("older", "behind"),
        ("newer", "ahead"),
        ("grew", "behind"),
        ("letters", "unknown"),
        ("candidate", "unknown"),
    ] {
        let line = said
            .lines()
            .find(|l| l.split_whitespace().next() == Some(n))
            .unwrap_or_else(|| panic!("no row for {n}: {said}"));
        assert_eq!(line.split_whitespace().nth(3), Some(status), "{line}");
    }
    assert!(
        said.contains("8 recipes  2 behind  1 ahead  3 ok  0 untracked  2 unknown"),
        "{said}"
    );
    // the version it could not order is still printed, because that is the thing a
    // scheme has to be written against
    assert!(said.contains("2026d aports/main/letters unordered"), "{said}");
}

// main before community before testing, which is how aports promotes
#[test]
fn ahead_takes_the_first_aports_repo_that_carries_the_name() {
    let (root, repo, _) = tree("aheadrepo");
    offer(&repo, "both", "1.0");
    aport(&root, "main", "both", "pkgver=2.0\n");
    aport(&root, "community", "both", "pkgver=3.0\n");

    let said = ahead_of(&root);
    assert!(said.contains("2.0 aports/main"), "{said}");
    assert!(!said.contains("3.0"), "{said}");
}

// a pkgver naming a variable needs the shell to resolve, and a literal $pkgver in the
// note would read as a version nobody can act on
#[test]
fn ahead_leaves_a_pkgver_it_cannot_read_alone() {
    let (root, repo, _) = tree("aheadvar");
    offer(&repo, "computed", "1.0");
    aport(&root, "main", "computed", "_x=3\npkgver=1.$_x\n");
    offer(&repo, "quoted", "1.0");
    aport(&root, "main", "quoted", "pkgver=\"1.4\"\n");

    let said = ahead_of(&root);
    assert!(said.contains("computed"), "{said}");
    assert!(!said.contains("$_x"), "{said}");
    assert!(said.contains("1.4 aports/main"), "{said}");
}

// local/ is for what alpine does not carry. once it does, somebody else is maintaining
// it for you and nothing else in the tree would ever mention it
#[test]
fn ahead_says_when_a_local_recipe_turned_up_in_aports() {
    let at = scratch("aheaddrop");
    let root = at.join("root");
    let repo = at.join("local");
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();
    offer(&repo, "mine", "1.0");
    aport(&root, "community", "mine", "pkgver=1.0\n");

    let said = ahead_of(&root);
    assert!(said.contains("now in aports"), "{said}");
}

// a tracker is a fetch, so a report that did not ask for the network has to say it did
// not look rather than report the package as having no upstream at all
#[test]
fn ahead_does_not_read_a_tracker_unless_asked() {
    let (root, repo, _) = tree("aheadtracker");
    offer(&repo, "tracked", "1.0");
    fs::write(repo.join("tracked/tracker"), "https://example.invalid/v\n").unwrap();

    let said = ahead_of(&root);
    assert!(said.contains("tracker not read"), "{said}");
}

// an apkbuild is sourced, not parsed, so the shell abuild uses is not optional for
// anything that writes a bump
fn have_busybox() -> bool {
    if Command::new("busybox").arg("true").status().is_ok() {
        return true;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "no busybox, and a bump cannot be read out of an apkbuild without ash"
    );
    false
}

#[test]
fn sync_names_what_moved_and_where_it_would_land() {
    let (root, repo, testing) = tree("syncplan");
    offer(&repo, "moved", "1.0");
    aport(&root, "main", "moved", "pkgver=1.1\n");
    offer(&repo, "level", "2.0");
    aport(&root, "main", "level", "pkgver=2.0\n");

    let o = kiry(&["sync", "-n", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("moved 1.0 -> 1.1"), "{said}");
    assert!(
        said.contains(testing.join("moved").to_str().unwrap()),
        "{said}"
    );
    assert!(said.contains("aports/main/moved/APKBUILD"), "{said}");
    assert!(!said.contains("level"), "{said}");
    assert!(said.contains("1 would be re-converted"), "{said}");
    assert!(!testing.join("moved").exists(), "-n wrote something");
}

// the recipe in the tree has been edited since it was converted -- a filter, a pin, a
// target list that is not the one a conversion writes. a bump that dropped those would
// quietly undo every fix the package ever needed
#[test]
fn sync_writes_the_bump_and_carries_what_the_tree_added() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncwrite");
    offer(&repo, "moved", "1.0");
    fs::write(repo.join("moved/targets"), "x86_64-musl x86_64-gnu\n").unwrap();
    fs::write(repo.join("moved/filter"), "filter-lto\n").unwrap();
    fs::write(repo.join("moved/pin"), "alpine/main\n").unwrap();
    aport(&root, "main", "moved", "pkgname=moved\npkgver=1.1\npkgrel=0\npackage() {\n\t:\n}\n");

    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let at = testing.join("moved");

    assert_eq!(fs::read_to_string(at.join("version")).unwrap(), "1.1 0\n");
    // a conversion always says x86_64-musl, so the recipe being bumped is the one that
    // knows what it builds for
    assert_eq!(
        fs::read_to_string(at.join("targets")).unwrap(),
        "x86_64-musl x86_64-gnu\n"
    );
    assert_eq!(fs::read_to_string(at.join("filter")).unwrap(), "filter-lto\n");
    assert_eq!(fs::read_to_string(at.join("pin")).unwrap(), "alpine/main\n");
    assert!(said.contains("carried"), "{said}");
    assert!(said.contains("1 bumped 0 failed"), "{said}");
}

// build is the one file a bump is entitled to rewrite and a hand is entitled to have
// edited. merging them is not kiry's call, so the tree's own stands, alpine's is put
// somewhere nameable and the bump waits
#[test]
fn sync_names_a_file_the_bump_and_a_hand_both_wrote() {
    if !have_busybox() {
        return;
    }
    let (root, repo, _) = tree("syncdiff");
    offer(&repo, "moved", "1.0");
    fs::write(repo.join("moved/build"), "# hand written\nmake\n").unwrap();
    aport(&root, "main", "moved", "pkgname=moved\npkgver=1.1\npkgrel=0\npackage() {\n\t:\n}\n");

    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("held"), "{said}");
    assert!(said.contains("build is this tree's"), "{said}");
    assert!(said.contains("converted/moved/pending"), "{said}");
    let build = fs::read_to_string(repo.join("moved/build")).unwrap();
    assert_eq!(build, "# hand written\nmake\n");
}

// carry keeps this tree's build and takes alpine's sources, so a release tarball becoming
// a git archive leaves ours configuring a tree that has no configure in it. naming the
// file alpine's build is in is not enough -- libfm was held with the file named and
// promoted anyway, and the step it was missing was one line
#[test]
fn a_bump_names_the_prepare_step_ours_does_not_do() {
    if !have_busybox() {
        return;
    }
    let (root, repo, _) = tree("syncprep");
    offer(&repo, "boot", "1.0");
    fs::write(repo.join("boot/build"), "build() {\n\t./configure\n\tmake\n}\n").unwrap();
    aport(
        &root,
        "main",
        "boot",
        "pkgname=boot\npkgver=1.1\npkgrel=0\nprepare() {\n\tdefault_prepare\n\tautoreconf -fi\n}\nbuild() {\n\t./configure\n\tmake\n}\npackage() {\n\t:\n}\n",
    );

    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("alpine's prepare does what ours does not"),
        "{said}"
    );
    assert!(said.contains("+ autoreconf -fi"), "{said}");
    // it heads every converted prepare there is, so reading it back is noise
    assert!(!said.contains("+ default_prepare"), "{said}");
}

// four bumps sat in testing with reasons that were not going to change, and every sync
// re-offered all of them. output you scroll past is output you stop reading
#[test]
fn a_held_version_is_not_re_offered() {
    let (root, repo, testing) = tree("syncheld");
    offer(&repo, "rust", "1.95.0");
    fs::write(repo.join("rust/hold"), "1.98.1\nblocked on llvm 21\n").unwrap();
    aport(&root, "main", "rust", "pkgname=rust\npkgver=1.98.1\npkgrel=0\npackage() {\n\t:\n}\n");

    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("hold 1.98.1  blocked on llvm 21"), "{said}");
    assert!(said.contains("1 held"), "{said}");
    // nothing was converted, so testing never got a copy to promote
    assert!(!testing.join("rust").exists(), "{said}");
}

// a misspelled flag that filters out silently is a flag that did nothing, and the one
// that does nothing here is the one asking for a description instead of the work
#[test]
fn a_flag_the_command_does_not_know_is_refused() {
    let (root, repo, testing) = tree("syncflag");
    offer(&repo, "rust", "1.95.0");
    aport(&root, "main", "rust", "pkgname=rust\npkgver=1.98.1\npkgrel=0\npackage() {\n\t:\n}\n");

    let o = kiry(&["sync", "--root", root.to_str().unwrap(), "--dry"]);
    assert!(!o.status.success(), "--dry was taken for a package name");
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("kiry: no such flag: --dry"), "{said}");
    // the whole point: refusing it means the sync it was meant to describe never ran
    assert!(!testing.join("rust").exists(), "{said}");
}

// the reason was recorded against one version and says nothing about the next, so the
// hold lapses rather than freezing the package for good
#[test]
fn a_hold_lapses_when_upstream_moves_past_it() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncheldpast");
    offer(&repo, "rust", "1.95.0");
    fs::write(repo.join("rust/hold"), "1.98.1\nblocked on llvm 21\n").unwrap();
    aport(&root, "main", "rust", "pkgname=rust\npkgver=1.99.0\npkgrel=0\npackage() {\n\t:\n}\n");

    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    // the note the hold prints, whichever version it names
    assert!(!said.contains("  hold "), "{said}");
    // and the bump actually ran rather than being skipped
    assert!(testing.join("rust/version").is_file(), "{said}");
}

#[test]
fn promote_moves_testing_over_the_recipe_it_replaces() {
    let (root, repo, testing) = tree("promote");
    offer(&repo, "moved", "1.0");
    offer(&testing, "moved", "1.1");

    let o = kiry(&["promote", "--root", root.to_str().unwrap(), "moved"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read_to_string(repo.join("moved/version")).unwrap(),
        "1.1 1\n"
    );
    assert!(!testing.join("moved").exists(), "testing kept a copy");
}

// a name testing is the first to carry has nowhere it came from, so it lands in extra
#[test]
fn promote_puts_a_new_name_in_extra() {
    let (root, repo, testing) = tree("promotenew");
    offer(&testing, "fresh", "1.0");

    let o = kiry(&["promote", "--root", root.to_str().unwrap(), "fresh"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let extra = repo.parent().unwrap().join("extra");
    assert!(extra.join("fresh/version").is_file(), "not in extra");
}

// a toolchain set that moved halfway is broken before it ever reaches a boot
#[test]
fn promote_will_not_move_half_a_group() {
    let (root, repo, testing) = tree("promotegroup");
    offer(&repo, "one", "1.0");
    offer(&repo, "two", "1.0");
    offer(&testing, "one", "1.1");
    fs::write(testing.join("one/group"), "one\ntwo\n").unwrap();

    let o = kiry(&["promote", "--root", root.to_str().unwrap(), "one"]);
    assert!(!o.status.success(), "half a group moved");
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("two is not in testing"), "{said}");
    assert!(testing.join("one").exists(), "it moved anyway");
}

// nineteen recipes in the tree have no upstream and never will. reporting them as
// something kiry could not identify is a non-answer repeated every run
#[test]
fn a_pin_of_none_is_an_answer_rather_than_a_shrug() {
    let (root, repo, _) = tree("pinnone");
    offer(&repo, "mine", "1.0");
    fs::write(repo.join("mine/pin"), "none\n").unwrap();
    // in aports under that very name, and still not tracked: the recipe said so
    aport(&root, "main", "mine", "pkgver=9.9\n");

    let said = ahead_of(&root);
    let line = said.lines().find(|l| l.starts_with("mine ")).unwrap();
    assert_eq!(line.split_whitespace().nth(3), Some("untracked"), "{line}");
    assert!(!line.contains("9.9"), "{line}");
    assert!(said.contains("1 untracked  0 unknown"), "{said}");
}

// pin says which alpine repo, so none says alpine is not where this comes from -- not
// that nothing does. a recipe carrying both is answered by the tracker
#[test]
fn a_tracker_outlives_a_pin_of_none() {
    let (root, repo, _) = tree("pintracker");
    offer(&repo, "tracked", "1.0");
    fs::write(repo.join("tracked/pin"), "none\n").unwrap();
    fs::write(repo.join("tracked/tracker"), "https://example.invalid/v\n").unwrap();

    let said = ahead_of(&root);
    let line = said.lines().find(|l| l.starts_with("tracked ")).unwrap();
    assert_eq!(line.split_whitespace().nth(3), Some("unknown"), "{line}");
    assert!(line.contains("tracker not read"), "{line}");
}

// alpine carries wlroots0.19 and wlroots0.20 side by side, and its llvm is a meta
// package pointing at whichever llvm is current rather than at ours. neither name is
// derivable from the one this tree uses
#[test]
fn a_pin_names_the_aport_where_alpine_spells_it_differently() {
    let (root, repo, _) = tree("pinname");
    offer(&repo, "wlroots", "0.19.3");
    fs::write(repo.join("wlroots/pin"), "alpine/testing/wlroots0.19\n").unwrap();
    aport(&root, "testing", "wlroots0.19", "pkgver=0.19.3\n");
    aport(&root, "community", "wlroots0.20", "pkgver=0.20.1\n");

    let said = ahead_of(&root);
    let line = said.lines().find(|l| l.starts_with("wlroots ")).unwrap();
    assert_eq!(line.split_whitespace().nth(3), Some("ok"), "{line}");
    assert!(line.contains("aports/testing/wlroots0.19"), "{line}");
}

// a pin is a statement about where to look. looking everywhere else after it misses
// would answer a question the recipe did not ask
#[test]
fn a_pin_that_finds_nothing_does_not_go_looking_elsewhere() {
    let (root, repo, _) = tree("pinmiss");
    offer(&repo, "thing", "1.0");
    fs::write(repo.join("thing/pin"), "alpine/main/thing99\n").unwrap();
    aport(&root, "community", "thing", "pkgver=2.0\n");

    let said = ahead_of(&root);
    let line = said.lines().find(|l| l.starts_with("thing ")).unwrap();
    assert_eq!(line.split_whitespace().nth(3), Some("unknown"), "{line}");
    assert!(line.contains("pin names no aport"), "{line}");
    assert!(!line.contains("2.0"), "{line}");
}

#[test]
fn a_recipe_is_reached_by_name_by_repo_and_by_path() {
    let at = scratch("resolve");
    let root = at.join("root");
    let repo = at.join("core");
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();
    offer(&repo, "thing", "1.0");

    let r = root.to_str().unwrap();
    for arg in ["thing", "core/thing", repo.join("thing").to_str().unwrap()] {
        let o = Command::new(KIRY).args(["--root", r, arg]).output().unwrap();
        assert!(o.status.success(), "{arg}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(
            String::from_utf8_lossy(&o.stdout).contains("thing 1.0 1"),
            "{arg}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
    }

    // and the name that is nowhere says where it looked, because the answer is almost
    // always that the repo list is not what you thought
    let o = Command::new(KIRY).args(["--root", r, "absent"]).output().unwrap();
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("absent"), "{said}");
    assert!(said.contains(repo.to_str().unwrap()), "{said}");
}

// a root that has configured nothing still has to find its recipes, or every fresh
// install starts by writing a config file that only ever says the default
#[test]
fn repos_fall_back_to_the_built_in_place_when_nothing_configured() {
    let at = scratch("defaultrepos");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    offer(&root.join("var/db/kiry/extra"), "found", "2.0");

    let o = Command::new(KIRY)
        .args(["--root", root.to_str().unwrap(), "found"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("found 2.0 1"));
}

// local wins and testing loses, which is why the default is a list in that order and
// not whatever readdir hands back
#[test]
fn the_built_in_repos_are_in_precedence_order() {
    let at = scratch("precedence");
    let root = at.join("root");
    let base = root.join("var/db/kiry");
    fs::create_dir_all(&root).unwrap();
    offer(&base.join("local"), "both", "9.9");
    offer(&base.join("extra"), "both", "1.0");

    let o = Command::new(KIRY)
        .args(["--root", root.to_str().unwrap(), "both"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("both 9.9 1"), "{said}");
}

// a root that can actually build, with a repo its recipes are reached by name through
fn workshop(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let at = scratch(name);
    let (root, repo) = (at.join("root"), at.join("extra"));
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return None;
    }
    fs::create_dir_all(&repo).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();
    Some((at, root, repo))
}

// each one installs a file named after itself, so what actually landed can be told apart
fn buildable(at: &Path, repo: &Path, name: &str, deps: &str) {
    let arc = at.join("hello-1.0.tar");
    if !arc.is_file() {
        tarball(at);
    }
    let sum = kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap();
    let d = repo.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.0 1\n").unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("sources"), format!("{}\n", arc.display())).unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();
    fs::write(d.join("depends"), deps).unwrap();
    fs::write(
        d.join("build"),
        format!("mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/{name}\"\n"),
    )
    .unwrap();
}

// die prefixes every line it is handed, so an error carrying two findings is two lines
// that grep rather than one prefixed and one loose
#[test]
fn a_batch_missing_two_dependencies_prints_both_prefixed() {
    let Some((at, root, repo)) = workshop("twodeps") else {
        return;
    };
    buildable(&at, &repo, "solo", "alpha\nbeta\n");
    // b wants them present too, so they are recorded for the build and taken away again
    bare(&root, "alpha", &[]);
    bare(&root, "beta", &[]);
    let r = root.to_str().unwrap();
    let o = kiry(&["b", "--root", r, "solo"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let db = root.join("usr/lib/kiry/db/installed/x86_64-musl");
    for n in ["alpha", "beta"] {
        fs::remove_dir_all(db.join(n)).unwrap();
    }

    let arc = root.join("var/kiry/cache/solo-1.0-1.x86_64-musl.tar.zst");
    let o = kiry(&["i", "--root", r, arc.to_str().unwrap()]);
    assert!(!o.status.success(), "it installed with both deps missing");
    let said = String::from_utf8_lossy(&o.stderr);
    let got: Vec<&str> = said
        .lines()
        .filter(|l| l.starts_with("kiry: solo needs"))
        .collect();
    assert_eq!(got, ["kiry: solo needs alpha", "kiry: solo needs beta"], "{said}");
}

// b took a name and i took a path, so installing anything meant walking the dependency
// tree by hand and pasting cache paths into it one at a time
#[test]
fn install_by_name_builds_the_closure_in_order() {
    let Some((at, root, repo)) = workshop("byname") else {
        return;
    };
    // named in the order that comes out wrong if nothing sorts them
    buildable(&at, &repo, "top", "mid\n");
    buildable(&at, &repo, "mid", "base\n");
    buildable(&at, &repo, "base", "");

    let o = kiry(&["i", "--root", root.to_str().unwrap(), "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for n in ["base", "mid", "top"] {
        assert!(root.join("usr/bin").join(n).is_file(), "{n} did not land");
    }

    // and in that order. mid builds against an installed base, so a level that went in
    // late is one the level above it could not have linked to
    let said = String::from_utf8_lossy(&o.stdout);
    let seen = |n: &str| said.find(n).unwrap_or_else(|| panic!("no {n}: {said}"));
    assert!(seen("base") < seen("mid"), "{said}");
    assert!(seen("mid") < seen("top"), "{said}");
}

// b always builds and i takes what is there, which is the split that keeps i from
// spending fifty minutes on llvm because a comment moved
#[test]
fn install_by_name_takes_what_is_already_built() {
    let Some((at, root, repo)) = workshop("bycache") else {
        return;
    };
    buildable(&at, &repo, "base", "");
    let r = root.to_str().unwrap();

    let o = kiry(&["b", "--root", r, "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let o = kiry(&["i", "--root", r, "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("base 1.0 x86_64-musl cached"), "{said}");
    assert!(root.join("usr/bin/base").is_file());

    let o = kiry(&["i", "--root", r, "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("base 1.0 x86_64-musl already installed"),
        "{said}"
    );
}

// the walk stops at what is in. depends carry no version constraint, so an installed
// package satisfies the edge and there is nothing under it left to plan
#[test]
fn install_by_name_leaves_an_installed_dependency_alone() {
    let Some((at, root, repo)) = workshop("byinstalled") else {
        return;
    };
    buildable(&at, &repo, "top", "base\n");
    buildable(&at, &repo, "base", "");
    let r = root.to_str().unwrap();
    assert!(kiry(&["i", "--root", r, "base"]).status.success());

    let o = kiry(&["i", "--root", r, "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(!said.contains("base"), "base was planned again: {said}");
}

// identity comes out of the sidecar. the file name is a display name and a version can
// contain the same - that would separate its fields
fn artifact(root: &Path, name: &str, up: &str, rev: u32) {
    let c = root.join("var/kiry/cache");
    fs::create_dir_all(&c).unwrap();
    let at = c.join(format!("{name}-{up}-{rev}.x86_64-musl.tar.zst"));
    fs::write(&at, "x").unwrap();
    let m = PathBuf::from(format!("{}.meta", at.display()));
    fs::create_dir_all(&m).unwrap();
    fs::write(m.join("name"), format!("{name}\n")).unwrap();
    fs::write(m.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(m.join("version"), format!("{up} {rev}\n")).unwrap();
    // written in order and read back by mtime, so two landing in the same tick would
    // make which of them is newest a coin toss
    std::thread::sleep(std::time::Duration::from_millis(5));
}

// nothing else in kiry deletes anything, so what this has to get right is not what it
// removes but what it does not
#[test]
fn gc_keeps_what_is_installed_the_two_newest_and_both_libcs() {
    let (root, repo, _) = tree("gckeep");
    offer(&repo, "foo", "4.0");
    for v in ["1.0", "2.0", "3.0", "4.0"] {
        artifact(&root, "foo", v, 1);
    }
    for v in ["1.0", "2.0", "3.0"] {
        artifact(&root, "musl", v, 1);
    }
    // installed at 1.0, which is nowhere near the two newest
    bare(&root, "foo", &[]);
    let r = root.to_str().unwrap();
    let cache = root.join("var/kiry/cache");
    let there = |n: &str| cache.join(n).exists();

    let o = kiry(&["gc", "-n", "--root", r]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(there("foo-2.0-1.x86_64-musl.tar.zst"), "-n removed it");

    let o = kiry(&["gc", "--root", r]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for n in ["foo-4.0-1", "foo-3.0-1", "foo-1.0-1"] {
        let f = format!("{n}.x86_64-musl.tar.zst");
        assert!(there(&f), "{f} was collected");
        assert!(there(&format!("{f}.meta")), "{f} lost its sidecar");
    }
    assert!(!there("foo-2.0-1.x86_64-musl.tar.zst"), "2.0 stayed");
    assert!(!there("foo-2.0-1.x86_64-musl.tar.zst.meta"), "2.0 meta stayed");
    // the repair case is putting a working libc back with no network
    for v in ["1.0", "2.0", "3.0"] {
        let f = format!("musl-{v}-1.x86_64-musl.tar.zst");
        assert!(there(&f), "{f} was collected");
    }
}

#[test]
fn gc_drops_a_tarball_no_recipe_names() {
    let (root, repo, _) = tree("gcsrc");
    offer(&repo, "foo", "1.0");
    fs::write(
        repo.join("foo/sources"),
        "https://example.invalid/foo-1.0.tar.gz\n",
    )
    .unwrap();
    let src = root.join("var/kiry/cache/sources");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("foo-1.0.tar.gz"), "keep").unwrap();
    fs::write(src.join("foo-0.9.tar.gz"), "drop").unwrap();

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(src.join("foo-1.0.tar.gz").is_file(), "the live one went");
    assert!(!src.join("foo-0.9.tar.gz").exists(), "the dead one stayed");
}

// kiry takes no lock, so a work directory being written right now and one abandoned an
// hour ago look identical. how recently something changed is the only thing between them
#[test]
fn gc_leaves_a_stage_directory_that_is_still_being_written() {
    let (root, repo, _) = tree("gcstage");
    offer(&repo, "foo", "1.0");
    let stage = root.join("var/kiry/stage/foo-1.0-1.x86_64-musl");
    fs::create_dir_all(stage.join("src")).unwrap();

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(stage.is_dir(), "a live build was collected");
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("1 kept back"), "{said}");
}

// held by busybox flock, which is a second process the way a second kiry is. ready
// once the child is inside the lock, so nothing races its start
fn hold(lock: &Path) -> std::process::Child {
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(lock, "").unwrap();
    let ready = lock.with_extension("ready");
    let _ = fs::remove_file(&ready);
    let c = Command::new("busybox")
        .arg("flock")
        .arg(lock)
        .args(["sh", "-c", &format!("touch {} && sleep 30", ready.display())])
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if ready.exists() {
            return c;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("flock never took {}", lock.display());
}

// compile() deletes the stage dir before it starts, so a second build of the same
// package under a first one that is still packing takes the first one's tree with it
#[test]
fn a_second_build_of_a_package_is_refused_while_one_runs() {
    let at = scratch("stage-lock");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let d = recipe(&at.join("repo"), "x86_64-musl", GOOD);
    let mut first = hold(&root.join("var/kiry/stage/hello-1.0-1.lock"));

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    let _ = first.kill();
    let _ = first.wait();
    assert!(!o.status.success(), "a second build ran beside the first");
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("hello is being built already"), "{said}");
}

// a build that has not written anything in six hours is still a build while it holds
// its lock. rust's stage2 goes quiet for long enough
#[test]
fn gc_leaves_a_stage_directory_whose_build_holds_the_lock() {
    let (root, repo, _) = tree("gcstage-lock");
    offer(&repo, "foo", "1.0");
    let stage = root.join("var/kiry/stage/foo-1.0-1.x86_64-musl");
    fs::create_dir_all(stage.join("src")).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
    for d in [stage.join("src"), stage.clone()] {
        fs::File::open(&d).unwrap().set_modified(old).unwrap();
    }
    let lock = root.join("var/kiry/stage/foo-1.0-1.lock");
    let mut build = hold(&lock);
    fs::File::open(&lock).unwrap().set_modified(old).unwrap();
    let _ = fs::remove_file(lock.with_extension("ready"));

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    let _ = build.kill();
    let _ = build.wait();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(stage.is_dir(), "a build holding its lock was collected");
    assert!(lock.is_file(), "a held lock was collected");
}

// a --root with a typo in it reaches no repo at all, and every source and log under it
// then looks like something nothing names
#[test]
fn gc_will_not_decide_anything_with_no_recipes_in_reach() {
    let at = scratch("gcempty");
    let root = at.join("root");
    fs::create_dir_all(root.join("var/kiry/cache/sources")).unwrap();
    fs::write(root.join("var/kiry/cache/sources/keep.tar"), "x").unwrap();

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    assert!(!o.status.success(), "it decided anyway");
    assert!(root.join("var/kiry/cache/sources/keep.tar").is_file());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("no recipes in reach"), "{said}");
}

// the same rule as one_target_failing_cancels_the_other, reached through i rather than
// b. it broke here first: i plans one (name, target) at a time, so musl packed and
// installed before gnu was ever tried
#[test]
fn install_by_name_will_not_install_one_target_without_the_other() {
    let Some((at, root, repo)) = workshop("byatomic") else {
        return;
    };
    buildable(&at, &repo, "two", "");
    fs::write(repo.join("two/targets"), "x86_64-musl x86_64-gnu\n").unwrap();
    fs::write(
        repo.join("two/build"),
        "[ \"$KIRY_TARGET\" = x86_64-musl ] || exit 1\n\
         mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/two\"\n",
    )
    .unwrap();

    let o = kiry(&["i", "--root", root.to_str().unwrap(), "two"]);
    assert!(!o.status.success(), "it installed anyway");
    assert!(artifacts(&root).is_empty(), "{:?}", artifacts(&root));
    assert!(!root.join("usr/bin/two").exists(), "musl landed on its own");
}

// knowing whether to wait or walk away is the whole reason to record a duration, and it
// is a decision made before the build starts rather than after
#[test]
fn a_build_says_what_it_took_and_remembers_for_next_time() {
    let Some((at, root, repo)) = workshop("eta") else {
        return;
    };
    buildable(&at, &repo, "base", "");
    let r = root.to_str().unwrap();

    let o = kiry(&["b", "--root", r, "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    // nothing known yet, so it says so instead of inventing a number
    assert!(said.contains("base 1.0 x86_64-musl building -"), "{said}");

    let kept = fs::read_to_string(root.join("var/kiry/times")).unwrap();
    assert!(kept.starts_with("base 1.0 x86_64-musl "), "{kept}");

    let o = kiry(&["b", "--root", r, "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("base 1.0 x86_64-musl building ~"), "{said}");

    let o = kiry(&["stats", "--root", r]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("world ~"), "{said}");
    assert!(said.contains("slowest base x86_64-musl"), "{said}");
}

#[test]
fn a_batch_says_what_it_will_cost_before_it_starts() {
    let Some((at, root, repo)) = workshop("forecast") else {
        return;
    };
    buildable(&at, &repo, "top", "mid\n");
    buildable(&at, &repo, "mid", "base\n");
    buildable(&at, &repo, "base", "");

    let o = kiry(&["i", "--root", root.to_str().unwrap(), "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let line = said
        .lines()
        .find(|l| l.contains(" builds  ~"))
        .unwrap_or_else(|| panic!("no forecast: {said}"));
    assert!(line.starts_with("3 builds"), "{line}");
    assert!(line.contains("3 never built here"), "{line}");
    // and before any of them, or it is a report rather than an estimate
    let (f, b) = (said.find("builds  ~"), said.find("base 1.0"));
    assert!(f < b, "{said}");
}

// routing every install through the inactive subvolume is right for a libc and absurd
// for a new cli tool. the rule is what decides, and it has to be said either way
#[test]
fn a_new_package_nothing_has_open_goes_live() {
    let Some((at, root, repo)) = workshop("routelive") else {
        return;
    };
    buildable(&at, &repo, "base", "");

    let o = kiry(&["i", "--root", root.to_str().unwrap(), "base"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let line = said
        .lines()
        .find(|l| l.starts_with("route "))
        .unwrap_or_else(|| panic!("nothing said which root: {said}"));
    assert!(line.starts_with("route live"), "{line}");
    // a staging root is not the running one, and /proc answers only for /
    assert!(line.contains("not the running root"), "{line}");
}

// kiry upgrading itself in an early level of a batch: the file it runs from is replaced
// underneath it, and the next level still has to enter its sandbox
#[test]
fn a_batch_still_builds_after_its_own_binary_is_replaced() {
    let Some((at, root, repo)) = workshop("selfswap") else {
        return;
    };
    buildable(&at, &repo, "low", "");
    buildable(&at, &repo, "high", "low\n");
    let me = at.join("kiry");
    fs::copy(KIRY, &me).unwrap();
    let hooks = root.join("etc/kiry/hooks.d");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("10-swap");
    fs::write(&hook, format!("#!/bin/sh\ncp '{0}' '{0}.new' && mv '{0}.new' '{0}'\n", me.display())).unwrap();
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let o = Command::new(&me).args(["i", "--root", root.to_str().unwrap(), "high"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(db::read(&root, "x86_64-musl", "high").is_ok(), "high never went in");
}

// a fresh root has no compiler, and building one needs a compiler. the cached one goes in
// first, whatever flags it was built with, and the build runs on it
#[test]
fn a_root_without_its_toolchain_takes_the_cached_one_first() {
    let Some((at, root, repo)) = workshop("toolfirst") else {
        return;
    };
    buildable(&at, &repo, "cc1", "");
    buildable(&at, &repo, "top", "cc1\n");
    let o = kiry(&["b", "--root", root.to_str().unwrap(), "cc1"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let tc = root.join("etc/kiry/toolchain");
    fs::write(&tc, fs::read_to_string(&tc).unwrap() + "cc1\n").unwrap();

    // -n installs nothing, so its plan has to count the toolchain as already there
    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("cc1 1.0 x86_64-musl goes in first from the cache"), "{said}");
    assert!(!said.contains("cc1 1.0 x86_64-musl cached"), "{said}");
    assert!(db::read(&root, "x86_64-musl", "cc1").is_err(), "a dry run installed cc1");

    let o = kiry(&["i", "--root", root.to_str().unwrap(), "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("cc1 1.0 x86_64-musl goes in first from the cache"), "{said}");
    assert!(db::read(&root, "x86_64-musl", "cc1").is_ok(), "cc1 never went in");
    assert!(db::read(&root, "x86_64-musl", "top").is_ok(), "top never went in");
}

// asking whether something needs a reboot should not be the same act as rebooting for it
#[test]
fn install_dash_n_says_the_plan_and_writes_nothing() {
    let Some((at, root, repo)) = workshop("drynstall") else {
        return;
    };
    buildable(&at, &repo, "top", "base\n");
    buildable(&at, &repo, "base", "");

    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "top"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("top 1.0 x86_64-musl builds"), "{said}");
    assert!(said.contains("base 1.0 x86_64-musl builds"), "{said}");
    assert!(said.contains("route "), "{said}");
    assert!(artifacts(&root).is_empty(), "{:?}", artifacts(&root));
    assert!(!root.join("usr/bin/top").exists(), "it installed anyway");
}

// every core package wants a reboot now, and sometimes you know better. the answer it
// overrules is still printed, because what is worth having in a log later is which one
// was set aside
#[test]
fn dash_live_overrules_the_other_root_and_says_what_it_overruled() {
    let at = scratch("liveover");
    let (root, core) = (at.join("root"), at.join("core"));
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    fs::create_dir_all(&core).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", core.display())).unwrap();
    buildable(&at, &core, "deep", "");
    let r = root.to_str().unwrap();

    // a fixture root is never A/B in the first place, so the override has to be visible
    // on the answer rather than on where the files went
    let o = kiry(&["i", "-n", "--live", "--root", r, "deep"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    let line = said.lines().find(|l| l.starts_with("route ")).unwrap();
    assert!(line.starts_with("route live"), "{line}");

    let o = kiry(&["i", "--live", "--root", r, "deep"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(root.join("usr/bin/deep").is_file(), "it did not install");
}

#[test]
fn the_word_help_reaches_the_usage_and_not_the_recipe_lookup() {
    for w in ["help", "-h", "--help"] {
        let out = kiry(&[w]);
        assert!(out.status.success(), "{w} exited {:?}", out.status.code());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.starts_with("usage: kiry"), "{w} printed {text}");
        assert!(text.contains("build what is missing, then install"));
    }
}

// the real file almost always carries the full version and the soname is a symlink onto
// it, so a bump renames the thing being compared. 356 of the libraries installed here are
// shaped that way, and keying the baseline on the path skipped every one of them
#[test]
fn a_library_whose_file_was_renamed_is_still_compared() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-renamed");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let both = lib_with(
        &at.join("v1"),
        "libp.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );
    let gone = lib_with(&at.join("v2"), "libp.so.1", "void p(void){}\n");

    let first = archive(&at, "ren-1", &[("usr/lib64/libp.so.1.2.3", &both)]);
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    place(
        &root,
        "useq",
        &[(
            "usr/bin/useq",
            &app_calling(&at.join("b"), "useq", "q", &both),
        )],
    );

    let second = archive(&at, "ren-2", &[("usr/lib64/libp.so.1.2.4", &gone)]);
    let o = kiry(&["i", "--root", r, second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| queued_pkg(l) == "useq"),
        "the file moved from libp.so.1.2.3 to libp.so.1.2.4 and q leaving went unseen: {q:?}"
    );
}

// one soname on two files is what a private copy beside a public one looks like. the
// soname cannot tell them apart, so the path is what pairs each build with its own
// predecessor and picking either one would queue a rebuild nothing asked for
#[test]
fn two_libraries_under_one_soname_are_each_compared_against_themselves() {
    if !have_cc() {
        return;
    }
    let at = scratch("abi-twosome");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let both = lib_with(
        &at.join("v1"),
        "libp.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );
    let only = lib_with(&at.join("v2"), "libp.so.1", "void p(void){}\n");

    let first = archive(
        &at,
        "two-1",
        &[
            ("usr/lib64/a/libp.so.1", &both),
            ("usr/lib64/b/libp.so.1", &only),
        ],
    );
    let o = kiry(&["i", "--root", r, first.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    place(
        &root,
        "useq",
        &[(
            "usr/bin/useq",
            &app_calling(&at.join("c"), "useq", "q", &both),
        )],
    );

    // a loses q, b never had it. only the a comparison can say anything happened
    let second = archive(
        &at,
        "two-2",
        &[
            ("usr/lib64/a/libp.so.1", &only),
            ("usr/lib64/b/libp.so.1", &only),
        ],
    );
    let o = kiry(&["i", "--root", r, "--force", second.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let q = queued(&root);
    assert!(
        q.iter().any(|l| queued_pkg(l) == "useq"),
        "a dropped q and the two sonames were not told apart: {q:?}"
    );
}

fn lib_versioned(at: &Path, soname: &str, ver: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{soname}.c"));
    fs::write(
        &src,
        format!("void p_impl(void){{}}\n__asm__(\".symver p_impl,p@@{ver}\");\n"),
    )
    .unwrap();
    let map = at.join("v.map");
    fs::write(&map, format!("{ver} {{ global: p; local: *; }};\n")).unwrap();
    let out = at.join(soname);
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg(format!("-Wl,--version-script,{}", map.display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap()
        .success());
    out
}

// the design's own example: a binary wanting foo@GLIBC_2.40 against a substrate that
// provides 2.38. both halves were indexed the whole time and nothing compared them until
// doctor was run by hand, so the first thing to notice was the program failing to start
#[test]
fn installing_a_binary_that_wants_a_version_nothing_provides_says_so() {
    if !have_cc() {
        return;
    }
    let at = scratch("skew");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let r = root.to_str().unwrap();

    let two = lib_versioned(&at.join("v2"), "libp.so.1", "V2");
    let one = lib_versioned(&at.join("v1"), "libp.so.1", "V1");
    let app = app_calling(&at.join("app"), "useskew", "p", &two);

    // linked against the one that has V2, installed beside the one that has only V1
    let arc = archive(
        &at,
        "skew",
        &[("usr/lib64/libp.so.1", &one), ("usr/bin/useskew", &app)],
    );
    let o = kiry(&["i", "--root", r, arc.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("missing-symbol p@V2"),
        "install said nothing about the version skew it just laid down: {text}"
    );
}

// the file name carries version and rev, and neither of those moves when the thing that
// actually decides the build does. an artifact compiled with the old flags kept being
// handed back as a hit, and kiry flags noticing afterwards was the only thing that did
#[test]
fn a_flag_change_takes_the_cached_artifact_out_of_play() {
    let at = scratch("cache-flags");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let r = root.to_str().unwrap();
    let dir = d.to_str().unwrap();

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let o = kiry(&["i", "--root", r, "-n", dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("hello 1.0 x86_64-musl cached"), "{said}");

    fs::create_dir_all(root.join("etc/kiry/pkg")).unwrap();
    fs::write(root.join("etc/kiry/pkg/hello"), "CFLAGS -O1\n").unwrap();

    let o = kiry(&["i", "--root", r, "-n", dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("rebuilding  flags changed"),
        "the artifact was compiled with other flags and is still on offer: {said}"
    );
}

// editing a recipe without bumping rev is what writing one looks like all day, and the
// artifact from the previous edit sat there under a name that still fit
#[test]
fn an_edited_recipe_takes_the_cached_artifact_out_of_play() {
    let at = scratch("cache-recipe");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let r = root.to_str().unwrap();
    let dir = d.to_str().unwrap();

    assert!(kiry(&["b", "--root", r, dir]).status.success());
    let o = kiry(&["i", "--root", r, "-n", dir]);
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("cached"),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );

    fs::write(d.join("build"), format!("{GOOD}echo second thoughts\n")).unwrap();

    let o = kiry(&["i", "--root", r, "-n", dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("rebuilding  recipe changed"),
        "the build script moved and the old artifact is still on offer: {said}"
    );
}

// training is the expensive half of a pgo build, so the profile outlives the stage dir.
// rm -r is the only retrain there is, and the artifact built on the old profile has to
// notice or it stays on offer as a hit forever
#[test]
fn a_profile_outlives_its_build_and_removing_it_retrains() {
    let at = scratch("profile");
    let script = format!(
        "{GOOD}if [ -n \"$KIRY_PROFILE\" ]; then\n\
         cp \"$KIRY_PROFILE\" \"$DESTDIR/usr/bin/fed\"\n\
         echo 'warning: function control flow change detected (hash mismatch) a'\n\
         echo 'warning: function control flow change detected (hash mismatch) b'\n\
         echo 'warning: function control flow change detected (hash mismatch) main'\n\
         else echo trained > \"$KIRY_PROFILE_OUT\"\n\
         echo 'warning: function control flow change detected (hash mismatch) main'\n\
         fi\n"
    );
    let d = recipe(&at, "x86_64-musl", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let r = root.to_str().unwrap();
    let dir = d.to_str().unwrap();
    let kept = root.join("var/kiry/profiles/hello");
    let prof = kept.join("x86_64-musl/merged.profdata");

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(&prof).unwrap(), "trained\n");
    let made = fs::read_to_string(kept.join("x86_64-musl/generated-for")).unwrap();
    assert!(made.starts_with("1.0 1 clang "), "{made}");
    assert!(made.ends_with(" discarded 1\n"), "{made}");
    let meta = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst.meta");
    let sum = kiry_core::sha256(fs::File::open(&prof).unwrap()).unwrap();
    assert_eq!(fs::read_to_string(meta.join("profile")).unwrap(), format!("{sum}\n"));

    let o = kiry(&["i", "--root", r, "-n", dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("hello 1.0 x86_64-musl cached"), "{said}");

    // the second build is handed the first one's profile and trains nothing
    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("hello x86_64-musl profile out of date for 2 functions"),
        "{said}"
    );

    fs::remove_dir_all(&kept).unwrap();
    let o = kiry(&["i", "--root", r, "-n", dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(
        said.contains("rebuilding  profile changed"),
        "the profile it was built on is gone and the artifact is still on offer: {said}"
    );

    let arc = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst");
    let o = kiry(&["i", "--root", r, arc.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(root.join("usr/bin/fed")).unwrap(), "trained\n");
}

// a root holding one recipe and one installed record of it, with no artifact anywhere:
// what the sets are read off is the db and the tree, and neither needs a build
fn set_root(at: &Path, installed: &str, in_tree: &str) -> PathBuf {
    let root = at.join("root");
    let d = root.join("var/db/kiry/core/hello");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), format!("{in_tree}\n")).unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("sources"), "").unwrap();
    fs::write(d.join("checksums"), "").unwrap();
    fs::write(d.join("build"), "true\n").unwrap();
    db::write(
        &root,
        &db::Installed {
            name: "hello".into(),
            target: "x86_64-musl".into(),
            version: Version::parse(installed).unwrap(),
            depends: Vec::new(),
            manifest: Vec::new(),
            hash: String::new(),
            users: Vec::new(),
            flags: Vec::new(),
        },
    )
    .unwrap();
    root
}

#[test]
fn outdated_names_what_the_tree_has_moved_past() {
    let at = scratch("outdated");
    let root = set_root(&at, "1.0 1", "2.0 1");
    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "@outdated"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("hello 1.0 1 -> 2.0 1"), "{out}");
}

// the same version in both places is not an upgrade, and a set that came back empty is
// nothing to do rather than an error
#[test]
fn outdated_says_nothing_when_the_tree_agrees() {
    let at = scratch("uptodate");
    let root = set_root(&at, "1.0 1", "1.0 1");
    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "@outdated"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!out.contains("hello"), "{out}");
}

#[test]
fn world_names_everything_installed() {
    let at = scratch("world");
    let root = set_root(&at, "1.0 1", "1.0 1");
    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "@world"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("hello"), "{out}");
}

#[test]
fn a_set_that_is_not_one_is_refused() {
    let at = scratch("noset");
    let root = set_root(&at, "1.0 1", "1.0 1");
    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), "@nope"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no set called @nope"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

// a recipe shaped the way a conversion writes one, which is the shape every recipe in
// this tree has and the shape a bump can write its assignments back into
fn converted_offer(repo: &Path, name: &str, version: &str, body: &str) {
    let d = repo.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("build"),
        format!(
            ". /usr/share/kiry/lib.sh\nsrcdir=/src\npkgname=\"{name}\"\npkgver=\"{version}\"\n\
             pkgrel=\"0\"\nsource=\"\"\nbuilddir=\"/src/{name}-{version}\"\n\n\
             build() {{\n{body}}}\n"
        ),
    )
    .unwrap();
    fs::write(d.join("version"), format!("{version} 0\n")).unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    // tree() is core, and a core recipe says what its numbering is or its bumps wait
    fs::write(d.join("scheme"), "major minor patch\n").unwrap();
}

fn upstream_at(root: &Path, name: &str, version: &str, body: &str) {
    aport(
        root,
        "main",
        name,
        &format!("pkgname={name}\npkgver={version}\npkgrel=0\nbuild() {{\n{body}}}\npackage() {{\n\t:\n}}\n"),
    );
}

fn sync_at(root: &Path) -> String {
    let o = kiry(&["sync", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

// the common bump moves a version and nothing else, and the old answer was to hand back
// the whole recipe regenerated so that every decision this tree ever made read as a
// deletion. what alpine changed is the question, and it is answerable
#[test]
fn a_bump_alpine_did_not_touch_promotes_itself() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncquiet");
    converted_offer(&repo, "quiet", "1.0", "\t./configure --without-the-thing\n");
    upstream_at(&root, "quiet", "1.0", "\tmake\n");

    let first = sync_at(&root);
    assert!(first.contains("measured against what alpine has now"), "{first}");

    upstream_at(&root, "quiet", "1.1", "\tmake\n");
    let said = sync_at(&root);
    assert!(said.contains("quiet 1.0 -> 1.1"), "{said}");
    assert!(said.contains("promoted"), "{said}");
    assert!(!testing.join("quiet").exists(), "{said}");

    let build = fs::read_to_string(repo.join("quiet/build")).unwrap();
    assert!(build.contains("--without-the-thing"), "{build}");
    assert!(build.contains("pkgver=\"1.1\""), "{build}");
    assert!(build.contains("builddir=\"/src/quiet-1.1\""), "{build}");
    assert!(!build.contains("make"), "the conversion's body won: {build}");
    assert_eq!(
        fs::read_to_string(repo.join("quiet/version")).unwrap(),
        "1.1 0\n"
    );
}

// and when alpine did move, the bump waits and says which line. the recipe sitting in
// testing is still this tree's own, so promoting it is never the thing that breaks
#[test]
fn a_bump_alpine_moved_is_held_and_names_the_line() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncmoved");
    converted_offer(&repo, "loud", "1.0", "\t./configure --without-the-thing\n");
    upstream_at(&root, "loud", "1.0", "\tmake\n");
    sync_at(&root);

    upstream_at(&root, "loud", "1.1", "\tautoreconf -fi\n\tmake\n");
    let said = sync_at(&root);
    assert!(said.contains("held"), "{said}");
    assert!(said.contains("alpine changed build()"), "{said}");
    assert!(said.contains("+ autoreconf -fi"), "{said}");
    assert!(testing.join("loud").exists(), "{said}");
    assert_eq!(
        fs::read_to_string(repo.join("loud/version")).unwrap(),
        "1.0 0\n"
    );
    // held is about reading, not about the recipe being unusable. what is waiting is
    // this tree's build with the new version in it
    let waiting = fs::read_to_string(testing.join("loud/build")).unwrap();
    assert!(waiting.contains("--without-the-thing"), "{waiting}");
    assert!(waiting.contains("pkgver=\"1.1\""), "{waiting}");
}

// a decision costs one reading, not one per bump forever. promoting is what says the
// conversion was looked at, so the next bump is measured from there
#[test]
fn promoting_a_held_bump_stops_it_being_asked_again() {
    if !have_busybox() {
        return;
    }
    let (root, repo, _) = tree("syncagain");
    converted_offer(&repo, "twice", "1.0", "\t./configure --without-the-thing\n");
    upstream_at(&root, "twice", "1.0", "\tmake\n");
    sync_at(&root);

    upstream_at(&root, "twice", "1.1", "\tautoreconf -fi\n\tmake\n");
    assert!(sync_at(&root).contains("held"));
    let o = kiry(&["promote", "--root", root.to_str().unwrap(), "twice"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    upstream_at(&root, "twice", "1.2", "\tautoreconf -fi\n\tmake\n");
    let said = sync_at(&root);
    assert!(said.contains("promoted"), "{said}");
    assert!(!said.contains("autoreconf"), "it asked twice: {said}");
    assert_eq!(
        fs::read_to_string(repo.join("twice/version")).unwrap(),
        "1.2 0\n"
    );
}

// a bump waiting in testing is the same package at the version alpine has, so a survey
// that walks every repo sees it twice. measuring the bump against its own conversion
// says nothing moved, and promotes something nobody read
#[test]
fn a_bump_already_waiting_does_not_become_its_own_measure() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncself");
    converted_offer(&repo, "twin", "1.0", "\t./configure --without-the-thing\n");
    upstream_at(&root, "twin", "1.1", "\tautoreconf -fi\n\tmake\n");

    assert!(sync_at(&root).contains("held"));
    assert!(testing.join("twin").exists());

    let said = sync_at(&root);
    assert!(said.contains("held"), "it promoted itself unread: {said}");
    assert_eq!(
        fs::read_to_string(repo.join("twin/version")).unwrap(),
        "1.0 0\n"
    );
}

// a recipe's own files live in a subdirectory, a conversion never writes one, and a
// a recipe names its own patches in sources and nowhere else, so a conversion writing
// that file fresh is what unlists them. the build stops being able to fetch what it
// installs and says so only once it runs
#[test]
fn a_local_source_entry_survives_a_bump() {
    if !have_busybox() {
        return;
    }
    let (root, repo, _testing) = tree("synclocal");

    let up = root.join("var/kiry/aports/main/served");
    fs::create_dir_all(&up).unwrap();
    fs::write(up.join("up.patch"), "--- a\n+++ b\n").unwrap();
    let apk = |v: &str| {
        fs::write(
            up.join("APKBUILD"),
            format!(
                "pkgname=served\npkgver={v}\npkgrel=0\nsource=\"up.patch\"\n\
                 build() {{\n\tmake\n}}\npackage() {{\n\t:\n}}\n"
            ),
        )
        .unwrap();
    };
    apk("1.0");

    let d = repo.join("served");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("ours.patch"), "--- c\n+++ d\n").unwrap();
    fs::write(
        d.join("build"),
        ". /usr/share/kiry/lib.sh\nsrcdir=/src\npkgname=\"served\"\npkgver=\"1.0\"\n\
         pkgrel=\"0\"\nsource=\"up.patch ours.patch\"\nbuilddir=\"/src/served-1.0\"\n\n\
         build() {\n\tmake\n}\n",
    )
    .unwrap();
    fs::write(d.join("sources"), "up.patch\nours.patch\n").unwrap();
    fs::write(
        d.join("checksums"),
        "1111111111111111111111111111111111111111111111111111111111111111\n\
         2222222222222222222222222222222222222222222222222222222222222222\n",
    )
    .unwrap();
    fs::write(d.join("version"), "1.0 0\n").unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();

    sync_at(&root);
    apk("1.1");
    let said = sync_at(&root);

    // alpine's build body did not move, so the bump promoted itself and the recipe is
    // back in the repo it came from
    let at = repo.join("served");
    let src = fs::read_to_string(at.join("sources")).unwrap();
    assert!(src.contains("ours.patch"), "{src}\n{said}");
    let sums = fs::read_to_string(at.join("checksums")).unwrap();
    assert_eq!(src.lines().count(), sums.lines().count(), "{src}\n{sums}");
    assert!(sums.contains("22222222"), "{sums}");
    // default_prepare walks the build's own variable, so a sources file naming a patch
    // the assignment does not is a patch that is fetched and never applied
    let build = fs::read_to_string(at.join("build")).unwrap();
    assert!(build.contains("ours.patch"), "{build}");
    assert!(said.contains("kept ours.patch"), "{said}");
}

// a core recipe is written by hand and carries none of the assignments a conversion
// opens with. reading that as a shape change handed the whole recipe to alpine, and
// busybox's defconfig edits and pkgconf's pkg-config symlink went with it
#[test]
fn a_hand_written_build_is_not_replaced_by_the_conversion() {
    if !have_busybox() {
        return;
    }
    let (root, repo, _testing) = tree("synchand");
    let d = repo.join("served");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("build"), "make defconfig\nmake install\n").unwrap();
    fs::write(d.join("version"), "1.0 0\n").unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("scheme"), "major minor patch\n").unwrap();
    upstream_at(&root, "served", "1.0", "\t./configure\n");

    sync_at(&root);
    upstream_at(&root, "served", "1.1", "\t./configure\n");
    let said = sync_at(&root);

    // the version has to have moved, or this passes on a held bump having simply left
    // the repo copy alone
    assert!(said.contains("promoted"), "{said}");
    let at = repo.join("served");
    assert_eq!(fs::read_to_string(at.join("version")).unwrap(), "1.1 0\n");
    let build = fs::read_to_string(at.join("build")).unwrap();
    assert!(build.contains("make defconfig"), "{build}\n{said}");
    assert!(!build.contains("./configure"), "{build}\n{said}");
    assert!(!build.contains("pkgver="), "{build}");
}

// promotion replaces the recipe wholesale. openntpd's nitro-run went that way
#[test]
fn a_recipe_subdirectory_comes_across_a_bump() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncfiles");
    converted_offer(&repo, "served", "1.0", "\tmake\n");
    fs::create_dir_all(repo.join("served/files")).unwrap();
    fs::write(repo.join("served/files/nitro-run"), "#!/bin/sh\nexec served\n").unwrap();
    upstream_at(&root, "served", "1.1", "\tmake\n");

    sync_at(&root);
    // what is waiting in testing is what a promotion moves over the recipe, so a file
    // missing from it is a file the promotion deletes
    let landed = testing.join("served/files/nitro-run");
    assert!(landed.exists(), "the subdirectory was dropped");
    assert_eq!(
        fs::read_to_string(landed).unwrap(),
        "#!/bin/sh\nexec served\n"
    );

    let o = kiry(&["promote", "--root", root.to_str().unwrap(), "served"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        repo.join("served/files/nitro-run").exists(),
        "the promotion deleted it"
    );
}

// the first bump of a package has nothing to measure against, so it wants reading. what
// waits in testing is still this tree's build: promoting it can fail a build, where
// promoting the conversion throws the recipe away and says nothing
#[test]
fn a_first_bump_keeps_this_trees_build_and_says_where_alpines_is() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncfirst");
    converted_offer(&repo, "fresh", "1.0", "\t./configure --without-the-thing\n");
    upstream_at(&root, "fresh", "1.1", "\tmake\n");

    let said = sync_at(&root);
    assert!(said.contains("held"), "{said}");
    assert!(said.contains("build is this tree's"), "{said}");
    assert!(said.contains("converted/fresh/pending"), "{said}");

    let waiting = fs::read_to_string(testing.join("fresh/build")).unwrap();
    assert!(waiting.contains("--without-the-thing"), "{waiting}");
    assert!(waiting.contains("pkgver=\"1.1\""), "{waiting}");
    assert!(!waiting.contains("make"), "the conversion's body won: {waiting}");
    let _ = repo;
}

// what install keeps back when a soname moves, written the way install writes it
fn preserve_record(root: &Path, name: &str, files: &[(&str, &Path)]) {
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
    db::write_preserved(root, "x86_64-gnu", name, &manifest).unwrap();
}

// gc refuses to decide anything with no recipe in reach, which is a mistyped --root
// rather than an empty tree
fn with_repo(at: &Path, root: &Path) {
    let repo = at.join("repo");
    recipe(&repo, "x86_64-gnu", GOOD);
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(
        root.join("etc/kiry/repos"),
        format!("{}\n", repo.display()),
    )
    .unwrap();
}

// the whole point of keeping it: the consumer goes on naming the soname that left until
// something rebuilds it, and until then the library has to be there
#[test]
fn a_preserved_library_something_still_links_is_not_released() {
    if !have_cc() {
        return;
    }
    let at = scratch("gc-keep");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    with_repo(&at, &root);

    let old = lib(&at, "libp.so.1");
    let now = lib(&at, "libp.so.2");
    let b = app(&at, "app", &old);
    place(&root, "libp", &[("usr/lib64/libp.so.2", &now)]);
    place(&root, "app", &[("usr/bin/app", &b)]);
    preserve_record(&root, "libp", &[("usr/lib64/libp.so.1", &old)]);

    let out = kiry(&["gc", "--root", root.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("released"), "{text}");
    assert!(root.join("usr/lib64/libp.so.1").is_file(), "{text}");
    assert!(!db::read_preserved(&root, "x86_64-gnu", "libp")
        .unwrap()
        .is_empty());
}

// and once nothing names it, carrying it forever is how a root fills up with libraries
// no loader will ever open again
#[test]
fn a_preserved_library_nothing_links_is_released() {
    if !have_cc() {
        return;
    }
    let at = scratch("gc-release");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    with_repo(&at, &root);

    let old = lib(&at, "libp.so.1");
    let now = lib(&at, "libp.so.2");
    // the consumer has been rebuilt and names the new soname, which is the drain having
    // done its job
    let b = app(&at, "app", &now);
    place(&root, "libp", &[("usr/lib64/libp.so.2", &now)]);
    place(&root, "app", &[("usr/bin/app", &b)]);
    preserve_record(&root, "libp", &[("usr/lib64/libp.so.1", &old)]);

    let out = kiry(&["gc", "--root", root.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("libp x86_64-gnu libp.so.1 released"), "{text}");
    assert!(!root.join("usr/lib64/libp.so.1").exists(), "{text}");
    assert!(db::read_preserved(&root, "x86_64-gnu", "libp")
        .unwrap()
        .is_empty());
    assert!(root.join("usr/lib64/libp.so.2").is_file());
}

// -n is a question, and one that deletes what it is asked about is not
#[test]
fn gc_dash_n_releases_nothing() {
    if !have_cc() {
        return;
    }
    let at = scratch("gc-dry");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    with_repo(&at, &root);

    let old = lib(&at, "libp.so.1");
    let now = lib(&at, "libp.so.2");
    place(&root, "libp", &[("usr/lib64/libp.so.2", &now)]);
    preserve_record(&root, "libp", &[("usr/lib64/libp.so.1", &old)]);

    kiry(&["gc", "-n", "--root", root.to_str().unwrap()]);
    assert!(root.join("usr/lib64/libp.so.1").is_file());
    assert!(!db::read_preserved(&root, "x86_64-gnu", "libp")
        .unwrap()
        .is_empty());
}

// same soname, different bytes, so it is still an elf gc can group and still not the
// file the record wrote down
fn relib(at: &Path, soname: &str, body: &str) -> PathBuf {
    let d = at.join("other");
    fs::create_dir_all(&d).unwrap();
    let src = d.join(format!("{soname}.c"));
    fs::write(&src, body).unwrap();
    let out = d.join(soname);
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

// the same rule removal uses. a file that no longer hashes to what was written down was
// put there by hand, and gc deleting it would be gc deleting somebody's repair
#[test]
fn a_preserved_library_edited_by_hand_is_left_alone() {
    if !have_cc() {
        return;
    }
    let at = scratch("gc-edited");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    with_repo(&at, &root);

    let old = lib(&at, "libp.so.1");
    let now = lib(&at, "libp.so.2");
    place(&root, "libp", &[("usr/lib64/libp.so.2", &now)]);
    preserve_record(&root, "libp", &[("usr/lib64/libp.so.1", &old)]);

    // nothing links libp.so.1, so the only thing between it and gc is the hash
    let swapped = relib(&at, "libp.so.1", "void p(void){}\nvoid q(void){}\n");
    fs::copy(&swapped, root.join("usr/lib64/libp.so.1")).unwrap();

    let out = kiry(&["gc", "--root", root.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("released"), "{text}");
    assert!(root.join("usr/lib64/libp.so.1").is_file());
    assert!(!db::read_preserved(&root, "x86_64-gnu", "libp")
        .unwrap()
        .is_empty());
}

// extra carries all of aports. asked about nothing in particular, a bump for a recipe
// nobody installed is a tarball fetched and a conversion written for nothing, and after
// the import that was nine thousand of them
#[test]
fn a_bare_ahead_leaves_out_what_extra_carries_and_nothing_installed() {
    let (root, repo, _) = tree("catalogue");
    let extra = repo.parent().unwrap().join("extra");
    offer(&repo, "kept", "1.0");
    offer(&extra, "used", "1.0");
    offer(&extra, "offered", "1.0");
    for n in ["kept", "used", "offered"] {
        aport(&root, "main", n, "pkgver=1.1\n");
    }
    record(&root, "used", &[], Vec::new());

    let said = ahead_of(&root);
    let rows: Vec<&str> = said
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| ["kept", "used", "offered"].contains(n))
        .collect();
    assert_eq!(rows, ["kept", "used"], "{said}");

    // named, it is a question about that recipe and it gets an answer
    let o = kiry(&["ahead", "--root", root.to_str().unwrap(), "offered"]);
    let named = String::from_utf8_lossy(&o.stdout);
    assert!(named.contains("offered 1.0"), "{named}");
}

// a recipe converted without fetching carries alpine's sha512, and the first fetch is
// what checks it. the length says which hash a line is
#[test]
fn a_sha512_checksum_is_checked_as_one() {
    let at = scratch("sha512");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let sum = kiry_core::sha512(fs::File::open(at.join("hello-1.0.tar")).unwrap()).unwrap();
    assert_eq!(sum.len(), 128);
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(!err.contains("no checksum"), "{err}");
    assert_eq!(artifacts(&root), ["hello-1.0-1.x86_64-musl.tar.zst"]);
}

#[test]
fn a_wrong_sha512_stops_the_build() {
    let at = scratch("sha512-wrong");
    let d = recipe(&at, "x86_64-musl", GOOD);
    fs::write(d.join("checksums"), format!("{}\n", "0".repeat(128))).unwrap();
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

// two packages nothing orders between build side by side, and both want the same
// tarball. a shared partial file is two downloads written into one, and the second
// rename finds the file already gone
#[test]
fn two_packages_in_one_level_build_side_by_side() {
    let one = scratch("wide-one");
    let two = scratch("wide-two");
    let a = recipe(&one, "x86_64-musl", GOOD);
    let b = recipe(&two, "x86_64-musl", GOOD);
    let b2 = two.join("hullo");
    fs::rename(&b, &b2).unwrap();
    fs::write(
        b2.join("build"),
        "mkdir -p \"$DESTDIR/usr/bin\"\ncp greeting \"$DESTDIR/usr/bin/hullo\"\n",
    )
    .unwrap();
    for d in [&a, &b2] {
        fs::write(d.join("sources"), "hello-1.0.tar::https://example.invalid/hello-1.0.tar\n").unwrap();
    }
    // slow enough that both fetches are in flight before either one renames
    let fetch = one.join("fetch.sh");
    fs::write(
        &fetch,
        format!("#!/bin/sh\nsleep 0.5\ncp {} \"$1\"\n", one.join("hello-1.0.tar").display()),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fetch).status().unwrap();

    let root = one.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), a.to_str().unwrap(), b2.to_str().unwrap()])
        .env("KIRY_PARALLEL", "2")
        .env("KIRY_FETCH", format!("{} %o %u", fetch.display()))
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{said}{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(root.join("usr/bin/hello")).unwrap(), "hi\n");
    assert_eq!(fs::read_to_string(root.join("usr/bin/hullo")).unwrap(), "hi\n");
    let parts = fs::read_dir(root.join("var/kiry/cache/sources"))
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().contains(".part")).count())
        .unwrap_or(0);
    assert_eq!(parts, 0);
}

// qt marks Gui an optional cmake component, so qt_build_repo() can configure, build
// nothing and exit 0 -- two seconds and an ok line for a package that installed no files
#[test]
fn a_build_that_stages_nothing_is_not_ok() {
    let at = scratch("empty");
    let d = recipe(&at, "x86_64-musl", "echo configured\necho built\n");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("staged nothing"), "{said}");
    assert!(artifacts(&root).is_empty());
}

// RLIMIT_DATA is per process, so the cap has to know how many builds hold one: two
// builds each allowed half the machine is the whole machine. only this side of the exec
// knows the number, so it crosses by name the way KIRY_MEM does
#[test]
fn the_memory_cap_is_split_between_the_builds_of_a_level() {
    let one = scratch("share-one");
    let two = scratch("share-two");
    let say = |f: &str| {
        format!("mkdir -p \"$DESTDIR/usr/bin\"\nprintf %s \"$KIRY_SHARE\" > \"$DESTDIR/usr/bin/{f}\"\n")
    };
    let a = recipe(&one, "x86_64-musl", &say("hello"));
    let b = recipe(&two, "x86_64-musl", &say("hullo"));
    let b2 = two.join("hullo");
    fs::rename(&b, &b2).unwrap();

    let root = one.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    // nothing orders these two, so they are one level and it is two wide
    let o = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), a.to_str().unwrap(), b2.to_str().unwrap()])
        .env("KIRY_PARALLEL", "2")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(root.join("usr/bin/hello")).unwrap(), "2");
    assert_eq!(fs::read_to_string(root.join("usr/bin/hullo")).unwrap(), "2");

    // and one on its own keeps the whole share, however wide the setting says
    let solo = scratch("share-solo");
    let c = recipe(&solo, "x86_64-musl", &say("only"));
    let c2 = solo.join("only");
    fs::rename(&c, &c2).unwrap();
    let o = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), c2.to_str().unwrap()])
        .env("KIRY_PARALLEL", "2")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(root.join("usr/bin/only")).unwrap(), "1");
}

// bmake reads MAKEFLAGS and has no -l, so a flag only gnu make knows is a lowdown that
// stops at its usage line. the number moves with the builds of the level, same as the
// memory share, and neither is the configured width when the level is one package wide
#[test]
fn make_is_told_the_jobs_of_a_level_and_nothing_gnu_only() {
    let one = scratch("mf-one");
    let two = scratch("mf-two");
    let say = |f: &str| {
        format!("mkdir -p \"$DESTDIR/usr/bin\"\nprintf %s \"$MAKEFLAGS\" > \"$DESTDIR/usr/bin/{f}\"\n")
    };
    let a = recipe(&one, "x86_64-musl", &say("pair"));
    let b = recipe(&two, "x86_64-musl", &say("pear"));
    let b2 = two.join("pear");
    fs::rename(&b, &b2).unwrap();

    let root = one.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), a.to_str().unwrap(), b2.to_str().unwrap()])
        .env("KIRY_PARALLEL", "2")
        .env("KIRY_JOBS", "8")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for f in ["pair", "pear"] {
        let got = fs::read_to_string(root.join("usr/bin").join(f)).unwrap();
        assert_eq!(got, "-j4", "two at once split eight jobs wrong");
    }

    // alone it gets the machine, and still nothing bmake would quit over
    let solo = scratch("mf-solo");
    let c = recipe(&solo, "x86_64-musl", &say("plum"));
    let c2 = solo.join("plum");
    fs::rename(&c, &c2).unwrap();
    let o = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), c2.to_str().unwrap()])
        .env("KIRY_PARALLEL", "2")
        .env("KIRY_JOBS", "8")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read_to_string(root.join("usr/bin/plum")).unwrap(), "-j8");
}

// a bare "no recipe java-jre" does not say it came from prismlauncher or that it is a
// name an aliases file can answer, and both are what the next step needs
#[test]
fn a_missing_dependency_is_named_with_what_wanted_it() {
    let at = scratch("missing-dep");
    let d = recipe(&at, "x86_64-musl", GOOD);
    fs::write(d.join("depends"), "java-jre\n").unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("hello depends on java-jre"), "{err}");
    assert!(err.contains("aliases"), "{err}");
}

// an upstream that moved costs a second fetch, from where alpine keeps the same bytes,
// when the recipe carries alpine's sha512 to prove them
#[test]
fn a_dead_upstream_falls_back_to_a_copy_the_sha512_vouches_for() {
    let at = scratch("mirror");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let tar = at.join("hello-1.0.tar");
    let mirror = at.join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    fs::copy(&tar, mirror.join("hello-1.0.tar")).unwrap();
    // port 9 refuses at once, which is what a dead host looks like without a network
    fs::write(d.join("sources"), "hello-1.0.tar::http://127.0.0.1:9/hello-1.0.tar\n").unwrap();
    let sum = kiry_core::sha512(fs::File::open(&tar).unwrap()).unwrap();
    fs::write(d.join("checksums"), format!("{sum}\n")).unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }

    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_MIRROR", format!("file://{}/nothing-here file://{}", at.display(), mirror.display()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains(&format!("hello-1.0.tar from file://{}/hello-1.0.tar", mirror.display())), "{said}");
}

// a sha256 is kiry's own hash of what it fetched the first time, and no mirror of
// alpine's has any reason to hold those bytes
#[test]
fn a_recipe_of_our_own_does_not_go_looking_elsewhere() {
    let at = scratch("no-mirror");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let mirror = at.join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    fs::copy(at.join("hello-1.0.tar"), mirror.join("hello-1.0.tar")).unwrap();
    fs::write(d.join("sources"), "hello-1.0.tar::http://127.0.0.1:9/hello-1.0.tar\n").unwrap();
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let o = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .env("KIRY_MIRROR", format!("file://{}", mirror.display()))
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stdout).contains(" from file://"));
}


fn have_rsync() -> bool {
    if Command::new("rsync").arg("--version").output().is_ok_and(|o| o.status.success()) {
        return true;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "no rsync to mirror a gentoo md5-cache with"
    );
    false
}

fn entry(at: &Path, cat: &str, pf: &str, body: &str) {
    let d = at.join("md5-cache").join(cat);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(pf), body).unwrap();
}

fn repo_recipe(root: &Path, repo: &str, name: &str, version: &str) -> PathBuf {
    let d = root.join("var/db/kiry").join(repo).join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), format!("{version} 1\n")).unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("build"), "true\n").unwrap();
    d
}

// exactly the version the recipe builds, keyworded for amd64, its highest revision. an
// extra recipe gets one wherever gentoo has that; a hand-kept one only where someone
// asked by putting a file there
#[test]
fn sync_writes_the_gentoo_entry_for_exactly_this_version() {
    if !have_rsync() {
        return;
    }
    let at = scratch("gentoo-sync");
    let root = at.join("root");
    let foot = repo_recipe(&root, "extra", "foot", "1.23.1");
    let old = repo_recipe(&root, "extra", "mako", "1.9.0");
    let mine = repo_recipe(&root, "core", "libdrm", "2.4.133");
    entry(&at, "gui-apps", "foot-1.23.1", "IUSE=X\nKEYWORDS=amd64\n");
    entry(&at, "gui-apps", "foot-1.23.1-r1", "IUSE=+wayland X\nKEYWORDS=~amd64 ~arm64\n");
    entry(&at, "gui-apps", "foot-1.23.1-r2", "IUSE=\nKEYWORDS=~arm64\n");
    entry(&at, "gui-apps", "mako-1.10.0", "IUSE=\nKEYWORDS=~amd64\n");
    entry(&at, "x11-libs", "libdrm-2.4.133", "IUSE=\nKEYWORDS=amd64\n");
    // a virtual and an account sharing the name are not a second candidate
    entry(&at, "virtual", "foot-1", "IUSE=\nKEYWORDS=amd64\n");
    entry(&at, "acct-user", "foot-0", "IUSE=\nKEYWORDS=amd64\n");

    let o = Command::new(KIRY)
        .args(["sync", "--root", root.to_str().unwrap()])
        .env("KIRY_GENTOO", format!("{}/md5-cache/", at.display()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let got = fs::read_to_string(foot.join("gentoo")).unwrap();
    assert!(got.starts_with("# gui-apps/foot-1.23.1-r1\n"), "{got}");
    assert!(got.contains("IUSE=+wayland X"), "{got}");
    assert!(!old.join("gentoo").exists());
    assert!(!mine.join("gentoo").exists());
}

// the whole mechanism in one build: a flag turned off, and the library it named is not
// in the namespace the build runs in
#[test]
fn a_flag_turned_off_takes_its_dependency_out_of_the_build() {
    let at = scratch("flag-off");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let h = root.join("usr/include/X11/Xlib.h");
    fs::create_dir_all(h.parent().unwrap()).unwrap();
    fs::write(&h, "int x;\n").unwrap();
    record(
        &root,
        "libx11",
        &[],
        vec![db::Entry {
            mode: 0o644,
            kind: db::Kind::File(kiry_core::sha256(fs::File::open(&h).unwrap()).unwrap()),
            path: "usr/include/X11/Xlib.h".into(),
        }],
    );
    db::write_provides(&root, "x86_64-musl", "libx11", &[]).unwrap();

    let d = recipe(
        &at,
        "x86_64-musl",
        &format!("if [ -e /usr/include/X11/Xlib.h ]; then echo x11=yes; else echo x11=no; fi\n{GOOD}"),
    );
    fs::write(d.join("depends"), "libx11\n").unwrap();
    fs::write(d.join("gentoo"), "# gui-apps/hello-1.0\nIUSE=X\nRDEPEND=X? ( x11-libs/libX11 )\n").unwrap();

    let run = || {
        let o = kiry(&["b", "-v", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert!(run().contains("x11=yes"));
    config(&root, None, "flags -X\n");
    assert!(run().contains("x11=no"));
}

// a header for another version is what a bump carried across, and it counts as no file
#[test]
fn an_entry_for_another_version_means_no_flags() {
    let root = scratch("stale-gentoo");
    let d = repo_recipe(&root, "extra", "foot", "1.24.0");
    fs::write(d.join("gentoo"), "# gui-apps/foot-1.23.1\nIUSE=X\n").unwrap();
    config(&root, None, "flags -X\n");

    let said = resolved(&root, "foot");
    assert!(!said.contains("resolved FLAGS"), "{said}");
    assert!(said.contains("gentoo gui-apps/foot-1.23.1 is for another version"), "{said}");
}

// sync writing an entry is not the recipe changing: without flags set it cannot move a
// build, and hashing it would have sent two thousand cached artifacts back to be rebuilt
#[test]
fn a_gentoo_entry_arriving_does_not_make_the_cache_stale() {
    let at = scratch("gentoo-hash");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let d = recipe(&at, "x86_64-musl", GOOD);
    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    fs::write(d.join("gentoo"), "# app-misc/hello-1.0\nIUSE=X\n").unwrap();

    let o = kiry(&["i", "-n", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("cached"), "{said}");
    assert!(!said.contains("recipe changed"), "{said}");
}

// a flag turned on that brings in a library nothing ends up linking is a flag configure
// never looked at, and the build says so rather than carrying a dependency for nothing
#[test]
fn a_flag_that_links_nothing_says_so() {
    let at = scratch("flag-unlinked");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    record(&root, "libx11", &[], Vec::new());
    db::write_provides(
        &root,
        "x86_64-musl",
        "libx11",
        &[db::Provide {
            soname: "libX11.so.6".into(),
            versioned: false,
            path: "usr/lib/libX11.so.6".into(),
        }],
    )
    .unwrap();
    let d = recipe(&at, "x86_64-musl", GOOD);
    fs::write(d.join("gentoo"), "# app-misc/hello-1.0\nIUSE=X\nRDEPEND=X? ( x11-libs/libX11 )\n").unwrap();
    let lib = root.join("var/db/kiry/extra/libx11");
    fs::create_dir_all(&lib).unwrap();
    fs::write(lib.join("version"), "1.0 1\n").unwrap();
    fs::write(lib.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(lib.join("build"), "true\n").unwrap();
    config(&root, None, "flags X\n");

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("flag brought in libx11 and nothing links libX11.so.6"), "{said}");
}

// the other half: on puts a library the recipe never listed into the namespace, and
// configure's own detection finds it there
#[test]
fn a_flag_turned_on_brings_its_dependency_into_the_build() {
    let at = scratch("flag-on");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let h = root.join("usr/include/X11/Xlib.h");
    fs::create_dir_all(h.parent().unwrap()).unwrap();
    fs::write(&h, "int x;\n").unwrap();
    record(
        &root,
        "libx11",
        &[],
        vec![db::Entry {
            mode: 0o644,
            kind: db::Kind::File(kiry_core::sha256(fs::File::open(&h).unwrap()).unwrap()),
            path: "usr/include/X11/Xlib.h".into(),
        }],
    );
    db::write_provides(&root, "x86_64-musl", "libx11", &[]).unwrap();
    repo_recipe(&root, "extra", "libx11", "1.0");

    let d = recipe(
        &at,
        "x86_64-musl",
        &format!("if [ -e /usr/include/X11/Xlib.h ]; then echo x11=yes; else echo x11=no; fi\n{GOOD}"),
    );
    fs::write(d.join("gentoo"), "# gui-apps/hello-1.0\nIUSE=X\nRDEPEND=X? ( x11-libs/libX11 )\n").unwrap();

    let run = || {
        let o = kiry(&["b", "-v", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert!(run().contains("x11=no"));
    config(&root, None, "flags X\n");
    assert!(run().contains("x11=yes"));
}

// a policy narrower than always holds a bump bigger than it names in testing, for a
// person to promote, and says which rule held it
#[test]
fn a_policy_holds_a_bump_bigger_than_it_allows() {
    if !have_busybox() {
        return;
    }
    let (root, _, testing) = tree("syncpolicy");
    let extra = root.parent().unwrap().join("extra");
    converted_offer(&extra, "careful", "1.0", "\t./configure\n");
    fs::write(extra.join("careful/policy"), "auto=patch\n").unwrap();
    upstream_at(&root, "careful", "1.0", "\tmake\n");
    let o = kiry(&["sync", "--root", root.to_str().unwrap(), "careful"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    upstream_at(&root, "careful", "1.1", "\tmake\n");
    let o = kiry(&["ahead", "--root", root.to_str().unwrap(), "careful"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("minor"), "{}", String::from_utf8_lossy(&o.stdout));
    let o = kiry(&["sync", "--root", root.to_str().unwrap(), "careful"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("auto=patch holds a minor bump"), "{said}");
    assert!(testing.join("careful").exists(), "{said}");
    assert_eq!(fs::read_to_string(extra.join("careful/version")).unwrap(), "1.0 0\n");
}

// the scheme rule: a core bump nobody has said the numbering of waits in testing
#[test]
fn a_core_bump_without_a_scheme_waits() {
    if !have_busybox() {
        return;
    }
    let (root, repo, testing) = tree("syncscheme");
    converted_offer(&repo, "toolish", "1.0", "\t./configure\n");
    fs::remove_file(repo.join("toolish/scheme")).unwrap();
    upstream_at(&root, "toolish", "1.0", "\tmake\n");
    sync_at(&root);

    upstream_at(&root, "toolish", "1.0.1", "\tmake\n");
    let said = sync_at(&root);
    assert!(said.contains("core recipes promote only with a scheme"), "{said}");
    assert!(testing.join("toolish").exists(), "{said}");
}

// an installed package made of real files, for the prune tests: they are about what the
// index sees, so the records have to point at elf files that exist
fn installed_as(root: &Path, name: &str, files: &[(&str, &Path)], sonames: &[&str]) {
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
    record(root, name, &[], manifest);
    let ps: Vec<db::Provide> = sonames
        .iter()
        .map(|s| db::Provide {
            soname: (*s).to_string(),
            versioned: false,
            path: format!("usr/lib/{s}"),
        })
        .collect();
    db::write_provides(root, "x86_64-musl", name, &ps).unwrap();
}

// a symbol-prune recipe whose "link" is a fake clang handing out one of two real
// libraries: the whole one, or the pruned one when a version script is asked for. the
// sandbox has no compiler, and what is under test is what kiry does around the link
fn prune_fixture(at: &Path, root: &Path) -> (PathBuf, PathBuf) {
    let full = lib_with(
        &at.join("full"),
        "libfoo.so.1",
        "void p(void){}\nvoid q(void){}\nvoid r(void){}\nconst char mark[] = \"WHOLE-LIBRARY\";\n",
    );
    let small = lib_with(&at.join("small"), "libfoo.so.1", "void p(void){}\n");
    let app = app_calling(&at.join("app"), "app", "p", &full);
    installed_as(root, "libfoo", &[("usr/lib/libfoo.so.1", &full)], &["libfoo.so.1"]);
    installed_as(root, "app", &[("usr/bin/app", &app)], &[]);

    let top = at.join("src/libfoo-1.0");
    fs::create_dir_all(&top).unwrap();
    fs::copy(&full, top.join("full.so")).unwrap();
    fs::copy(&small, top.join("small.so")).unwrap();
    let arc = at.join("libfoo-1.0.tar");
    assert!(Command::new("tar")
        .arg("-cf")
        .arg(&arc)
        .arg("-C")
        .arg(at.join("src"))
        .arg("libfoo-1.0")
        .status()
        .unwrap()
        .success());
    let d = at.join("libfoo");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.0 1\n").unwrap();
    fs::write(d.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(d.join("sources"), "../libfoo-1.0.tar\n").unwrap();
    fs::write(d.join("checksums"), format!("{}\n", kiry_core::sha256(fs::File::open(&arc).unwrap()).unwrap())).unwrap();
    fs::write(d.join("symbol-prune"), "").unwrap();
    fs::write(
        d.join("build"),
        "mkdir -p /src/bin \"$DESTDIR/usr/lib\" \"$DESTDIR/usr/share\"\n\
         cat > /src/bin/clang <<'X'\n\
         #!/bin/sh\n\
         o= p= v=\n\
         for a do [ \"$p\" = -o ] && o=$a; case $a in -Wl,--version-script=*) v=${a#*=} ;; esac; p=$a; done\n\
         if [ -n \"$v\" ]; then cp /src/libfoo-1.0/small.so \"$o\"; cp \"$v\" \"$DESTDIR/usr/share/keep\"; else cp /src/libfoo-1.0/full.so \"$o\"; fi\n\
         X\n\
         chmod +x /src/bin/clang\n\
         export PATH=/src/bin:$PATH\n\
         kirycc -shared -o libfoo.so.1 foo.o\n\
         cp libfoo.so.1 \"$DESTDIR/usr/lib/libfoo.so.1\"\n",
    )
    .unwrap();
    (d, full)
}

// only what installed code asks for is exported, and the whole library ships beside the
// pruned one so the next build of anything linking it links against all of it
#[test]
fn a_pruned_library_keeps_what_is_asked_for_and_builds_see_all_of_it() {
    let at = scratch("prune");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) || !have_cc() {
        return;
    }
    let (d, _) = prune_fixture(&at, &root);

    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("pruned libfoo.so.1 4 exports -> 1"), "{said}");
    let art = format!("{}/var/kiry/cache/libfoo-1.0-1.x86_64-musl.tar.zst", root.display());
    let o = kiry(&["i", "--root", root.to_str().unwrap(), &art]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let keep = fs::read_to_string(root.join("usr/share/keep")).unwrap();
    assert!(keep.contains("\"p\";") && !keep.contains("\"q\";"), "{keep}");
    assert!(keep.contains("local: *;"), "{keep}");
    let has = |f: &str| String::from_utf8_lossy(&fs::read(root.join(f)).unwrap()).contains("WHOLE-LIBRARY");
    assert!(has("usr/lib/kiry/prune/libfoo.so.1"));
    assert!(!has("usr/lib/libfoo.so.1"));

    // a build against it is handed the whole one
    let u = recipe(
        &at.join("user"),
        "x86_64-musl",
        &format!("grep -q WHOLE-LIBRARY /usr/lib/libfoo.so.1 && echo whole-here\n{GOOD}"),
    );
    fs::write(u.join("depends"), "libfoo\n").unwrap();
    let o = kiry(&["b", "-v", "--root", root.to_str().unwrap(), u.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("whole-here"));
}

// a soname bump keeps the old library for what has not rebuilt yet, and a build tool
// that has not rebuilt yet runs inside the sandbox too
#[test]
fn a_build_gets_the_libraries_its_closure_still_runs_on() {
    let at = scratch("preserved-staged");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let new = at.join("new.so");
    let old = at.join("old.so");
    fs::write(&new, "generation 2\n").unwrap();
    fs::write(&old, "generation 1\n").unwrap();
    installed_as(&root, "libfoo", &[("usr/lib/libfoo.so.2", &new)], &["libfoo.so.2"]);
    fs::copy(&old, root.join("usr/lib/libfoo.so.1")).unwrap();
    let kept = db::Entry {
        mode: 0o755,
        kind: db::Kind::File(kiry_core::sha256(fs::File::open(&old).unwrap()).unwrap()),
        path: "usr/lib/libfoo.so.1".into(),
    };
    db::write_preserved(&root, "x86_64-musl", "libfoo", &[kept]).unwrap();

    let u = recipe(
        &at.join("user"),
        "x86_64-musl",
        &format!("grep -q 'generation 1' /usr/lib/libfoo.so.1 && echo old-here\n{GOOD}"),
    );
    fs::write(u.join("depends"), "libfoo\n").unwrap();
    let o = kiry(&["b", "-v", "--root", root.to_str().unwrap(), u.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("old-here"));
}

// something installed after the prune that wants what it dropped gets the library
// queued with the symbol, and the next prune keeps it
#[test]
fn a_symbol_pruned_away_and_wanted_later_queues_the_library() {
    let at = scratch("prune-later");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) || !have_cc() {
        return;
    }
    let (d, full) = prune_fixture(&at, &root);
    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let later = app_calling(&at.join("later"), "later", "q", &full);
    installed_as(&root, "later", &[("usr/bin/later", &later)], &[]);
    let art = format!("{}/var/kiry/cache/libfoo-1.0-1.x86_64-musl.tar.zst", root.display());
    let o = kiry(&["i", "--root", root.to_str().unwrap(), &art]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("libfoo x86_64-musl was pruned of q, which later needs  queued"), "{said}");
    let q = db::read_queue(&root).unwrap();
    assert!(
        q.iter().any(|r| r.name == "libfoo" && r.soname == "libfoo.so.1" && r.changed == ["q"]),
        "{:?}",
        q.iter().map(|r| (&r.name, &r.soname, &r.changed)).collect::<Vec<_>>()
    );

    // and the next prune keeps it
    let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = kiry(&["i", "--root", root.to_str().unwrap(), &art]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let keep = fs::read_to_string(root.join("usr/share/keep")).unwrap();
    assert!(keep.contains("\"p\";") && keep.contains("\"q\";"), "{keep}");
}

// the three cases where pruning cannot be decided, each left whole with the reason said
#[test]
fn a_prune_that_cannot_be_decided_leaves_the_library_whole() {
    let at = scratch("prune-no");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) || !have_cc() {
        return;
    }
    let (d, full) = prune_fixture(&at, &root);
    let build = || {
        let o = kiry(&["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };

    // something that can dlopen has it in what it loads
    let src = at.join("loader.c");
    fs::write(&src, "void p(void);\nvoid *dlopen(const char *, int);\nvoid _start(void){p();dlopen(0,0);}\n").unwrap();
    let loader = at.join("loader");
    assert!(Command::new("cc")
        .args(["-nostdlib", "-Wl,--unresolved-symbols=ignore-all", "-o"])
        .arg(&loader)
        .arg(&src)
        .arg(&full)
        .status()
        .unwrap()
        .success());
    installed_as(&root, "loader", &[("usr/bin/loader", &loader)], &[]);
    let said = build();
    assert!(said.contains("not pruned: libfoo can dlopen") || said.contains("can dlopen, and libfoo.so.1 is in what it loads"), "{said}");
    db::forget(&root, "x86_64-musl", "loader").unwrap();

    // versioned: an anonymous script cannot sit beside named versions
    db::write_provides(
        &root,
        "x86_64-musl",
        "libfoo",
        &[db::Provide {
            soname: "libfoo.so.1".into(),
            versioned: true,
            path: "usr/lib/libfoo.so.1".into(),
        }],
    )
    .unwrap();
    assert!(build().contains("not pruned: it versions its symbols"));

    // never installed: nothing says what uses it
    db::forget(&root, "x86_64-musl", "libfoo").unwrap();
    assert!(build().contains("not pruned: not installed yet"));
}

// the test sandbox has no compiler, so the toolchain comes from the closure like any
// other: a clang that prints the layout dump a header carries in //D lines
fn layout_toolchain(at: &Path, root: &Path) {
    let f = at.join("fake-clang");
    fs::write(
        &f,
        "#!/bin/sh\nf= p=\nfor a do [ \"$p\" = -include ] && f=$a; p=$a; done\n\
         [ -n \"$f\" ] && sed -n 's#^//D ##p; s#^//D$##p' \"$f\"\nexit 0\n",
    )
    .unwrap();
    installed_as(root, "clang-fake", &[("usr/bin/clang", &f)], &[]);
}

fn layout_recipe(at: &Path, version: &str, header: &str) -> PathBuf {
    let d = recipe(
        at,
        "x86_64-musl",
        &format!(
            "mkdir -p \"$DESTDIR/usr/include\"\ncat > \"$DESTDIR/usr/include/bar.h\" <<'H'\n{header}H\n{GOOD}"
        ),
    );
    let bar = at.join("libbar");
    let _ = fs::remove_dir_all(&bar);
    fs::rename(&d, &bar).unwrap();
    fs::write(bar.join("version"), format!("{version} 1\n")).unwrap();
    fs::write(bar.join("depends"), "clang-fake make\n").unwrap();
    bar
}

fn bar_h(size: u32, line: u32) -> String {
    format!(
        "//D *** Dumping AST Record Layout\n//D          0 | struct bar\n//D          0 |   int a\n\
         //D            | [sizeof={size}, dsize={size}, align=4,\n//D            |  nvsize={size}, nvalign=4]\n//D\n\
         //D *** Dumping AST Record Layout\n//D          0 | struct (unnamed at /dest/usr/include/bar.h:{line}:1)\n\
         //D          0 |   char c\n//D            | [sizeof=1, dsize=1, align=1,\n//D            |  nvsize=1, nvalign=1]\n//D\n\
         struct bar {{ int a; }};\n"
    )
}

// a struct in a public header grows and nothing in any elf says so. what builds against
// it is queued; a comment moving a line is not a change; and what the same batch built
// against the new header already has it
#[test]
fn a_struct_that_grows_queues_what_builds_against_it() {
    let at = scratch("layout");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    layout_toolchain(&at, &root);
    let r = root.to_str().unwrap();
    let build_and_install = |version: &str, header: &str| -> String {
        let d = layout_recipe(&at, version, header);
        let o = kiry(&["b", "--root", r, d.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let art = format!("{r}/var/kiry/cache/libbar-{version}-1.x86_64-musl.tar.zst");
        let o = kiry(&["i", "--root", r, &art]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };

    build_and_install("1.0", &bar_h(4, 7));
    record(&root, "baruser", &["libbar"], Vec::new());
    db::write_provides(&root, "x86_64-musl", "baruser", &[]).unwrap();

    let said = build_and_install("1.1", &bar_h(4, 12));
    assert!(!said.contains("layout moved"), "{said}");
    assert!(db::read_queue(&root).unwrap().is_empty());

    let said = build_and_install("1.2", &bar_h(8, 12));
    assert!(said.contains("libbar x86_64-musl layout moved for 1 records (struct bar), queued 1"), "{said}");
    let q = db::read_queue(&root).unwrap();
    assert!(q.iter().any(|x| x.name == "baruser" && x.soname == "layout"), "{}", q.len());
}

// two installs into one root that both plan against the same db both say ok, and one
// record comes out empty. the second waits for the first, and says who it waits on
#[test]
fn a_second_writer_waits_for_the_first() {
    let at = scratch("dblock");
    let root = at.join("root");
    let file = at.join("f");
    fs::write(&file, "x\n").unwrap();
    let arc = archive(&at, "foo", &[("usr/share/foo", &file)]);
    let lock = root.join("usr/lib/kiry/db/lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let held = fs::File::create(&lock).unwrap();
    rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();
    fs::write(&lock, "4242\n").unwrap();

    let second = Command::new(KIRY)
        .args(["i", "--root", root.to_str().unwrap(), arc.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let early = db::dir(&root, "x86_64-gnu", "foo").exists();
    drop(held);
    let o = second.wait_with_output().unwrap();
    assert!(!early, "installed while another writer held the db");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("waiting for pid 4242"), "{said}");
    assert!(db::dir(&root, "x86_64-gnu", "foo").exists());
}

// a caller types what they were shown: /bin/hello as often as /usr/bin/hello, with a
// doubled slash or a .. on the way. and a path that is not there is not one nobody owns
#[test]
fn owns_reads_a_path_the_way_the_root_resolves_it() {
    let at = scratch("owns");
    let root = at.join("root");
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    std::os::unix::fs::symlink("usr/bin", root.join("bin")).unwrap();
    fs::write(root.join("usr/bin/hello"), "hi\n").unwrap();
    fs::write(root.join("usr/bin/stray"), "x\n").unwrap();
    let e = db::Entry {
        mode: 0o755,
        kind: db::Kind::File("0".repeat(64)),
        path: "usr/bin/hello".into(),
    };
    record(&root, "hello", &[], vec![e]);
    let r = root.to_str().unwrap();

    for typed in ["/bin/hello", "//usr/./bin/hello", "/usr/share/../bin/hello", "bin//hello"] {
        let o = kiry(&["owns", "--root", r, typed]);
        let said = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success(), "{typed}: {said}");
        assert_eq!(said, "/usr/bin/hello hello x86_64-musl\n", "{typed}");
    }
    let o = kiry(&["owns", "--root", r, "/bin/stray"]);
    assert!(!o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout), "/usr/bin/stray owned by nobody\n");
    let o = kiry(&["owns", "--root", r, "/bin/nope"]);
    assert!(!o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout), "/usr/bin/nope does not exist\n");
}

// perl- starts perl-dbd-mysql's logs too, and the newer of the two must not be the one
// shown
#[test]
fn log_is_for_the_package_named_and_not_one_whose_name_starts_the_same() {
    let root = scratch("logexact");
    let d = root.join("var/kiry/log");
    fs::create_dir_all(&d).unwrap();
    let r = root.to_str().unwrap();
    fs::write(d.join("perl-5.42.0-1.x86_64-musl.log"), "perl itself\n").unwrap();
    fs::write(d.join("foo-1.0-1.x86_64-musl.log"), "foo itself\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(d.join("perl-dbd-mysql-4.050-1.x86_64-musl.log"), "the driver\n").unwrap();
    // a name that ends in a digit of its own looks like foo and a version
    fs::write(d.join("foo-2-1.0-1.x86_64-musl.log"), "foo-2 instead\n").unwrap();

    let said = String::from_utf8_lossy(&kiry(&["log", "--root", r, "perl"]).stdout).into_owned();
    assert!(said.contains("perl itself") && !said.contains("the driver"), "{said}");

    // what the db says is installed settles it
    record(&root, "foo", &[], Vec::new());
    let said = String::from_utf8_lossy(&kiry(&["log", "--root", r, "foo"]).stdout).into_owned();
    assert!(said.contains("foo itself") && !said.contains("foo-2 instead"), "{said}");
}

// the config resolves for any name at all, so a typo would get a confident answer
#[test]
fn flags_for_a_package_nothing_knows_is_an_error() {
    let root = scratch("flagsnone");
    config(&root, None, "OPT -O2\n");
    let o = kiry(&["flags", "--root", root.to_str().unwrap(), "nosuch"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("nosuch"));
}

// r -n is not a package called -n
#[test]
fn r_refuses_a_flag_it_does_not_know() {
    let root = scratch("rflag");
    bare(&root, "one", &[]);
    let o = kiry(&["r", "--root", root.to_str().unwrap(), "-n", "one"]);
    assert!(!o.status.success());
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("no such flag: -n"), "{said}");
    assert!(db::read(&root, "x86_64-musl", "one").is_ok(), "removed it anyway");
}

// a mistyped --root would read as a tree with nothing in it, which is what a clean one
// says
#[test]
fn a_root_that_is_not_there_is_an_error_to_read_too() {
    let at = scratch("noroot");
    let typo = at.join("typo");
    for cmd in ["l", "doctor"] {
        let o = kiry(&[cmd, "--root", typo.to_str().unwrap()]);
        assert!(!o.status.success(), "{cmd}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("no such directory"), "{cmd}");
    }
}

// a pack killed partway leaves its .part behind. one whose build still holds its lock is
// a pack in flight
#[test]
fn gc_drops_a_part_nobody_is_packing() {
    let (root, repo, _) = tree("gcpart");
    offer(&repo, "foo", "1.0");
    let cache = root.join("var/kiry/cache");
    fs::create_dir_all(&cache).unwrap();
    let dead = cache.join("foo-1.0-1.x86_64-musl.tar.zst.part");
    let live = cache.join("bar-2.0-1.x86_64-musl.tar.zst.part");
    fs::write(&dead, "dead").unwrap();
    fs::write(&live, "live").unwrap();
    let mut build = hold(&root.join("var/kiry/stage/bar-2.0-1.lock"));

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    let _ = build.kill();
    let _ = build.wait();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!dead.exists(), "a dead .part stayed");
    assert!(live.exists(), "a pack in flight lost its .part");
}

// kept back is files left on disk, not how many recipes or source names there are
#[test]
fn gc_counts_what_it_keeps_in_files() {
    let (root, repo, _) = tree("gccount");
    for n in ["foo", "bar", "baz"] {
        offer(&repo, n, "1.0");
    }
    fs::write(
        repo.join("foo/sources"),
        "https://example.invalid/foo-1.0.tar.gz\nhttps://example.invalid/foo-extra.tar.gz\n",
    )
    .unwrap();
    let src = root.join("var/kiry/cache/sources");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("foo-1.0.tar.gz"), "keep").unwrap();
    fs::write(src.join("foo-0.9.tar.gz"), "drop").unwrap();
    let log = root.join("var/kiry/log");
    fs::create_dir_all(&log).unwrap();
    fs::write(log.join("foo-1.0-1.x86_64-musl.log"), "keep").unwrap();
    fs::write(log.join("foo-0.9-1.x86_64-musl.log"), "drop").unwrap();
    fs::write(log.join("rebuilds"), "foo x86_64-musl doctor same 1\n").unwrap();

    let o = kiry(&["gc", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    for what in ["sources ", "log "] {
        let l = said.lines().find(|l| l.starts_with(what)).unwrap_or_default();
        assert!(l.ends_with("  1 kept back"), "{what}: {said}");
    }
    assert!(log.join("rebuilds").exists(), "gc took the rebuild log stats reads");
}

// kiry killed under a build must not leave it compiling in its namespace. the loop writes
// beside the build's output, so it stops on its own once that is deleted, whatever the
// verdict
#[test]
fn a_build_dies_with_the_kiry_that_started_it() {
    let at = scratch("orphan");
    let d = recipe(
        &at,
        "x86_64-musl",
        "while echo x >> \"$DESTDIR/beat\"; do sleep 1; done &\nwait\n",
    );
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let work = root.join("var/kiry/stage/hello-1.0-1.x86_64-musl");
    let beat = work.join("dest/beat");
    let mut k = Command::new(KIRY)
        .args(["b", "--root", root.to_str().unwrap(), d.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if beat.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = k.kill();
    let _ = k.wait();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let then = fs::read_to_string(&beat).unwrap_or_default();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let now = fs::read_to_string(&beat).unwrap_or_default();
    let _ = fs::remove_dir_all(&work);
    assert!(!then.is_empty(), "the build never started");
    assert_eq!(then, now, "the build outlived the kiry that started it");
}

// b of a package the cache already holds under the same key is a second build of the same
// thing, so it says whether the bytes came out the same. -q hides it with the ok line
#[test]
fn a_rebuild_under_the_same_key_says_it_made_the_same_bytes() {
    let at = scratch("repro-same");
    // a hardlink comes back out of the tar as a link to whichever name went in first,
    // and the staged tree has two files. both are the same bytes
    let script = format!(
        "{GOOD}ln \"$DESTDIR/usr/bin/hello\" \"$DESTDIR/usr/bin/hi\"\n\
         ln -s hello \"$DESTDIR/usr/bin/hey\"\n"
    );
    let d = recipe(&at, "x86_64-musl", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let (r, dir) = (root.to_str().unwrap(), d.to_str().unwrap());

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(!said.contains("same bytes"), "nothing to compare the first build with: {said}");

    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("hello 1.0 x86_64-musl ok ") && said.contains(" same bytes\n"), "{said}");

    let o = kiry(&["b", "-q", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!String::from_utf8_lossy(&o.stdout).contains("same bytes"));
}

// a file that changes every run is the build not reproducing, and the log says which
#[test]
fn a_rebuild_that_comes_out_different_names_the_file_in_its_log() {
    let at = scratch("repro-differs");
    let script = format!(
        "{GOOD}mkdir -p \"$DESTDIR/usr/share/hello\"\n\
         head -c16 /dev/urandom > \"$DESTDIR/usr/share/hello/stamp\"\n\
         ln -s ../../bin/hello \"$DESTDIR/usr/share/hello/link\"\n"
    );
    let d = recipe(&at, "x86_64-musl", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let (r, dir) = (root.to_str().unwrap(), d.to_str().unwrap());

    assert!(kiry(&["b", "--root", r, dir]).status.success());
    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains(" 1 file differs\n"), "{said}");
    let log = fs::read_to_string(root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log")).unwrap();
    assert!(log.contains("kiry:   content      usr/share/hello/stamp\n"), "{log}");
    assert!(!log.contains("usr/share/hello/link"), "{log}");

    // a name that changes is one file gone and another come
    let at = scratch("repro-renamed");
    let script = format!(
        "{GOOD}mkdir -p \"$DESTDIR/usr/share/hello\"\n\
         n=$(tr -dc a-z </dev/urandom | head -c12)\n\
         touch \"$DESTDIR/usr/share/hello/$n\"\n"
    );
    let d = recipe(&at, "x86_64-musl", &script);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    assert!(bootstrap(&root));
    let (r, dir) = (root.to_str().unwrap(), d.to_str().unwrap());
    assert!(kiry(&["b", "--root", r, dir]).status.success());
    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(said.contains(" 2 files differ\n"), "{said}");
    let log = fs::read_to_string(root.join("var/kiry/log/hello-1.0-1.x86_64-musl.log")).unwrap();
    assert!(log.contains("kiry:   only in old  usr/share/hello/"), "{log}");
    assert!(log.contains("kiry:   only in new  usr/share/hello/"), "{log}");
}

// a rebuild after the recipe, the flags or the sidecar moved is a different build, and
// comparing it with the old one would call a real change irreproducible
#[test]
fn a_rebuild_under_another_key_compares_nothing() {
    let at = scratch("repro-other");
    let d = recipe(&at, "x86_64-musl", GOOD);
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    if !bootstrap(&root) {
        return;
    }
    let (r, dir) = (root.to_str().unwrap(), d.to_str().unwrap());
    let quiet = |said: &str| !said.contains("same bytes") && !said.contains("differ");

    assert!(kiry(&["b", "--root", r, dir]).status.success());
    fs::write(d.join("build"), format!("{GOOD}echo second thoughts\n")).unwrap();
    let o = kiry(&["b", "--root", r, dir]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(quiet(&said), "the recipe moved: {said}");

    fs::create_dir_all(root.join("etc/kiry/pkg")).unwrap();
    fs::write(root.join("etc/kiry/pkg/hello"), "CFLAGS -O1\n").unwrap();
    let o = kiry(&["b", "--root", r, dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(quiet(&said), "the flags moved: {said}");

    // a sidecar from before profiles were recorded cannot say it was this build
    let meta = root.join("var/kiry/cache/hello-1.0-1.x86_64-musl.tar.zst.meta");
    fs::remove_file(meta.join("profile")).unwrap();
    let o = kiry(&["b", "--root", r, dir]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(quiet(&said), "the sidecar says nothing about a profile: {said}");
}
