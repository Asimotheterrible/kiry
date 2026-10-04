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

// $source holds what the apkbuild wrote, not the names the files land under. 64 recipes
// reach for ${p##*/} to get a name back out of it, and bash finds its vendor patches by
// matching */bash[0-9][0-9]-[0-9]*, which a bare name never matches
#[test]
fn source_holds_the_entries_and_patch_args_comes_with_them() {
    if !have_busybox() {
        return;
    }
    let at = scratch("srcline");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         patch_args=\"-p0\"\n\
         source=\"thing-1.0.tar.gz::https://example.invalid/get?id=7\n\
         https://example.invalid/patches/thing-001\n\
         local.patch\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    assert!(
        script.contains("thing-1.0.tar.gz::https://example.invalid/get?id=7"),
        "{script}"
    );
    assert!(
        script.contains("https://example.invalid/patches/thing-001"),
        "{script}"
    );
    assert!(script.contains("patch_args=\"-p0\""), "{script}");

    // and a pattern with a slash in it still finds the entry, which is the whole point
    let o = Command::new("busybox")
        .args([
            "ash",
            "-c",
            &format!(
                "{}\nfor p in $source; do case $p in */thing-[0-9]*) echo hit ${{p##*/}} ;; esac; done",
                script
                    .lines()
                    .take_while(|l| !l.starts_with("prepare()"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .replace(". /usr/share/kiry/lib.sh", ":")
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        "hit thing-001",
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
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
    // called in order, and prepare before the two that need what it applied
    let p = script.rfind("\nprepare\n").unwrap();
    let b = script.rfind("\nbuild\n").unwrap();
    assert!(
        p < b && b < script.rfind("\npackage\n").unwrap(),
        "{script}"
    );

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
// with no kiry equivalent. what cannot be carried is reported rather than invented. a
// so: name is carried as it is: which package it means is decided when the recipe is
// read, against what is installed then
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
        // core/aliases maps meson to muon, so the conversion picks that up from the
        // repos this machine has configured
        ["barlib", "expat make", "libfoo", "muon make", "so:libz.so.1 make"],
        "{deps}"
    );
    assert!(said.contains("!gettext-dev"), "{said}");
    assert!(!said.contains("so:libz.so.1"), "{said}");
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
    // both spellings in the body were rewritten, counted inside package() so the
    // cleanup abuild also does after it cannot pad the number
    let body = script.split("package() {").nth(1).unwrap();
    let body = body.split("\n}").next().unwrap();
    assert_eq!(body.matches("$DESTDIR").count(), 2, "{script}");
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
    // found under the renamed key, which is the only place alpine files it
    let sums = fs::read_to_string(d.join("checksums")).unwrap();
    assert_eq!(sums.trim(), "abc", "{said}");

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
// instead of failing, so install libbz2.so.$pkgver lands a file called libbz2.so
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

// sources and checksums pair by position. offline there is no fetching a tarball, so the
// patches beside the apkbuild get checksums and the tarball does not, and a recipe with
// 73 sources against 72 checksums does not load at all
#[test]
fn a_recipe_converted_offline_still_loads() {
    if !have_busybox() {
        return;
    }
    let at = scratch("offline-counts");
    let d = at.join("aports/thing");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("fix.patch"), "--- a\n+++ b\n").unwrap();
    fs::write(
        d.join("APKBUILD"),
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         source=\"https://example.invalid/thing-1.0.tar.gz\n\tfix.patch\"\n\
         build() {\n\tmake\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    )
    .unwrap();
    let out = at.join("out");
    let o = kiry(&[
        "convert",
        "-n",
        d.join("APKBUILD").to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let made = out.join("thing");
    let sources = fs::read_to_string(made.join("sources")).unwrap();
    assert_eq!(sources.lines().count(), 2, "{sources}");
    // nothing rather than half an answer. the sha256 lands when something fetches
    assert!(
        fs::read_to_string(made.join("checksums")).unwrap().trim().is_empty(),
        "a partial checksums file is one the recipe cannot be read back with"
    );

    let shown = kiry(&[made.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&shown.stderr);
    assert!(shown.status.success(), "{text}");
    assert!(!text.contains("but"), "{text}");
}

fn apkbuild(at: &Path, name: &str, extra: &str) -> PathBuf {
    let d = at.join("aports").join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("APKBUILD"),
        format!(
            "pkgname={name}\npkgver=1.0\npkgrel=0\n{extra}\n\
             build() {{\n\tmake\n}}\n\
             package() {{\n\tmake install DESTDIR=\"$pkgdir\"\n}}\n"
        ),
    )
    .unwrap();
    d.join("APKBUILD")
}

fn deps(d: &Path) -> Vec<String> {
    fs::read_to_string(d.join("depends"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect()
}

// a subpackage is folded into the recipe that builds it, so naming one is naming its
// parent. 1788 of the imported recipes name one
#[test]
fn a_dependency_on_a_subpackage_is_a_dependency_on_its_parent() {
    if !have_busybox() {
        return;
    }
    let at = scratch("parents");
    let a = apkbuild(&at, "db", "subpackages=\"$pkgname-doc $pkgname-client:client libdb:libs\"");
    // lua is a name two apkbuilds claim, and picking one is a decision
    let l1 = apkbuild(&at, "lua5.3", "provides=\"lua\"");
    let l2 = apkbuild(&at, "lua5.4", "provides=\"lua\"");
    let b = apkbuild(
        &at,
        "app",
        "depends=\"db-client libdb lua\"\nmakedepends=\"db-dev\"",
    );
    let out = at.join("out");
    // a root of its own: this machine's aliases may well have chosen a lua already, and
    // what is under test is that convert does not
    let root = at.join("root");
    fs::create_dir_all(&root).unwrap();
    let o = kiry(&[
        "convert",
        "--root",
        root.to_str().unwrap(),
        "-n",
        a.to_str().unwrap(),
        l1.to_str().unwrap(),
        l2.to_str().unwrap(),
        b.to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let got = deps(&out.join("app"));
    assert!(!got.contains(&"db-client".to_string()), "{got:?}");
    assert!(!got.contains(&"libdb".to_string()), "{got:?}");
    assert!(got.contains(&"db".to_string()), "{got:?}");
    assert!(got.contains(&"lua".to_string()), "an ambiguous name got resolved: {got:?}");

    // written where the next single conversion can read it, and marked as convert's own
    let table = fs::read_to_string(out.join("aliases")).unwrap();
    assert!(table.starts_with("# written by kiry convert"), "{table}");
    assert!(table.contains("db-client\tdb"), "{table}");
    assert!(!table.lines().any(|l| l.starts_with("lua\t")), "{table}");
}

// a hand-kept pair outranks anything derived, and a parent that is itself aliased is
// chased -- so alpine's clang lands on this tree's llvm and not on a second llvm
#[test]
fn a_hand_kept_alias_wins_over_a_derived_one() {
    if !have_busybox() {
        return;
    }
    let at = scratch("parents-hand");
    let repo = at.join("core");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("aliases"), "clang23\tllvm\nclang23-extra\t-\n").unwrap();
    let root = at.join("root");
    fs::create_dir_all(root.join("etc/kiry")).unwrap();
    fs::write(root.join("etc/kiry/repos"), format!("{}\n", repo.display())).unwrap();

    let c = apkbuild(&at, "clang23", "subpackages=\"clang23-extra:extra clang:clang_link\"");
    let b = apkbuild(&at, "app", "makedepends=\"clang\"");
    // its own subpackage is something the one recipe already builds
    let s = apkbuild(&at, "self", "subpackages=\"self-tools:tools\"\ndepends=\"self-tools\"");
    let out = at.join("out");
    let o = kiry(&[
        "convert",
        "-n",
        "--root",
        root.to_str().unwrap(),
        c.to_str().unwrap(),
        b.to_str().unwrap(),
        s.to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(deps(&out.join("app")), ["llvm"]);
    assert!(deps(&out.join("self")).is_empty(), "{:?}", deps(&out.join("self")));
    // what a hand-kept file already answers is not written again, or the derived copy
    // outlives the day somebody changes the hand-kept one
    let table = fs::read_to_string(out.join("aliases")).unwrap();
    assert!(!table.contains("clang23-extra"), "{table}");
}

// offline there is no hashing the tarball, but alpine already did. its sha512 goes in,
// and the first fetch is checked against it rather than trusted
#[test]
fn an_offline_conversion_keeps_alpines_sha512() {
    if !have_busybox() {
        return;
    }
    let at = scratch("offline-sha512");
    let sum = "ab".repeat(64);
    let a = apkbuild(
        &at,
        "thing",
        &format!(
            "source=\"https://example.invalid/thing-1.0.tar.gz\"\nsha512sums=\"{sum}  thing-1.0.tar.gz\""
        ),
    );
    let out = at.join("out");
    let o = kiry(&["convert", "-n", a.to_str().unwrap(), out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read_to_string(out.join("thing/checksums")).unwrap().trim(),
        sum
    );
}

// kiry never runs check(), so a suite the configure line asks for is built for nothing,
// and glm, cxxopts and mbedtls2 each failed inside theirs on clang's -Werror
#[test]
fn test_suites_the_build_asks_for_are_switched_off() {
    if !have_busybox() {
        return;
    }
    let at = scratch("untest");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         _conf() {\n\t./configure --enable-tests \"--enable-unit-tests\" --enable-test\n}\n\
         build() {\n\tcmake -B build -DBUILD_TESTING=ON \\\n\
         \t\t-DGLM_BUILD_TESTS=TRUE -DKDSoap_TESTS=true -DBUILD_TESTS:BOOL=1 \\\n\
         \t\t-DQUIC_BUILD_TEST=on -DENABLE_TESTING=Yes\n\
         \tmeson setup build -Dtests=true -Dunit_tests=enabled -Dtest=true\n\t_conf\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    for want in [
        "./configure --disable-tests \"--disable-unit-tests\" --disable-test\n",
        "-DBUILD_TESTING=OFF \\\n",
        "-DGLM_BUILD_TESTS=FALSE ",
        "-DKDSoap_TESTS=false ",
        "-DBUILD_TESTS:BOOL=0 ",
        "-DQUIC_BUILD_TEST=off ",
        "-DENABLE_TESTING=No\n",
        "-Dtests=false ",
        "-Dunit_tests=disabled ",
        "-Dtest=false\n",
    ] {
        assert!(script.contains(want), "no {want:?} in\n{script}");
    }
}

// gobject-introspection, vala and gtk-doc are dropped in core/aliases, so a switch the
// build forces on for one of them is switched off with it
#[test]
fn switches_for_what_the_aliases_drop_are_switched_off() {
    if !have_busybox() {
        return;
    }
    let at = scratch("undropped");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         build() {\n\tmeson setup build -Dintrospection=enabled -Dvapi=true \\\n\
         \t\t-Dgtk_doc=true -Dgir=true -Dfoo=true\n\
         \t./configure --enable-introspection --enable-vala --enable-gtk-doc=yes\n}\n\
         package() {\n\tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    for want in [
        "-Dintrospection=disabled -Dvapi=false \\\n",
        "-Dgtk_doc=false -Dgir=false -Dfoo=true\n",
        "--disable-introspection --disable-vala --enable-gtk-doc=yes\n",
    ] {
        assert!(script.contains(want), "no {want:?} in\n{script}");
    }
}

// alpine's -flto=auto is full lto on top of kiry's own LTO knob, and never goes through
// the thinlto cache. the words go and the quotes and continuations around them stay,
// and the same text inside a longer word is left alone
#[test]
fn alpines_lto_words_come_out_and_the_line_stays_whole() {
    if !have_busybox() {
        return;
    }
    let at = scratch("unlto");
    let (d, _) = convert(
        &at,
        "pkgname=thing\npkgver=1.0\npkgrel=0\n\
         build() {\n\tCFLAGS=\"$CFLAGS -O2 -flto=auto\" \\\n\
         \tCXXFLAGS=\"$CXXFLAGS -flto=auto -ffat-lto-objects -DNDEBUG\" \\\n\
         \t./configure --prefix=/usr\n\
         \texport LDFLAGS=\"$LDFLAGS -flto=auto\"\n}\n\
         package() {\n\tsed -i -e \"s| -flto=auto||g\" Config_heavy.pl\n\
         \tmake install DESTDIR=\"$pkgdir\"\n}\n",
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    for want in [
        "CFLAGS=\"$CFLAGS -O2 \" \\\n",
        "CXXFLAGS=\"$CXXFLAGS   -DNDEBUG\" \\\n",
        "export LDFLAGS=\"$LDFLAGS \"\n",
        // inside a longer word it is something else, here what perl's own sed removes
        "sed -i -e \"s| -flto=auto||g\" Config_heavy.pl\n",
    ] {
        assert!(script.contains(want), "no {want:?} in\n{script}");
    }
}

// a false positive switches off something the package installs, which is worse than a
// suite built for nothing. each of these is a real flag from a real apkbuild
#[test]
fn options_that_only_look_like_tests_stay_on() {
    if !have_busybox() {
        return;
    }
    let at = scratch("keeptest");
    let flags = [
        // link the system gtest, not build one
        "-DUSE_SYSTEM_GTEST=ON",
        "-DRC_ENABLE_GTEST_TESTS=ON",
        // already saying off
        "-DNO_TESTS=ON",
        "-DSKIP_TESTS=ON",
        "-DIGNORE_TESTS=ON",
        "-DDISABLE_TESTS=ON",
        // libSDL2_test, liborc-test and the kcapi tool, all installed
        "-DSDL_TEST=ON",
        "-Dorc-test=enabled",
        "--enable-kcapi-test",
        // php's zend_test extension
        "--enable-zend-test=shared",
        // a value is what it asks for, and flipping the name alone keeps the value
        "--enable-testing-mode=no",
        // a value that is not a yes
        "-DBUILD_TESTING=\"$build_testing\"",
        "-DTESTDATA=ON",
        "-DLATEST=ON",
    ];
    let (d, _) = convert(
        &at,
        &format!(
            "pkgname=thing\npkgver=1.0\npkgrel=0\n\
             build() {{\n\tcmake {}\n}}\n\
             package() {{\n\tmake install DESTDIR=\"$pkgdir\"\n}}\n",
            flags.join(" ")
        ),
    );
    let script = fs::read_to_string(d.join("build")).unwrap();
    for f in flags {
        assert!(script.contains(&format!(" {f}")), "{f} was changed\n{script}");
    }
}

// gtk+-3.0.pc never names wayland-protocols. alpine's gtk+3.0-dev hands it to whatever
// builds against gtk, so a recipe converted from that makedepends has to name it itself
#[test]
fn a_dev_makedepend_brings_its_depends_dev_as_build_deps() {
    if !have_busybox() {
        return;
    }
    let at = scratch("devdeps");
    let tree = at.join("aports/community");
    for (pkg, body) in [
        ("gtk", "pkgname=gtk\npkgver=3\npkgrel=0\ndepends_dev=\"glib-dev pango-libs python3 wayland-protocols\"\n"),
        (
            "thing",
            "pkgname=thing\npkgver=1.0\npkgrel=0\nmakedepends=\"gtk-dev glib-dev\"\n\
             package() {\n\ttrue\n}\n",
        ),
    ] {
        fs::create_dir_all(tree.join(pkg)).unwrap();
        fs::write(tree.join(pkg).join("APKBUILD"), body).unwrap();
    }
    let out = at.join("out");
    let a = tree.join("thing/APKBUILD");
    let o = kiry(&["convert", "-n", a.to_str().unwrap(), out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let deps = fs::read_to_string(out.join("thing/depends")).unwrap();
    let lines: Vec<&str> = deps.lines().collect();
    assert!(lines.contains(&"wayland-protocols build"), "{deps}");
    // a library comes in through its own .pc, so it adds nothing
    assert!(lines.contains(&"glib make") && !lines.contains(&"glib build"), "{deps}");
    assert!(!lines.contains(&"pango build"), "{deps}");
    // a tool is not a header, whatever list it sits in
    assert!(!lines.iter().any(|l| l.starts_with("python3")), "{deps}");
}
