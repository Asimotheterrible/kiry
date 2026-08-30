use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .join(format!("kiry-c-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn kiry(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kiry"))
        .args(args)
        .output()
        .unwrap()
}

// an apkbuild is sourced, not parsed, so the shell abuild uses is not optional here
fn have_busybox() -> bool {
    if Command::new("busybox").arg("true").status().is_ok() {
        return true;
    }
    assert!(
        std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
        "no busybox, and the converter cannot read an apkbuild without ash"
    );
    false
}

fn convert(at: &Path, body: &str) -> (PathBuf, String) {
    let d = at.join("aports/thing");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("APKBUILD"), body).unwrap();
    let out = at.join("out");
    let o = kiry(&[
        "convert",
        "-n",
        d.join("APKBUILD").to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    (
        out.join("thing"),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    )
}

// $CARCH picks the value and only a shell knows which branch ran. a regex over the text
// would carry every branch across, or the wrong one
#[test]
fn a_case_on_carch_decides_a_private_variable() {
    if !have_busybox() {
        return;
    }
    let at = scratch("carch");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         case \"$CARCH\" in\nx86_64) _flavour=\"wide\" ;;\n*) _flavour=\"narrow\" ;;\nesac\n\
         build() {\n\techo $_flavour\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("_flavour=\"wide\""), "{script}");
    assert!(!script.contains("narrow"), "{script}");
}

// abuild runs default_prepare when an apkbuild writes no prepare() of its own. 1505
// recipes carry patches and define none, and without this they build unpatched and
// succeed, which is the failure that says nothing
#[test]
fn an_apkbuild_with_no_prepare_still_applies_its_patches() {
    if !have_busybox() {
        return;
    }
    let at = scratch("defprep");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         source=\"thing-1.0.tar.gz musl-macros.patch\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("prepare() {"), "{script}");
    assert!(script.contains("default_prepare"), "{script}");
    assert!(script.trim_end().ends_with("package"), "{script}");
    // and it runs before build
    let p = script.rfind("\nprepare\n").unwrap();
    assert!(p < script.rfind("\nbuild\n").unwrap(), "{script}");

    // one that writes its own is left alone
    let at = scratch("ownprep");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         prepare() {\n\techo mine\n}\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("echo mine"), "{script}");
    assert_eq!(script.matches("prepare() {").count(), 1, "{script}");
    assert!(!script.contains("default_prepare"), "{script}");
}

// a body calls helpers defined beside it. _configure and _build are the common two and
// 112 packages call one, so dropping them leaves a script that fails on its first line
#[test]
fn a_private_helper_comes_across_and_a_subpackage_one_does_not() {
    if !have_busybox() {
        return;
    }
    let at = scratch("helpers");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         subpackages=\"$pkgname-dev $pkgname-doc:manuals $pkgname-c++\"\n\
         _configure() {\n\t./configure --prefix=/usr\n}\n\
         build() {\n\t_configure\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n\
         dev() {\n\tamove usr/include\n}\n\
         manuals() {\n\tamove usr/share/man\n}\n\
         c__() {\n\tamove usr/lib/libthing++.so\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("_configure() {"), "{script}");
    assert!(script.contains("./configure --prefix=/usr"), "{script}");
    // defined before the cd, so a helper can be the body's first line
    assert!(
        script.find("_configure() {").unwrap() < script.find("cd \"$builddir\"").unwrap(),
        "{script}"
    );
    assert!(!script.contains("amove"), "{script}");
    for f in ["dev() {", "manuals() {", "c__() {"] {
        assert!(!script.contains(f), "{f} came across: {script}");
    }
}

// alpine splits a package and kiry does not, so the functions that move files into a
// subpackage have nothing to move and must not come across
#[test]
fn subpackage_functions_do_not_come_across() {
    if !have_busybox() {
        return;
    }
    let at = scratch("subpkg");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\nsubpackages=\"$pkgname-dev\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n\
         dev() {\n\tamove usr/include\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("make install"), "{script}");
    assert!(!script.contains("amove"), "{script}");
}

// a version constraint, a ! for a conflict and a -dev suffix are all alpine spellings
// with no kiry equivalent. what cannot be carried is reported rather than invented
#[test]
fn alpine_dep_spellings_become_kiry_names() {
    if !have_busybox() {
        return;
    }
    let at = scratch("deps");
    let (d, said) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         depends=\"libfoo>=2.1 barlib\"\n\
         makedepends=\"expat-dev>=2.8.0 !gettext-dev so:libz.so.1 meson\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let deps = fs::read_to_string(d.join("depends")).unwrap();
    let mut got: Vec<&str> = deps.lines().collect();
    got.sort_unstable();
    assert_eq!(
        got,
        ["barlib", "expat make", "libfoo", "meson make"],
        "{deps}"
    );
    assert!(said.contains("!gettext-dev"), "{said}");
    assert!(said.contains("so:libz.so.1"), "{said}");
}

// abuild cds into builddir before running build(), and kiry lands in /src when more than
// one thing unpacked there. the prologue is what makes the body's assumption true again
#[test]
fn the_script_cds_where_the_body_expects_to_be() {
    if !have_busybox() {
        return;
    }
    let at = scratch("builddir");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("builddir=\"/src/thing-1.0\""), "{script}");
    assert!(script.contains("cd \"$builddir\""), "{script}");

    let at = scratch("builddir-set");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0_rc1\npkgrel=0\n\
         builddir=\"$srcdir/thing-${pkgver/_/-}\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(
        script.contains("builddir=\"/src/thing-1.0-rc1\""),
        "{script}"
    );
}

// $pkgdir is where abuild stages and $DESTDIR is where kiry does. nothing else in a
// package() body needs touching
#[test]
fn pkgdir_becomes_destdir() {
    if !have_busybox() {
        return;
    }
    let at = scratch("pkgdir");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n\tinstall -Dm644 x ${pkgdir}/usr/share/x\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(!script.contains("pkgdir"), "{script}");
    assert_eq!(script.matches("$DESTDIR").count(), 2, "{script}");
}

// abuild runs each phase as a function and 852 scripts declare a local in one. inlined
// at top level ash refuses the line outright
#[test]
fn a_phase_is_a_function_because_local_only_works_in_one() {
    if !have_busybox() {
        return;
    }
    let at = scratch("localvar");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         build() {\n\tlocal opt=fast\n\tmake $opt\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("build() {"), "{script}");
    // defined first, then called, so builddir is the cwd when the body runs
    let cd = script.find("cd \"$builddir\"").unwrap();
    assert!(script.find("build() {").unwrap() < cd, "{script}");
    assert!(script.rfind("\nbuild\n").unwrap() > cd, "{script}");
    assert!(script.rfind("\npackage\n").unwrap() > cd, "{script}");

    // and the shell that runs it agrees
    let o = Command::new("busybox")
        .args(["ash", "-n", d.join("build").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// alpine's sha512sums is keyed by the name a source was renamed to, so a converter that
// reads the url's basename comes away with no checksum for two fifths of aports
#[test]
fn a_renamed_source_keeps_its_name_and_its_checksum() {
    if !have_busybox() {
        return;
    }
    let at = scratch("rename");
    let (d, said) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         source=\"thing-1.0.tar.gz::https://example.invalid/download?id=7\"\n\
         sha512sums=\"abc  thing-1.0.tar.gz\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let srcs = fs::read_to_string(d.join("sources")).unwrap();
    assert_eq!(
        srcs, "thing-1.0.tar.gz::https://example.invalid/download?id=7\n",
        "{srcs}"
    );
    assert!(said.contains("thing-1.0.tar.gz not fetched"), "{said}");

    // a wrong hash under the renamed key has to be caught. keyed by the url's basename
    // it is never looked up, and the recipe comes out with a sha256 nobody vouched for
    let at = scratch("rename-sum");
    let d = at.join("aports/thing");
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("APKBUILD"),
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         source=\"thing-1.0.tar.gz::https://example.invalid/download?id=7\"\n\
         sha512sums=\"deadbeef  thing-1.0.tar.gz\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    )
    .unwrap();
    let fetcher = at.join("fetch.sh");
    fs::write(&fetcher, "#!/bin/sh\necho hello > \"$2\"\n").unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&fetcher)
        .status()
        .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_kiry"))
        .args([
            "convert",
            d.join("APKBUILD").to_str().unwrap(),
            at.join("out").to_str().unwrap(),
        ])
        .env("KIRY_FETCH", format!("{} %u %o", fetcher.display()))
        .output()
        .unwrap();
    assert!(!o.status.success());
    let said =
        String::from_utf8_lossy(&o.stderr).into_owned() + &String::from_utf8_lossy(&o.stdout);
    assert!(said.contains("the apkbuild says deadbeef"), "{said}");
}

// alpine's name for a thing is not always this system's name for it, and nothing derives
// one from the other. a name mapped to - has no equivalent here and is not a dependency
#[test]
fn aliases_rename_and_drop() {
    if !have_busybox() {
        return;
    }
    let at = scratch("aliases");
    let repo = at.join("repo");
    fs::create_dir_all(repo.join("llvm")).unwrap();
    fs::write(repo.join("llvm/build"), "make\n").unwrap();
    fs::write(
        repo.join("aliases"),
        "# alpine        here\nllvm22          llvm\nlibselinux      -\n",
    )
    .unwrap();
    let root = at.join("root");
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();

    let d = at.join("aports/thing");
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("APKBUILD"),
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         makedepends=\"llvm22-dev libselinux-dev cowsay\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    )
    .unwrap();
    let out = at.join("out");
    let o = kiry(&[
        "convert",
        "-n",
        "--root",
        root.to_str().unwrap(),
        d.join("APKBUILD").to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let said = String::from_utf8_lossy(&o.stdout);

    let deps = fs::read_to_string(out.join("thing/depends")).unwrap();
    assert_eq!(deps, "cowsay make\nllvm make\n", "{deps}");
    assert!(
        said.contains("libselinux-dev has no equivalent here"),
        "{said}"
    );
    // llvm is carried, cowsay is not, and only one of those should be said out loud
    assert!(
        said.contains("cowsay is a dependency no repo carries"),
        "{said}"
    );
    assert!(!said.contains("llvm is a dependency no repo"), "{said}");
}

// binutils and gcc read CTARGET to decide whether they are building a cross compiler,
// and an unset one is not equal to CHOST. the cross branch renames the package, so the
// recipe lands somewhere nothing will look for it
#[test]
fn a_package_that_reads_ctarget_is_not_a_cross_compiler() {
    if !have_busybox() {
        return;
    }
    let at = scratch("ctarget");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         if [ \"$CHOST\" != \"$CTARGET\" ]; then\n\tpkgname=\"$pkgname-$CTARGET_ARCH\"\nfi\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    assert!(d.ends_with("thing"), "{}", d.display());
    assert!(d.join("version").is_file(), "{}", d.display());
}

// a body says $pkgver as readily as it says $srcdir. an unset one expands to nothing
// instead of failing, so install libbz2.so.$pkgver lands a file called libbz2.so.
#[test]
fn the_script_knows_its_own_name_and_version() {
    if !have_busybox() {
        return;
    }
    let at = scratch("pkgvars");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0.8\npkgrel=6\n\
         build() {\n\tmake\n}\n\
         package() {\n\tinstall -D lib.so.$pkgver \"$pkgdir\"/usr/lib/lib.so.$pkgver\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(script.contains("pkgname=\"thing\""), "{script}");
    assert!(script.contains("pkgver=\"1.0.8\""), "{script}");
    assert!(script.contains("pkgrel=\"6\""), "{script}");
}

// alpine splits build deps three ways for cross compiling. reading only makedepends
// leaves a package that uses the split forms looking like it has none, which is worse
// than missing them: it looks buildable
#[test]
fn the_split_makedepends_forms_are_dependencies_too() {
    if !have_busybox() {
        return;
    }
    let at = scratch("splitdeps");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         makedepends_host=\"ncurses-dev\"\nmakedepends_build=\"flex\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let deps = fs::read_to_string(d.join("depends")).unwrap();
    let mut got: Vec<&str> = deps.lines().collect();
    got.sort_unstable();
    assert_eq!(got, ["flex make", "ncurses make"], "{deps}");
}
