// doctor answers the same question ldd does, so ldd is what it gets checked against.
// every fixture is built with cc and installed through the real binary

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn kiry(args: &[&str]) -> Output {
    Command::new(KIRY).args(args).output().unwrap()
}

fn recipe(at: &Path, name: &str, script: &str) -> PathBuf {
    let d = at.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("version"), "1.0 1\n").unwrap();
    fs::write(d.join("targets"), format!("{TARGET}\n")).unwrap();
    fs::write(d.join("build"), script).unwrap();
    d
}

// -nostdlib keeps libc out of DT_NEEDED, so the only unresolved thing in a fixture root
// is the one the test put there
const LIB: &str = "echo 'void p(void){}' > p.c\n\
     cc -shared -fPIC -nostdlib -Wl,-soname,libp.so.1 -o libp.so.1 p.c\n";
const MAIN: &str = "printf 'void p(void);\\nvoid _start(void){p();}\\n' > m.c\n";

fn have_cc() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn skip(why: &str) -> bool {
    if have_cc() && Command::new("ldd").arg("--version").output().is_ok() {
        return false;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "{why}: cc and ldd are what this suite checks against"
    );
    true
}

fn build_and_install(at: &Path, root: &Path, pkgs: &[&Path]) {
    let mut args: Vec<String> = vec!["b".into(), "--root".into(), root.display().to_string()];
    args.extend(pkgs.iter().map(|p| p.display().to_string()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = kiry(&refs);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    for p in pkgs {
        let name = p.file_name().unwrap().to_str().unwrap();
        let art = root.join(format!("var/kiry/cache/{name}-1.0-1.{TARGET}.tar.zst"));
        let o = kiry(&["i", "--root", root.to_str().unwrap(), art.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let _ = at;
}

fn doctor(root: &Path) -> (bool, String) {
    let o = kiry(&["doctor", "--root", root.to_str().unwrap()]);
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
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/\"\n"),
    );
    let bin = recipe(
        &at,
        "app",
        &format!("{LIB}{MAIN}cc -nostdlib -o app m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp app \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&lib, &bin]);

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
    fs::create_dir_all(&root).unwrap();

    let bin = recipe(
        &at,
        "app",
        &format!("{LIB}{MAIN}cc -nostdlib -o app m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp app \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&bin]);

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
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64/priv\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/priv/\"\n"),
    );
    let bare = recipe(
        &at,
        "bare",
        &format!("{LIB}{MAIN}cc -nostdlib -o bare m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp bare \"$DESTDIR/usr/bin/\"\n"),
    );
    let rp = recipe(
        &at,
        "rp",
        &format!("{LIB}{MAIN}cc -nostdlib -Wl,-rpath,'$ORIGIN/../lib64/priv' -o rp m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp rp \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&lib, &bare, &rp]);

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
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/\"\n"),
    );
    build_and_install(&at, &root, &[&lib]);
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
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/\"\n"),
    );
    build_and_install(&at, &root, &[&lib]);

    // same path, different soname, which is what a hand swapped library looks like
    let src = at.join("swap.c");
    fs::write(&src, "void p(void){}\n").unwrap();
    let built = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libp.so.2",
            "-o",
        ])
        .arg(root.join("usr/lib64/libp.so.1"))
        .arg(&src)
        .status()
        .unwrap();
    assert!(built.success());

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

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64/priv\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/priv/\"\n"),
    );
    let app = recipe(
        &at,
        "app",
        &format!("{LIB}{MAIN}cc -nostdlib -Wl,-rpath,'$ORIGIN/../../lib64/priv' -o app m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp app \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&lib, &app]);

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
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/lib64/priv\"\ncp libp.so.1 \"$DESTDIR/usr/lib64/priv/\"\n"),
    );
    let app = recipe(
        &at,
        "app",
        &format!("{LIB}{MAIN}cc -nostdlib -Wl,--disable-new-dtags -Wl,-rpath,'$ORIGIN/../lib64/priv' -o app m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp app \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&lib, &app]);

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

// every other fixture spells the runpath $ORIGIN/.., where the .. cancels the token
// even unexpanded. a library sitting beside its binary is what actually needs $ORIGIN
// to mean something, and it is the commonest shape a bundled app ships
#[test]
fn origin_names_the_directory_the_binary_is_in() {
    if skip("origin") {
        return;
    }
    let at = scratch("origin");
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();

    let lib = recipe(
        &at,
        "libp",
        &format!("{LIB}mkdir -p \"$DESTDIR/usr/bin\"\ncp libp.so.1 \"$DESTDIR/usr/bin/\"\n"),
    );
    let app = recipe(
        &at,
        "app",
        &format!("{LIB}{MAIN}cc -nostdlib -Wl,-rpath,'$ORIGIN' -o app m.c libp.so.1\nmkdir -p \"$DESTDIR/usr/bin\"\ncp app \"$DESTDIR/usr/bin/\"\n"),
    );
    build_and_install(&at, &root, &[&lib, &app]);

    let (ok, out) = doctor(&root);
    assert!(ok && out.is_empty(), "{out}");
    assert!(ldd_finds(&root.join("usr/bin/app"), "libp.so.1"));
}
