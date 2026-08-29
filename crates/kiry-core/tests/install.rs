// end to end against real archives built by the real tools. this shells out, which
// is exactly why it lives in tests/ rather than inside kiry-core

use std::fs;
use std::path::{Path, PathBuf};

use kiry_core::archive::{Member, What};
use kiry_core::install::{Job, Removed};
use kiry_core::{archive, db, install, Error};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .join(format!("kiry-e2e-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// same job as archive's own root(), kept separate so the two suites stay independent
fn root(name: &str) -> PathBuf {
    let d = scratch(name);
    let r = d.join("root");
    fs::create_dir_all(&r).unwrap();
    r
}

fn paths(m: &[Member]) -> Vec<&str> {
    m.iter().map(|e| e.path.as_str()).collect()
}

fn pack(at: &Path, name: &str, deps: &[&str], files: &[&str]) -> PathBuf {
    packed(at, name, deps, files, name)
}

// body separately, because two packages shipping identical bytes is the case where the
// modified-file guard cannot tell one owner from the next
fn packed(at: &Path, name: &str, deps: &[&str], files: &[&str], body: &str) -> PathBuf {
    let src = at.join(format!("{name}-src"));
    for f in files {
        let p = src.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body.as_bytes()).unwrap();
    }

    let arc = at.join(format!("{name}.tar.zst"));
    let sh = format!(
        "cd {} && tar cf - . | zstd -q -o {}",
        src.display(),
        arc.display()
    );
    assert!(std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .status()
        .unwrap()
        .success());

    let meta = at.join(format!("{name}.tar.zst.meta"));
    fs::create_dir_all(&meta).unwrap();
    fs::write(meta.join("name"), format!("{name}\n")).unwrap();
    fs::write(meta.join("version"), "1.0 1\n").unwrap();
    fs::write(meta.join("targets"), "x86_64-musl\n").unwrap();
    fs::write(meta.join("depends"), deps.join("\n")).unwrap();
    arc
}

fn run(root: &Path, archives: &[PathBuf], force: bool) -> Result<Vec<Job>, Error> {
    let jobs = install::plan(root, archives, force)?;
    install::apply(root, &jobs)?;
    Ok(jobs)
}

fn rooted(name: &str) -> (PathBuf, PathBuf) {
    let d = scratch(name);
    let root = d.join("root");
    fs::create_dir_all(&root).unwrap();
    (d, root)
}

#[test]
fn a_batch_installs_and_records_itself() {
    let d = scratch("basic");
    let root = d.join("root");
    fs::create_dir_all(&root).unwrap();
    let a = pack(&d, "foo", &[], &["usr/bin/foo"]);

    run(&root, &[a], false).unwrap();

    assert_eq!(fs::read_to_string(root.join("usr/bin/foo")).unwrap(), "foo");
    let rec = db::read(&root, "x86_64-musl", "foo").unwrap();
    assert_eq!(rec.version.to_string(), "1.0 1");
    assert!(rec.manifest.iter().any(|e| e.path == "usr/bin/foo"));
}

fn rm(root: &Path, names: &[&str], force: bool) -> Result<Vec<(String, Removed)>, Error> {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    install::remove(root, "x86_64-musl", &names, force)
}

// most of these remove one package and want to talk about the counts
fn one(root: &Path, name: &str, force: bool) -> Removed {
    let mut done = rm(root, &[name], force).unwrap();
    assert_eq!(done.len(), 1);
    done.remove(0).1
}

#[test]
fn removing_takes_the_files_and_the_record() {
    let (d, root) = rooted("rm");
    run(
        &root,
        &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])],
        false,
    )
    .unwrap();

    let r = one(&root, "foo", false);
    assert_eq!(r.gone, 1);
    assert_eq!(r.kept, 0);
    assert!(!root.join("usr/share/doc/foo").exists());
    assert!(db::read(&root, "x86_64-musl", "foo").is_err());
}

#[test]
fn a_modified_file_is_left_where_it_is() {
    let (d, root) = rooted("modified");
    run(
        &root,
        &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])],
        false,
    )
    .unwrap();
    fs::write(root.join("usr/share/doc/foo/x"), b"edited by hand").unwrap();

    let r = one(&root, "foo", false);
    assert_eq!(r.kept, 1);
    assert_eq!(r.gone, 0);
    assert_eq!(
        fs::read_to_string(root.join("usr/share/doc/foo/x")).unwrap(),
        "edited by hand"
    );
}

#[test]
fn a_symlink_where_the_file_belongs_counts_as_modified() {
    let (d, root) = rooted("swapped");
    run(
        &root,
        &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])],
        false,
    )
    .unwrap();
    let p = root.join("usr/share/doc/foo/x");
    fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &p).unwrap();

    assert_eq!(one(&root, "foo", false).kept, 1);
    assert!(p.symlink_metadata().unwrap().is_symlink());
}

fn at(d: &Path, sub: &str) -> PathBuf {
    let p = d.join(sub);
    fs::create_dir_all(&p).unwrap();
    p
}

// the manifest that named a file is overwritten by the new version, so anything the new
// version stopped shipping is left owned by nobody and no check can see it afterwards
#[test]
fn a_new_version_takes_the_files_it_stopped_shipping() {
    let (d, root) = rooted("prune");
    let v1 = pack(
        &at(&d, "v1"),
        "foo",
        &[],
        &["usr/share/foo/keep", "usr/share/foo/gone"],
    );
    run(&root, &[v1], false).unwrap();
    assert!(root.join("usr/share/foo/gone").exists());

    let v2 = pack(&at(&d, "v2"), "foo", &[], &["usr/share/foo/keep"]);
    run(&root, &[v2], false).unwrap();
    assert!(root.join("usr/share/foo/keep").exists());
    assert!(!root.join("usr/share/foo/gone").exists());
}

// same rule removal already follows: a file that no longer hashes to what the manifest
// said was edited by hand, and is not ours to delete
#[test]
fn a_dropped_file_edited_by_hand_stays() {
    let (d, root) = rooted("prune-edited");
    let v1 = pack(&at(&d, "v1"), "foo", &[], &["usr/share/foo/x"]);
    run(&root, &[v1], false).unwrap();
    fs::write(root.join("usr/share/foo/x"), b"edited by hand").unwrap();

    let v2 = pack(&at(&d, "v2"), "foo", &[], &["usr/share/foo/y"]);
    run(&root, &[v2], false).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("usr/share/foo/x")).unwrap(),
        "edited by hand"
    );
}

// the whole batch decides what survives. a path one member gives up and another takes
// over has a new owner, not no owner, and the order they were named in decides nothing
#[test]
fn a_path_another_member_picks_up_survives_the_drop() {
    let (d, root) = rooted("prune-handover");
    let v1 = pack(&at(&d, "v1"), "foo", &[], &["usr/share/x", "usr/share/foo"]);
    run(&root, &[v1], false).unwrap();

    let v2 = pack(&at(&d, "v2"), "foo", &[], &["usr/share/foo"]);
    // identical bytes, so nothing but the batch keep-set can save this file
    let bar = packed(&at(&d, "bar"), "bar", &[], &["usr/share/x"], "foo");
    run(&root, &[v2, bar], false).unwrap();

    assert!(root.join("usr/share/x").exists());
    let rec = db::read(&root, "x86_64-musl", "bar").unwrap();
    assert!(rec.manifest.iter().any(|e| e.path == "usr/share/x"));
}

#[test]
fn a_dependent_blocks_removal() {
    let (d, root) = rooted("needed");
    let lib = pack(&d, "libbar", &[], &["usr/share/doc/bar/x"]);
    let app = pack(&d, "foo", &["libbar"], &["usr/share/doc/foo/x"]);
    run(&root, &[lib, app], false).unwrap();

    match rm(&root, &["libbar"], false) {
        Err(Error::Needed { pkg, by }) => {
            assert_eq!((pkg.as_str(), by.as_str()), ("libbar", "foo"));
        }
        other => panic!("wanted Needed, got {other:?}"),
    }
    assert!(rm(&root, &["libbar"], true).is_ok());
}

#[test]
fn a_dependent_leaving_in_the_same_breath_does_not_block() {
    let (d, root) = rooted("together");
    let lib = pack(&d, "libbar", &[], &["usr/share/doc/bar/x"]);
    let app = pack(&d, "foo", &["libbar"], &["usr/share/doc/foo/x"]);
    run(&root, &[lib, app], false).unwrap();

    // the depended-on name is typed first, so what is under test is the order rather
    // than the dependency
    assert_eq!(rm(&root, &["libbar", "foo"], false).unwrap().len(), 2);
    assert!(db::read(&root, "x86_64-musl", "libbar").is_err());
    assert!(db::read(&root, "x86_64-musl", "foo").is_err());
}

#[test]
fn one_blocked_member_leaves_the_whole_batch_alone() {
    let (d, root) = rooted("blocked");
    let lib = pack(&d, "libbar", &[], &["usr/share/doc/bar/x"]);
    let app = pack(&d, "foo", &["libbar"], &["usr/share/doc/foo/x"]);
    let odd = pack(&d, "qux", &[], &["usr/share/doc/qux/x"]);
    run(&root, &[lib, app, odd], false).unwrap();

    assert!(rm(&root, &["qux", "libbar"], false).is_err());
    assert!(root.join("usr/share/doc/qux/x").exists());
    assert!(db::read(&root, "x86_64-musl", "qux").is_ok());
}

#[test]
fn protected_directories_survive() {
    let (d, root) = rooted("protected");
    run(&root, &[pack(&d, "foo", &[], &["usr/bin/foo"])], false).unwrap();

    rm(&root, &["foo"], false).unwrap();
    assert!(!root.join("usr/bin/foo").exists());
    assert!(root.join("usr/bin").is_dir());
    assert!(root.join("usr").is_dir());
}

#[test]
fn a_directory_another_package_still_uses_survives() {
    let (d, root) = rooted("shared");
    let a = pack(&d, "foo", &[], &["usr/share/doc/a"]);
    let b = pack(&d, "bar", &[], &["usr/share/doc/b"]);
    run(&root, &[a, b], false).unwrap();

    rm(&root, &["foo"], false).unwrap();
    assert!(root.join("usr/share/doc/b").exists());
    assert!(root.join("usr/share/doc").is_dir());
}

#[test]
fn it_reads_a_tarball_the_real_tools_made() {
    let r = root("realzst");
    let src = r.join("src");
    fs::create_dir_all(src.join("usr/bin")).unwrap();
    fs::write(src.join("usr/bin/foo"), b"hi").unwrap();
    std::os::unix::fs::symlink("foo", src.join("usr/bin/bar")).unwrap();

    let arc = r.join("p.tar.zst");
    let sh = format!(
        "cd {} && tar cf - . | zstd -q -o {}",
        src.display(),
        arc.display()
    );
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .status()
        .unwrap();
    assert!(ok.success(), "the suite needs tar and zstd on PATH");

    let dest = r.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let m = archive::plan(&dest, &arc).unwrap();

    let got = paths(&m);
    assert!(got.contains(&"usr/bin/foo"), "got {got:?}");
    assert!(got.contains(&"usr/bin"), "got {got:?}");
    assert!(
        !got.iter().any(|p| p.is_empty()),
        "the ./ root leaked in: {got:?}"
    );
    assert!(
        m.iter().any(|e| e.what == What::Link("foo".into())),
        "the symlink did not survive: {m:?}"
    );
}

fn build(dir: &Path, arc: &Path) {
    let sh = format!(
        "cd {} && tar cf - . | zstd -q -o {}",
        dir.display(),
        arc.display()
    );
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .status()
        .unwrap();
    assert!(ok.success(), "the suite needs tar and zstd on PATH");
}

#[test]
fn it_extracts_a_real_tarball() {
    let r = root("ex");
    let src = r.join("src");
    fs::create_dir_all(src.join("usr/bin")).unwrap();
    fs::write(src.join("usr/bin/foo"), b"hello there").unwrap();
    fs::set_permissions(
        src.join("usr/bin/foo"),
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("foo", src.join("usr/bin/bar")).unwrap();

    let arc = r.join("p.tar.zst");
    build(&src, &arc);

    let dest = r.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let man = archive::extract(&dest, &arc).unwrap();

    assert_eq!(
        fs::read_to_string(dest.join("usr/bin/foo")).unwrap(),
        "hello there"
    );
    assert_eq!(
        fs::read_link(dest.join("usr/bin/bar"))
            .unwrap()
            .display()
            .to_string(),
        "foo"
    );

    let foo = man.iter().find(|e| e.path == "usr/bin/foo").unwrap();
    assert_eq!(foo.mode, 0o755);
    let db::Kind::File(sha) = &foo.kind else {
        panic!("usr/bin/foo is not a file in the manifest")
    };

    let out = std::process::Command::new("sha256sum")
        .arg(dest.join("usr/bin/foo"))
        .output()
        .unwrap();
    let want = String::from_utf8_lossy(&out.stdout);
    assert_eq!(sha, want.split_whitespace().next().unwrap());
}

#[test]
fn a_file_replaces_a_symlink_already_on_disk() {
    let r = root("clobber");
    let src = r.join("src");
    fs::create_dir_all(src.join("usr/bin")).unwrap();
    fs::write(src.join("usr/bin/foo"), b"new").unwrap();
    let arc = r.join("p.tar.zst");
    build(&src, &arc);

    let dest = r.join("dest");
    fs::create_dir_all(dest.join("usr/bin")).unwrap();
    fs::write(dest.join("canary"), b"do not touch").unwrap();
    std::os::unix::fs::symlink("/canary", dest.join("usr/bin/foo")).unwrap();

    archive::extract(&dest, &arc).unwrap();

    assert_eq!(
        fs::read_to_string(dest.join("canary")).unwrap(),
        "do not touch"
    );
    assert_eq!(fs::read_to_string(dest.join("usr/bin/foo")).unwrap(), "new");
    assert!(!dest
        .join("usr/bin/foo")
        .symlink_metadata()
        .unwrap()
        .is_symlink());
}

#[test]
fn setuid_bits_do_not_survive() {
    let r = root("suid");
    let src = r.join("src");
    fs::create_dir_all(src.join("usr/bin")).unwrap();
    let p = src.join("usr/bin/ping");
    fs::write(&p, b"x").unwrap();
    fs::set_permissions(
        &p,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o4755),
    )
    .unwrap();
    let arc = r.join("p.tar.zst");
    build(&src, &arc);

    let dest = r.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let man = archive::extract(&dest, &arc).unwrap();

    let e = man.iter().find(|e| e.path == "usr/bin/ping").unwrap();
    assert_eq!(e.mode, 0o755, "setuid survived into the manifest");
    let on_disk = <fs::Metadata as std::os::unix::fs::MetadataExt>::mode(
        &fs::metadata(dest.join("usr/bin/ping")).unwrap(),
    );
    assert_eq!(on_disk & 0o7000, 0, "setuid survived onto disk");
}

// a real .so, because a file with elf magic and nothing behind it would let a
// scanner that never actually parses anything pass
#[test]
fn installing_a_library_records_what_it_provides() {
    let (root, at) = rooted("provides");
    let src = at.join("g.c");
    fs::write(&src, "int greet(int x){return x+1;}\n").unwrap();
    let so = at.join("libgreet.so.1");

    let built = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,-soname,libgreet.so.1", "-o"])
        .arg(&so)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a shared library"
            );
            return;
        }
    }

    // a second one with a version script, so .gnu.version_d is actually present
    let vs = at.join("v.map");
    fs::write(&vs, "GREET_1 { global: greet; local: *; };\n").unwrap();
    let vso = at.join("libver.so.1");
    assert!(std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,-soname,libver.so.1"])
        .arg(format!("-Wl,--version-script={}", vs.display()))
        .arg("-o")
        .arg(&vso)
        .arg(&src)
        .status()
        .unwrap()
        .success());

    let stage = at.join("stage");
    fs::create_dir_all(stage.join("usr/lib")).unwrap();
    fs::create_dir_all(stage.join("usr/bin")).unwrap();
    fs::copy(&so, stage.join("usr/lib/libgreet.so.1")).unwrap();
    fs::copy(&vso, stage.join("usr/lib/libver.so.1")).unwrap();
    fs::write(stage.join("usr/bin/greet-hi"), "#!/bin/sh\necho hi\n").unwrap();
    // the same library under a second name is one soname, not two
    fs::hard_link(
        stage.join("usr/lib/libgreet.so.1"),
        stage.join("usr/lib/libgreet.so"),
    )
    .unwrap();

    let arc = at.join("libgreet.tar.zst");
    let sh = format!(
        "cd {} && tar cf - . | zstd -q -o {}",
        stage.display(),
        arc.display()
    );
    assert!(std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .status()
        .unwrap()
        .success());
    let meta = at.join("libgreet.tar.zst.meta");
    fs::create_dir_all(&meta).unwrap();
    fs::write(meta.join("name"), "libgreet\n").unwrap();
    fs::write(meta.join("version"), "1.0 1\n").unwrap();
    fs::write(meta.join("targets"), "x86_64-musl\n").unwrap();

    run(&root, &[arc], false).unwrap();

    let got = db::read_provides(&root, "x86_64-musl", "libgreet").unwrap();
    let names: Vec<&str> = got.iter().map(|p| p.soname.as_str()).collect();
    assert_eq!(names, ["libgreet.so.1", "libver.so.1"], "{got:?}");

    let plain = got.iter().find(|p| p.soname == "libgreet.so.1").unwrap();
    assert_eq!(plain.path, "usr/lib/libgreet.so.1");
    assert!(
        !plain.versioned,
        "a plain -shared build defines no versions"
    );

    let ver = got.iter().find(|p| p.soname == "libver.so.1").unwrap();
    assert!(
        ver.versioned,
        "a --version-script build carries .gnu.version_d"
    );

    // the shell script sits in the same package and is not an elf
    assert!(!got.iter().any(|p| p.path.contains("greet-hi")));
}

// --force hands a path to the newcomer, and the database has to stop claiming the old
// owner still has it. two records disagreeing about one path is how the sandbox came to
// rebuild a symlink over a regular file and produce a loop the loader could not walk
#[test]
fn forcing_a_path_takes_it_off_the_previous_owner() {
    let (at, root) = rooted("dispossess");
    let first = pack(&at, "first", &[], &["usr/bin/tool", "usr/share/first/note"]);
    let second = pack(&at, "second", &[], &["usr/bin/tool"]);

    install::apply(&root, &run(&root, &[first], false).unwrap()).unwrap();
    let jobs = run(&root, &[second], true).unwrap();
    install::apply(&root, &jobs).unwrap();

    let old = db::read(&root, "x86_64-musl", "first").unwrap();
    let kept: Vec<&str> = old.manifest.iter().map(|e| e.path.as_str()).collect();
    assert!(
        !kept.contains(&"usr/bin/tool"),
        "first still claims the path second took: {kept:?}"
    );
    assert!(
        kept.contains(&"usr/share/first/note"),
        "the rest of first went with it: {kept:?}"
    );

    let new = db::read(&root, "x86_64-musl", "second").unwrap();
    assert!(new.manifest.iter().any(|e| e.path == "usr/bin/tool"));
}

// provides carries paths too, and it is read by the sandbox to decide which files a
// dependency contributes. leaving the old owner pointing at a file it lost is what made
// doctor report llvm20-libs as stale the moment our own llvm went in over it
#[test]
fn forcing_a_path_takes_it_out_of_provides_as_well() {
    let (at, root) = rooted("dispossess-provides");
    let first = pack(
        &at,
        "first",
        &[],
        &["usr/lib/libthing.so.1", "usr/lib/libkept.so.1"],
    );
    let second = pack(&at, "second", &[], &["usr/lib/libthing.so.1"]);

    install::apply(&root, &run(&root, &[first], false).unwrap()).unwrap();
    db::write_provides(
        &root,
        "x86_64-musl",
        "first",
        &[
            db::Provide {
                soname: "libthing.so.1".into(),
                versioned: false,
                path: "usr/lib/libthing.so.1".into(),
            },
            db::Provide {
                soname: "libkept.so.1".into(),
                versioned: false,
                path: "usr/lib/libkept.so.1".into(),
            },
        ],
    )
    .unwrap();

    install::apply(&root, &run(&root, &[second], true).unwrap()).unwrap();

    let left = db::read_provides(&root, "x86_64-musl", "first").unwrap();
    let paths: Vec<&str> = left.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, vec!["usr/lib/libkept.so.1"], "{paths:?}");
}
