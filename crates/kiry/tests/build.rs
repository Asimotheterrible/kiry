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

// the seed is this machine's busybox and this machine runs glibc, so a fixture whose
// packages are gnu has a gnu host too. everything else here is musl on musl, where the
// two are the same answer and saying so changes nothing
fn kiry_on(host: &str, args: &[&str]) -> Output {
    Command::new(KIRY)
        .args(args)
        .env("KIRY_HOST", host)
        .output()
        .unwrap()
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
        "head", "install", "patch", "tar", "dd", "true", "false", "sed", "touch",
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

    assert!(kiry(&["doctor", "--root", r]).status.success());
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

// a global flag turned off for one package, without that package restating the set
#[test]
fn a_later_minus_takes_a_flag_back_off() {
    let root = scratch("useflags");
    config(&root, None, "flags x11 wayland vulkan\n");
    config(&root, Some("foot"), "flags -x11\n");

    assert!(
        resolved(&root, "foot").contains("resolved FLAGS vulkan wayland"),
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
#[test]
fn a_failure_nothing_matches_walks_down_the_ladder() {
    let at = scratch("ladder");
    let d = recipe(
        &at,
        "x86_64-musl",
        "case \"$CFLAGS\" in\n\
         *-O3*) echo 'the build is displeased' >&2; exit 1 ;;\n\
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
