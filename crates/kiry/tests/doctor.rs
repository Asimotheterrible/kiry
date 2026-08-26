// doctor answers the same question ldd does, so ldd is what it gets checked against.
// the fixtures are placed and recorded the way install records them rather than built
// through b: what is under test is linkage, not the build path

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kiry_core::pkg::Version;
use kiry_core::{db, install};

const KIRY: &str = env!("CARGO_BIN_EXE_kiry");
const TARGET: &str = "x86_64-gnu";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .join(format!("kiry-doc-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn skip(why: &str) -> bool {
    let have = |p: &str| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if have("cc") && have("ldd") {
        return false;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "{why}: cc and ldd are what this suite checks against"
    );
    true
}

// -nostdlib keeps libc out of DT_NEEDED, so the only unresolved thing in a fixture root
// is the one the test put there
fn lib(at: &Path, soname: &str) -> PathBuf {
    let src = at.join("p.c");
    fs::write(&src, "void p(void){}\n").unwrap();
    let out = at.join(soname);
    let ok = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-nostdlib",
            &format!("-Wl,-soname,{soname}"),
        ])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    out
}

fn bin(at: &Path, name: &str, against: &Path, rpath: Option<&str>) -> PathBuf {
    let src = at.join("m.c");
    fs::write(&src, "void p(void);\nvoid _start(void){p();}\n").unwrap();
    let out = at.join(name);
    let mut c = Command::new("cc");
    c.arg("-nostdlib");
    if let Some(r) = rpath {
        c.arg(format!("-Wl,-rpath,{r}"));
    }
    let ok = c
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(against)
        .status()
        .unwrap();
    assert!(ok.success());
    out
}

// exactly what apply() records, so a test root and a real one agree
fn install(root: &Path, name: &str, files: &[(&str, &Path)]) {
    let mut manifest = Vec::new();
    for (path, from) in files {
        let dst = root.join(path);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
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
            name: name.to_string(),
            target: TARGET.to_string(),
            version: Version::parse("1.0 1").unwrap(),
            depends: Vec::new(),
            manifest: manifest.clone(),
        },
    )
    .unwrap();

    let provides: Vec<db::Provide> = install::scan(root, &manifest)
        .unwrap()
        .into_iter()
        .filter_map(|(path, o)| {
            let o = o?;
            Some(db::Provide {
                soname: o.soname?,
                versioned: o.versioned,
                path,
            })
        })
        .collect();
    db::write_provides(root, TARGET, name, &provides).unwrap();
}

fn doctor(root: &Path) -> (bool, String) {
    let o: Output = Command::new(KIRY)
        .args(["doctor", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into(),
    )
}

// ldd resolves against the real filesystem, which is what the fixture root is
fn ldd_finds(bin: &Path, soname: &str) -> bool {
    let o = Command::new("ldd")
        .arg(bin)
        .env_remove("LD_LIBRARY_PATH")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .any(|l| l.contains(soname) && !l.contains("not found"))
}

#[test]
fn a_root_whose_linkage_resolves_says_nothing() {
    if skip("clean root") {
        return;
    }
    let at = scratch("clean");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    let b = bin(&at, "app", &l, None);
    install(&root, "libp", &[("usr/lib64/libp.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &b)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");
}

#[test]
fn a_library_nothing_installs_is_unresolved() {
    if skip("missing library") {
        return;
    }
    let at = scratch("missing");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    let b = bin(&at, "app", &l, None);
    install(&root, "app", &[("usr/bin/app", &b)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "doctor passed a root with nothing providing libp.so.1");
    assert!(
        out.contains(&format!("usr/bin/app {TARGET} unresolved libp.so.1")),
        "{out}"
    );
    assert!(!ldd_finds(&root.join("usr/bin/app"), "libp.so.1"));
}

// the case a soname-only check gets wrong: the library IS installed, just nowhere the
// loader would look for it
#[test]
fn a_library_off_the_search_path_does_not_count_as_provided() {
    if skip("search path") {
        return;
    }
    let at = scratch("offpath");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    let bare = bin(&at, "bare", &l, None);
    let rp = bin(&at, "rp", &l, Some("$ORIGIN/../lib64/priv"));
    install(&root, "libp", &[("usr/lib64/priv/libp.so.1", &l)]);
    install(&root, "bare", &[("usr/bin/bare", &bare)]);
    install(&root, "rp", &[("usr/bin/rp", &rp)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "{out}");
    assert!(
        out.contains(&format!("usr/bin/bare {TARGET} unresolved libp.so.1")),
        "the one with no runpath should not resolve: {out}"
    );
    assert!(
        !out.contains("usr/bin/rp "),
        "the one whose runpath points at the library should resolve: {out}"
    );

    // and that is exactly what the loader does with the same two files
    assert!(!ldd_finds(&root.join("usr/bin/bare"), "libp.so.1"));
    assert!(ldd_finds(&root.join("usr/bin/rp"), "libp.so.1"));
}

#[test]
fn a_manifest_file_that_vanished_is_reported_rather_than_skipped() {
    if skip("vanished file") {
        return;
    }
    let at = scratch("vanished");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    install(&root, "libp", &[("usr/lib64/libp.so.1", &l)]);
    fs::remove_file(root.join("usr/lib64/libp.so.1")).unwrap();

    let (ok, out) = doctor(&root);
    assert!(!ok, "a root missing an installed file is not healthy");
    assert!(
        out.contains(&format!("usr/lib64/libp.so.1 {TARGET} unreadable")),
        "{out}"
    );
}

#[test]
fn a_soname_that_moved_out_from_under_the_db_shows_as_stale() {
    if skip("stale provides") {
        return;
    }
    let at = scratch("stale");
    let root = at.join("root");
    let one = lib(&at, "libp.so.1");
    install(&root, "libp", &[("usr/lib64/libp.so.1", &one)]);

    // same path, different soname, which is what a hand swapped library looks like
    let two = lib(&at, "libp.so.2");
    fs::copy(&two, root.join("usr/lib64/libp.so.1")).unwrap();

    let (ok, out) = doctor(&root);
    assert!(!ok, "{out}");
    assert!(
        out.contains(&format!("libp {TARGET} stale-provides")),
        "{out}"
    );
}

// a real root is usrmerged, so a runpath may spell one directory /lib64 and another
// /usr/lib64. the loader follows the symlink; doctor folds the name
#[test]
fn the_usrmerge_spelling_of_a_directory_is_the_same_directory() {
    if skip("usrmerge") {
        return;
    }
    let at = scratch("usrmerge");
    let root = at.join("root");
    fs::create_dir_all(root.join("usr/lib64")).unwrap();
    std::os::unix::fs::symlink("usr/lib64", root.join("lib64")).unwrap();

    let l = lib(&at, "libp.so.1");
    let b = bin(&at, "app", &l, Some("$ORIGIN/../../lib64/priv"));
    install(&root, "libp", &[("usr/lib64/priv/libp.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &b)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "{out}");
    assert!(ldd_finds(&root.join("usr/bin/app"), "libp.so.1"));
}

// --disable-new-dtags still emits DT_RPATH, and plenty of shipped binaries carry one
#[test]
fn dt_rpath_resolves_when_there_is_no_runpath() {
    if skip("dt_rpath") {
        return;
    }
    let at = scratch("rpath");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");

    let src = at.join("m.c");
    fs::write(&src, "void p(void);\nvoid _start(void){p();}\n").unwrap();
    let b = at.join("app");
    let ok = Command::new("cc")
        .args([
            "-nostdlib",
            "-Wl,--disable-new-dtags",
            "-Wl,-rpath,$ORIGIN/../lib64/priv",
            "-o",
        ])
        .arg(&b)
        .arg(&src)
        .arg(&l)
        .status()
        .unwrap();
    assert!(ok.success());

    install(&root, "libp", &[("usr/lib64/priv/libp.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &b)]);

    let raw = Command::new("readelf")
        .args(["-dW"])
        .arg(root.join("usr/bin/app"))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&raw.stdout).contains("(RPATH)"),
        "the linker emitted RUNPATH, so this fixture no longer reaches the rpath branch"
    );

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "{out}");
    assert!(ldd_finds(&root.join("usr/bin/app"), "libp.so.1"));
}

// every other fixture spells the runpath $ORIGIN/.., where the .. cancels the token even
// unexpanded. a library beside its binary is what actually needs $ORIGIN to mean something
#[test]
fn origin_names_the_directory_the_binary_is_in() {
    if skip("origin") {
        return;
    }
    let at = scratch("origin");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    let b = bin(&at, "app", &l, Some("$ORIGIN"));
    install(&root, "libp", &[("usr/bin/libp.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &b)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "{out}");
    assert!(ldd_finds(&root.join("usr/bin/app"), "libp.so.1"));
}
