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
    libsrc(at, soname, "void p(void){}\n")
}

fn libsrc(at: &Path, soname: &str, body: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{soname}.c"));
    fs::write(&src, body).unwrap();
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
    binsrc(
        at,
        name,
        "void p(void);\nvoid _start(void){p();}\n",
        against,
        rpath,
    )
}

fn binsrc(at: &Path, name: &str, body: &str, against: &Path, rpath: Option<&str>) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{name}.c"));
    fs::write(&src, body).unwrap();
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
    installed(root, name, files, &[])
}

fn installed(root: &Path, name: &str, files: &[(&str, &Path)], links: &[(&str, &str)]) {
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
    add_links(root, &mut manifest, links);
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

fn add_links(root: &Path, manifest: &mut Vec<db::Entry>, links: &[(&str, &str)]) {
    for (at, to) in links {
        let dst = root.join(at);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&dst);
        std::os::unix::fs::symlink(to, &dst).unwrap();
        manifest.push(db::Entry {
            kind: db::Kind::Link((*to).to_string()),
            mode: 0o777,
            path: (*at).to_string(),
        });
    }
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

// -r makes ldd resolve every relocation instead of stopping at the libraries, which is
// the question doctor is asking. it reports on stderr
fn ldd_relocs(bin: &Path) -> String {
    let o = Command::new("ldd")
        .args(["-r"])
        .arg(bin)
        .env_remove("LD_LIBRARY_PATH")
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
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

// the case a soname check gets wrong from the other side: the library is installed,
// under the name it always had, in the directory the binary looks in, and the function
// the binary calls is not in it any more
#[test]
fn a_symbol_the_library_dropped_is_reported() {
    if skip("dropped symbol") {
        return;
    }
    let at = scratch("dropped");
    let root = at.join("root");
    let full = libsrc(
        &at.join("was"),
        "libt.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );
    let thin = libsrc(&at.join("now"), "libt.so.1", "void p(void){}\n");
    let app = binsrc(
        &at.join("was"),
        "app",
        "void q(void);\nvoid _start(void){q();}\n",
        &full,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "libt", &[("usr/lib64/libt.so.1", &thin)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "doctor passed a binary whose symbol is gone");
    assert!(
        out.contains(&format!("usr/bin/app {TARGET} missing-symbol q")),
        "{out}"
    );
    assert!(
        !out.contains("unresolved"),
        "the library itself resolves, only the symbol is gone: {out}"
    );

    let ldd = ldd_relocs(&root.join("usr/bin/app"));
    assert!(ldd.contains("undefined symbol: q"), "ldd disagrees: {ldd}");
}

// a weak undefined is allowed to stay undefined. reporting those would bury every real
// finding under the __gmon_start__ in front of it
#[test]
fn a_weak_undefined_symbol_is_not_a_finding() {
    if skip("weak undefined") {
        return;
    }
    let at = scratch("weak");
    let root = at.join("root");
    let l = lib(&at, "libt.so.1");
    let app = binsrc(
        &at,
        "app",
        "void p(void);\n__attribute__((weak)) void q(void);\nvoid _start(void){p();if(q)q();}\n",
        &l,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "libt", &[("usr/lib64/libt.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");
}

// same name, same library, different version node. the symbol is right there and the
// binary still will not start
#[test]
fn a_symbol_that_kept_its_name_and_changed_version_does_not_satisfy() {
    if skip("version moved") {
        return;
    }
    let at = scratch("vers");
    let root = at.join("root");
    let was = versioned_lib(&at.join("was"), "V1");
    let now = versioned_lib(&at.join("now"), "V2");
    let app = binsrc(
        &at.join("was"),
        "app",
        "void p(void);\nvoid _start(void){p();}\n",
        &was,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "libt", &[("usr/lib64/libt.so.1", &now)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "doctor passed a binary asking for a version that left");
    assert!(
        out.contains(&format!("usr/bin/app {TARGET} missing-symbol p@V1")),
        "{out}"
    );

    let ldd = ldd_relocs(&root.join("usr/bin/app"));
    assert!(ldd.contains("V1"), "ldd disagrees: {ldd}");
}

fn versioned_lib(at: &Path, node: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let map = at.join("v.map");
    fs::write(&map, format!("{node} {{ global: p; local: *; }};\n")).unwrap();
    let src = at.join("v.c");
    fs::write(&src, "void p(void){}\n").unwrap();
    let out = at.join("libt.so.1");
    let ok = Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib", "-Wl,-soname,libt.so.1"])
        .arg(format!("-Wl,--version-script,{}", map.display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    out
}

// alpine ships libscudo.so with no DT_SONAME at all and lld asks for it by that name.
// DT_NEEDED names a file, and the loader opens it without ever asking what the library
// calls itself
#[test]
fn a_library_with_no_soname_resolves_by_the_name_asked_for() {
    if skip("no soname") {
        return;
    }
    let at = scratch("nosoname");
    let root = at.join("root");
    let l = nameless(&at, "libt.so.1");
    let app = against_name(&at, "app", &l, "libt.so.1", Some("$ORIGIN/../lib64"));
    install(&root, "libt", &[("usr/lib64/libt.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");
    assert!(ldd_finds(&root.join("usr/bin/app"), "libt.so.1"));
}

// musl reaches its libc through usr/lib/libc.musl-x86_64.so.1, which is a symlink to the
// loader. resolution that stops at regular files calls that root broken
#[test]
fn a_library_reached_through_a_symlink_resolves() {
    if skip("symlinked library") {
        return;
    }
    let at = scratch("symlinked");
    let root = at.join("root");
    let l = nameless(&at, "libt.so.1");
    let app = against_name(&at, "app", &l, "libt.so.1", Some("$ORIGIN/../lib64"));
    install(&root, "libt", &[("usr/lib64/libreal.so.1", &l)]);
    installed(
        &root,
        "app",
        &[("usr/bin/app", &app)],
        &[("usr/lib64/libt.so.1", "libreal.so.1")],
    );

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");
}

// no -Wl,-soname, so the library says nothing about what it is called
fn nameless(at: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{name}.c"));
    fs::write(&src, "void p(void){}\n").unwrap();
    let out = at.join(name);
    let ok = Command::new("cc")
        .args(["-shared", "-fPIC", "-nostdlib"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    out
}

// -l: takes the file name verbatim, which is what lands in DT_NEEDED when the library
// carries no soname of its own
fn against_name(at: &Path, name: &str, lib: &Path, asks: &str, rpath: Option<&str>) -> PathBuf {
    let src = at.join(format!("{name}.c"));
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
        .arg(format!("-L{}", lib.parent().unwrap().display()))
        .arg(format!("-l:{asks}"))
        .status()
        .unwrap();
    assert!(ok.success());
    out
}
