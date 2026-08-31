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
    versioned(at, soname, body, None)
}

fn versioned(at: &Path, soname: &str, body: &str, script: Option<&str>) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let src = at.join(format!("{soname}.c"));
    fs::write(&src, body).unwrap();
    let out = at.join(soname);
    let mut c = Command::new("cc");
    c.args([
        "-shared",
        "-fPIC",
        "-nostdlib",
        &format!("-Wl,-soname,{soname}"),
    ]);
    if let Some(v) = script {
        let map = at.join("v.map");
        fs::write(&map, v).unwrap();
        c.arg(format!("-Wl,--version-script,{}", map.display()));
    }
    let ok = c.arg("-o").arg(&out).arg(&src).status().unwrap();
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
    // a library is allowed to leave symbols to whatever loads it, which is the case
    // under test. the linker refusing that would make the fixture unbuildable
    c.args(["-nostdlib", "-Wl,--allow-shlib-undefined"]);
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

// a perl xs module is dlopened, so nothing links it and it is not asked. the library it
// pulls in is linked, but only by something equally unknowable. texinfo ships exactly
// this shape and it made 123 findings out of symbols perl supplies at load time
#[test]
fn a_library_reached_only_through_a_dlopened_one_is_not_asked_either() {
    if skip("chain") {
        return;
    }
    let at = scratch("chain");
    let root = at.join("root");

    // the bottom of the chain leaves q to whatever interpreter loads it
    let deep = libsrc(&at, "libdeep.so.1", "void q(void);\nvoid d(void){q();}\n");
    // the module links it and is itself dlopened, so nothing names the module
    let module = at.join("libmod.so.1");
    let src = at.join("mod.c");
    fs::write(&src, "void d(void);\nvoid m(void){d();}\n").unwrap();
    let ok = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,--allow-shlib-undefined",
            "-Wl,-soname,libmod.so.1",
            "-Wl,-rpath,$ORIGIN",
        ])
        .arg("-o")
        .arg(&module)
        .arg(&src)
        .arg(&deep)
        .status()
        .unwrap();
    assert!(ok.success());

    install(
        &root,
        "chain",
        &[
            ("usr/lib64/libdeep.so.1", &deep),
            ("usr/lib64/libmod.so.1", &module),
        ],
    );
    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");

    // a program that links the module makes the whole chain answerable again
    let app = bin_calling(
        &at,
        "app",
        "void m(void);\nvoid _start(void){m();}\n",
        &module,
    );
    install(&root, "app", &[("usr/bin/app", &app)]);
    let (ok, out) = doctor(&root);
    assert!(!ok, "a chain a program can reach still owes its symbols");
    assert!(
        out.contains(&format!("usr/lib64/libdeep.so.1 {TARGET} missing-symbol q")),
        "{out}"
    );
}

fn bin_calling(at: &Path, name: &str, body: &str, against: &Path) -> PathBuf {
    binsrc(at, name, body, against, Some("$ORIGIN/../lib64"))
}

// a library nothing links can only be reached through dlopen, and then its symbols come
// from whichever process opened it. python ships thousands of such modules and every one
// of them leans on the interpreter for its CPython symbols
#[test]
fn a_library_nothing_links_is_not_asked_about_its_symbols() {
    if skip("plugin") {
        return;
    }
    let at = scratch("plugin");
    let root = at.join("root");
    let plug = libsrc(&at, "libplug.so.1", "void q(void);\nvoid p(void){q();}\n");
    install(&root, "plug", &[("usr/lib64/libplug.so.1", &plug)]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");

    // link something against it and the same symbol becomes a real finding
    let app = bin(&at, "app", &plug, Some("$ORIGIN/../lib64"));
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "a linked library still owes its undefined symbols");
    assert!(
        out.contains(&format!("usr/lib64/libplug.so.1 {TARGET} missing-symbol q")),
        "{out}"
    );
}

// perl installs /usr/bin/perl as a second name for perl5.44.0, and every script it ships
// points at the hardlink. counting only Kind::File called all 32 of them interpreterless
#[test]
fn an_interpreter_reached_by_hardlink_is_there() {
    if skip("hardlink") {
        return;
    }
    let at = scratch("hardlink");
    let root = at.join("root");

    let real = at.join("fictionsh");
    fs::write(&real, "an interpreter\n").unwrap();
    let script = at.join("thing");
    fs::write(&script, "#!/usr/bin/fictionsh\necho hi\n").unwrap();

    let sum = kiry_core::sha256(fs::File::open(&real).unwrap()).unwrap();
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::copy(&real, root.join("usr/bin/fictionsh-1.0")).unwrap();
    fs::hard_link(
        root.join("usr/bin/fictionsh-1.0"),
        root.join("usr/bin/fictionsh"),
    )
    .unwrap();
    db::write(
        &root,
        &db::Installed {
            name: "fictionsh".to_string(),
            target: TARGET.to_string(),
            version: Version::parse("1.0 1").unwrap(),
            depends: Vec::new(),
            manifest: vec![
                db::Entry {
                    mode: 0o755,
                    kind: db::Kind::File(sum),
                    path: "usr/bin/fictionsh-1.0".to_string(),
                },
                db::Entry {
                    mode: 0o755,
                    kind: db::Kind::Hard("usr/bin/fictionsh-1.0".to_string()),
                    path: "usr/bin/fictionsh".to_string(),
                },
            ],
        },
    )
    .unwrap();
    install(&root, "thing", &[("usr/bin/thing", &script)]);

    let (ok, out) = doctor(&root);
    assert!(ok, "a hardlinked interpreter was called missing: {out}");
    assert!(!out.contains("no-interpreter"), "{out}");
}

// the kernel refuses to start a script whose interpreter is not installed, which is the
// same shape of failure as a missing library and nothing was looking for it
#[test]
fn a_script_whose_interpreter_is_missing_is_reported() {
    if skip("shebang") {
        return;
    }
    let at = scratch("shebang");
    let root = at.join("root");
    let script = at.join("thing");
    fs::write(&script, "#!/usr/bin/env fictionsh\necho hi\n").unwrap();
    install(&root, "thing", &[("usr/bin/thing", &script)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "doctor passed a script with no interpreter");
    assert!(
        out.contains(&format!(
            "usr/bin/thing {TARGET} no-interpreter /usr/bin/env fictionsh"
        )),
        "{out}"
    );

    // env takes options of its own before the program name, and -S is common enough
    // that reading the word after env finds a flag rather than an interpreter
    let dashs = at.join("dashs");
    fs::write(&dashs, "#!/usr/bin/env -S fictionsh -u\necho hi\n").unwrap();
    install(&root, "dashs", &[("usr/bin/dashs", &dashs)]);
    let (_, out) = doctor(&root);
    assert!(
        out.contains(&format!("usr/bin/dashs {TARGET} no-interpreter")),
        "{out}"
    );
    assert!(!out.contains("no-interpreter /usr/bin/env -S\n"), "{out}");

    // install the interpreter and the findings go away
    let sh = at.join("fictionsh");
    fs::write(&sh, "an interpreter with no interpreter of its own\n").unwrap();
    install(&root, "fictionsh", &[("usr/bin/fictionsh", &sh)]);
    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "expected silence, got {out:?}");
}

// two libraries exporting one name means whichever loads first wins and the loser's
// callers quietly get the wrong implementation. nothing else on the system says so
#[test]
fn two_libraries_exporting_one_name_are_reported() {
    if skip("duplicate symbols") {
        return;
    }
    let at = scratch("dupes");
    let root = at.join("root");
    let one = libsrc(
        &at.join("one"),
        "libone.so.1",
        "void p(void){}\nvoid q(void){}\n",
    );
    let two = libsrc(&at.join("two"), "libtwo.so.1", "void p(void){}\n");
    // the binary links both, so both are in the namespace where the clash happens
    let app = binsrc(
        &at,
        "app",
        "void p(void);\nvoid q(void);\nvoid _start(void){p();q();}\n",
        &one,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "one", &[("usr/lib64/libone.so.1", &one)]);
    install(&root, "two", &[("usr/lib64/libtwo.so.1", &two)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    // app names libone, so make libtwo reachable the same way anything else would be
    let (ok, out) = doctor(&root);
    if ok {
        // nothing links libtwo yet, so there is no clash to report
        assert!(out.is_empty(), "{out}");
    }

    let both = binsrc(
        &at.join("both"),
        "both",
        "void p(void);\nvoid _start(void){p();}\n",
        &two,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "both", &[("usr/bin/both", &both)]);

    let (ok, out) = doctor(&root);
    assert!(!ok, "doctor passed a root where two libraries export p");
    assert!(
        out.contains(&format!(
            "usr/lib64/libone.so.1 {TARGET} duplicate-symbols 1 usr/lib64/libtwo.so.1"
        )),
        "{out}"
    );
}

// alpine's toolchain leaves _init and _fini in the dynamic symbol table of every object
// it links, so a plain group-by over exports pairs off every library on the system. the
// fixtures here are -nostdlib and carry nothing, which is why the storm only showed up
// against a real root
#[test]
fn the_names_every_object_carries_are_not_a_clash() {
    if skip("duplicate symbols") {
        return;
    }
    let at = scratch("housekeeping");
    let root = at.join("root");
    let body =
        |own: &str| format!("void _init(void){{}}\nvoid _fini(void){{}}\nvoid {own}(void){{}}\n");
    let one = libsrc(&at.join("one"), "libone.so.1", &body("a"));
    let two = libsrc(&at.join("two"), "libtwo.so.1", &body("b"));
    let ua = binsrc(
        &at.join("ua"),
        "ua",
        "void a(void);\nvoid _start(void){a();}\n",
        &one,
        Some("$ORIGIN/../lib64"),
    );
    let ub = binsrc(
        &at.join("ub"),
        "ub",
        "void b(void);\nvoid _start(void){b();}\n",
        &two,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "one", &[("usr/lib64/libone.so.1", &one)]);
    install(&root, "two", &[("usr/lib64/libtwo.so.1", &two)]);
    install(&root, "ua", &[("usr/bin/ua", &ua)]);
    install(&root, "ub", &[("usr/bin/ub", &ub)]);

    let (ok, out) = doctor(&root);
    assert!(!out.contains("duplicate-symbols"), "{out}");
    assert!(ok, "{out}");
}

// libgcc_s exports six names at two versions each. grouping by name alone puts one
// library in its own bucket twice and pairs it with itself, which reads as a clash and
// is one library doing exactly what versioning is for
#[test]
fn one_library_exporting_a_name_at_two_versions_is_not_a_clash() {
    if skip("duplicate symbols") {
        return;
    }
    let at = scratch("twoversions");
    let root = at.join("root");
    let lib = versioned(
        &at.join("v"),
        "libv.so.1",
        "void p_old(void){}\n__asm__(\".symver p_old,p@V1\");\n\
         void p_new(void){}\n__asm__(\".symver p_new,p@@V2\");\n",
        Some("V1 { global: p; local: *; };\nV2 { global: p; local: *; } V1;\n"),
    );
    let app = binsrc(
        &at.join("app"),
        "app",
        "void p(void);\nvoid _start(void){p();}\n",
        &lib,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "v", &[("usr/lib64/libv.so.1", &lib)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(!out.contains("duplicate-symbols"), "{out}");
    assert!(ok, "{out}");
}

// an inline function or a template instantiation is emitted into every object that used
// it and marked weak so the linker can collapse them. libclang-cpp and libLLVM share 299
// of those, which is c++ working rather than two libraries fighting over a name
#[test]
fn a_weak_export_in_two_libraries_is_not_a_clash() {
    if skip("duplicate symbols") {
        return;
    }
    let at = scratch("vague");
    let root = at.join("root");
    let body =
        |own: &str| format!("__attribute__((weak)) void shared(void){{}}\nvoid {own}(void){{}}\n");
    let one = libsrc(&at.join("one"), "libone.so.1", &body("a"));
    let two = libsrc(&at.join("two"), "libtwo.so.1", &body("b"));
    let ua = binsrc(
        &at.join("ua"),
        "ua",
        "void a(void);\nvoid _start(void){a();}\n",
        &one,
        Some("$ORIGIN/../lib64"),
    );
    let ub = binsrc(
        &at.join("ub"),
        "ub",
        "void b(void);\nvoid _start(void){b();}\n",
        &two,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "one", &[("usr/lib64/libone.so.1", &one)]);
    install(&root, "two", &[("usr/lib64/libtwo.so.1", &two)]);
    install(&root, "ua", &[("usr/bin/ua", &ua)]);
    install(&root, "ub", &[("usr/bin/ub", &ub)]);

    let (ok, out) = doctor(&root);
    assert!(!out.contains("duplicate-symbols"), "{out}");
    assert!(ok, "{out}");
}

// libssl and libcrypto both define OPENSSL_3.0.0 through OPENSSL_3.5.0, and a library
// carries one symbol per version node it defines, named after the node. those label the
// version rather than exporting anything
#[test]
fn a_version_label_in_two_libraries_is_not_a_clash() {
    if skip("duplicate symbols") {
        return;
    }
    let at = scratch("labels");
    let root = at.join("root");
    let script = "V1 { global: a; b; local: *; };\n";
    let one = versioned(
        &at.join("one"),
        "libone.so.1",
        "void a(void){}\n",
        Some(script),
    );
    let two = versioned(
        &at.join("two"),
        "libtwo.so.1",
        "void b(void){}\n",
        Some(script),
    );
    let ua = binsrc(
        &at.join("ua"),
        "ua",
        "void a(void);\nvoid _start(void){a();}\n",
        &one,
        Some("$ORIGIN/../lib64"),
    );
    let ub = binsrc(
        &at.join("ub"),
        "ub",
        "void b(void);\nvoid _start(void){b();}\n",
        &two,
        Some("$ORIGIN/../lib64"),
    );
    install(&root, "one", &[("usr/lib64/libone.so.1", &one)]);
    install(&root, "two", &[("usr/lib64/libtwo.so.1", &two)]);
    install(&root, "ua", &[("usr/bin/ua", &ua)]);
    install(&root, "ub", &[("usr/bin/ub", &ub)]);

    let (ok, out) = doctor(&root);
    assert!(!out.contains("duplicate-symbols"), "{out}");
    assert!(ok, "{out}");
}

// the gnu tree is usr/lib64 and the musl one is usr/lib. musl's loader ignores symbol
// versions entirely, so a gnu binary that reaches across binds to whatever carries the
// right name and nothing errors until it behaves oddly
#[test]
fn a_gnu_binary_reaching_into_the_musl_tree_is_flagged() {
    if skip("cross tier") {
        return;
    }
    let at = scratch("crosstier");
    let root = at.join("root");
    let l = lib(&at, "libp.so.1");
    let app = bin(&at, "app", &l, Some("$ORIGIN/../lib"));
    install(&root, "libp", &[("usr/lib/libp.so.1", &l)]);
    install(&root, "app", &[("usr/bin/app", &app)]);

    let (ok, out) = doctor(&root);
    assert!(
        !ok,
        "a gnu binary resolved into the musl tree and doctor was happy"
    );
    assert!(
        out.contains(&format!(
            "usr/bin/app {TARGET} cross-tier usr/lib/libp.so.1"
        )),
        "{out}"
    );
}
