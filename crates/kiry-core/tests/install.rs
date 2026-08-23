// end to end against real archives built by the real tools. this shells out, which
// is exactly why it lives in tests/ rather than inside kiry-core

use std::fs;
use std::path::{Path, PathBuf};

use kiry_core::{archive, db, install, Error};
use kiry_core::install::Job;
use kiry_core::archive::{Member, What};

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
    let src = at.join(format!("{name}-src"));
    for f in files {
        let p = src.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, name.as_bytes()).unwrap();
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

#[test]
fn removing_takes_the_files_and_the_record() {
    let (d, root) = rooted("rm");
    run(&root, &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])], false).unwrap();

    let r = install::remove(&root, "x86_64-musl", "foo", false).unwrap();
    assert_eq!(r.gone, 1);
    assert_eq!(r.kept, 0);
    assert!(!root.join("usr/share/doc/foo").exists());
    assert!(db::read(&root, "x86_64-musl", "foo").is_err());
}

#[test]
fn a_modified_file_is_left_where_it_is() {
    let (d, root) = rooted("modified");
    run(&root, &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])], false).unwrap();
    fs::write(root.join("usr/share/doc/foo/x"), b"edited by hand").unwrap();

    let r = install::remove(&root, "x86_64-musl", "foo", false).unwrap();
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
    run(&root, &[pack(&d, "foo", &[], &["usr/share/doc/foo/x"])], false).unwrap();
    let p = root.join("usr/share/doc/foo/x");
    fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &p).unwrap();

    assert_eq!(install::remove(&root, "x86_64-musl", "foo", false).unwrap().kept, 1);
    assert!(p.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn a_dependent_blocks_removal() {
    let (d, root) = rooted("needed");
    let lib = pack(&d, "libbar", &[], &["usr/share/doc/bar/x"]);
    let app = pack(&d, "foo", &["libbar"], &["usr/share/doc/foo/x"]);
    run(&root, &[lib, app], false).unwrap();

    match install::remove(&root, "x86_64-musl", "libbar", false) {
        Err(Error::Needed { pkg, by }) => {
            assert_eq!((pkg.as_str(), by.as_str()), ("libbar", "foo"));
        }
        other => panic!("wanted Needed, got {other:?}"),
    }
    assert!(install::remove(&root, "x86_64-musl", "libbar", true).is_ok());
}

#[test]
fn protected_directories_survive() {
    let (d, root) = rooted("protected");
    run(&root, &[pack(&d, "foo", &[], &["usr/bin/foo"])], false).unwrap();

    install::remove(&root, "x86_64-musl", "foo", false).unwrap();
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

    install::remove(&root, "x86_64-musl", "foo", false).unwrap();
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
    assert!(!got.iter().any(|p| p.is_empty()), "the ./ root leaked in: {got:?}");
    assert!(
        m.iter().any(|e| e.what == What::Link("foo".into())),
        "the symlink did not survive: {m:?}"
    );
}


fn build(dir: &Path, arc: &Path) {
    let sh = format!("cd {} && tar cf - . | zstd -q -o {}", dir.display(), arc.display());
    let ok = std::process::Command::new("sh").arg("-c").arg(&sh).status().unwrap();
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

    assert_eq!(fs::read_to_string(dest.join("usr/bin/foo")).unwrap(), "hello there");
    assert_eq!(
        fs::read_link(dest.join("usr/bin/bar")).unwrap().display().to_string(),
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

    assert_eq!(fs::read_to_string(dest.join("canary")).unwrap(), "do not touch");
    assert_eq!(fs::read_to_string(dest.join("usr/bin/foo")).unwrap(), "new");
    assert!(!dest.join("usr/bin/foo").symlink_metadata().unwrap().is_symlink());
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

