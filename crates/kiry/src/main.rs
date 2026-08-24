use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use kiry_core::pkg::Package;
use kiry_core::{db, install, pkg};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => usage(),
        Some("b") => build_cmd(&args[1..]),
        Some("i") => install_cmd(&args[1..]),
        Some("r") => remove_cmd(&args[1..]),
        Some("l") => list_cmd(&args[1..]),
        Some(dir) => show(dir),
        None => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    println!("usage: kiry b [--root DIR] [--target T] [-v] <package dir>...");
    println!("       kiry i [--root DIR] [--force] <archive>...");
    println!("       kiry r [--root DIR] [--force] <pkg>...");
    println!("       kiry l [--root DIR]");
    println!("       kiry <package dir>");
}

fn die(msg: String) -> ! {
    eprintln!("kiry: {msg}");
    std::process::exit(1);
}

fn opts(args: &[String]) -> (PathBuf, bool, Vec<String>) {
    let mut root = std::env::var("KIRY_ROOT").unwrap_or_else(|_| "/".into());
    let mut force = false;
    let mut rest = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" => force = true,
            "--root" => match it.next() {
                Some(r) => root = r.clone(),
                None => die("--root wants a path".into()),
            },
            _ => rest.push(a.clone()),
        }
    }
    (PathBuf::from(root), force, rest)
}

fn build_cmd(args: &[String]) {
    let mut want = None;
    let mut verbose = false;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" => verbose = true,
            "--target" => match it.next() {
                Some(t) => want = Some(t.clone()),
                None => die("--target wants a name".into()),
            },
            _ => rest.push(a.clone()),
        }
    }

    let (root, _, dirs) = opts(&rest);
    if dirs.is_empty() {
        die("nothing to build".into());
    }

    for d in &dirs {
        let p = match pkg::load(Path::new(d)) {
            Ok(p) => p,
            Err(e) => die(e.to_string()),
        };
        let targets = match &want {
            Some(t) if !p.targets.contains(t) => die(format!("{} does not build for {t}", p.name)),
            Some(t) => vec![t.clone()],
            None => p.targets.clone(),
        };
        if let Err(e) = build(&root, &p, &targets, verbose) {
            die(e);
        }
    }

    println!("cached {}", root.join("var/kiry/cache").display());
}

// every target compiles and packs before any of them is renamed into place, so a
// package cannot leave half its targets in the cache
fn build(root: &Path, p: &Package, targets: &[String], verbose: bool) -> Result<(), String> {
    let srcs = sources(root, p)?;
    let hash = recipe_hash(p, &srcs)?;

    let mut built = Vec::new();
    for t in targets {
        let start = Instant::now();
        let work = compile(root, p, t, &srcs, verbose)?;
        println!("{} {} {t} ok {}", p.name, p.version.upstream, took(start));
        built.push((t.clone(), work));
    }

    let mut ready = Vec::new();
    for (t, work) in &built {
        ready.push((t.clone(), pack(root, p, t, work)?));
    }

    // sidecar lands after the rename. ahead of it, a target failing to tar leaves a
    // .meta for an artifact that never arrives
    for (t, (art, part)) in &ready {
        fs::rename(part, art).map_err(|e| format!("{}: {e}", art.display()))?;
        meta(p, t, &hash, art)?;
    }
    for (_, work) in &built {
        let _ = fs::remove_dir_all(work);
    }
    Ok(())
}

fn sources(root: &Path, p: &Package) -> Result<Vec<(PathBuf, String)>, String> {
    let cache = root.join("var/kiry/cache/sources");
    mkdirs(&cache)?;

    let mut out = Vec::new();
    for (i, s) in p.sources.iter().enumerate() {
        let path = if s.contains("://") {
            let name = s.rsplit('/').next().unwrap_or("");
            if name.is_empty() {
                return Err(format!("{s}: no file name at the end of that"));
            }
            let dst = cache.join(name);
            if !dst.exists() {
                grab(s, &dst)?;
            }
            dst
        } else {
            p.dir.join(s)
        };

        let sum = sha(&path)?;
        match p.checksums.get(i) {
            Some(want) if want != &sum => {
                return Err(format!(
                    "{}: checksum is {sum}, recipe says {want}",
                    path.display()
                ))
            }
            Some(_) => {}
            None => eprintln!("kiry: {}: no checksum, sha256 is {sum}", path.display()),
        }
        out.push((path, sum));
    }
    Ok(out)
}

// %u is the url, %o the output. split on whitespace and exec directly, no shell,
// so a url never gets a second round of quoting
fn grab(url: &str, dst: &Path) -> Result<(), String> {
    let mut part = dst.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let tmpl = std::env::var("KIRY_FETCH").unwrap_or_else(|_| "curl -fL --retry 3 -o %o %u".into());
    let mut words = tmpl.split_whitespace();
    let Some(prog) = words.next() else {
        return Err("KIRY_FETCH is empty".into());
    };

    let mut c = Command::new(prog);
    for w in words {
        c.arg(
            w.replace("%o", &part.display().to_string())
                .replace("%u", url),
        );
    }
    run(&mut c, url)?;

    fs::rename(&part, dst).map_err(|e| format!("{}: {e}", dst.display()))
}

fn compile(
    root: &Path,
    p: &Package,
    t: &str,
    srcs: &[(PathBuf, String)],
    verbose: bool,
) -> Result<PathBuf, String> {
    let work = root.join("var/kiry/stage").join(format!(
        "{}-{}-{}.{t}",
        p.name, p.version.upstream, p.version.rev
    ));
    let _ = fs::remove_dir_all(&work);

    let src = work.join("src");
    let dest = work.join("dest");
    mkdirs(&src)?;
    mkdirs(&dest)?;

    for (path, _) in srcs {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if tarball(name) {
            run(
                Command::new("tar").arg("-xf").arg(path).arg("-C").arg(&src),
                name,
            )?;
        } else {
            fs::copy(path, src.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
    }

    let script = p.dir.join("build");
    if !script.is_file() {
        return Err(format!("{}: no build script", p.dir.display()));
    }

    let mut c = Command::new("sh");
    c.arg("-e")
        .arg(abs(&script)?)
        .current_dir(unpacked(&src))
        .env("DESTDIR", abs(&dest)?)
        .env("KIRY_SRCDIR", abs(&src)?)
        .env("KIRY_TARGET", t)
        .env("KIRY_NAME", &p.name)
        .env("KIRY_VERSION", &p.version.upstream)
        .env("KIRY_REV", p.version.rev.to_string());

    // the log is written either way. -v only decides whether you also watch it
    let log = root.join("var/kiry/log").join(format!(
        "{}-{}-{}.{t}.log",
        p.name, p.version.upstream, p.version.rev
    ));
    if !verbose {
        mkdirs(&root.join("var/kiry/log"))?;
        let out = fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("{}: {e}", log.display()))?;
        c.stdout(out).stderr(err);
    }

    match c.status() {
        Ok(s) if s.success() => Ok(work),
        Ok(_) if verbose => Err(format!("{} {t}: build failed", p.name)),
        Ok(_) => Err(format!(
            "{} {t}: build failed, log is {}",
            p.name,
            log.display()
        )),
        Err(e) => Err(format!("{} {t}: {e}", p.name)),
    }
}

// one top directory and nothing else means the build starts inside it, which is
// what every recipe expects
fn unpacked(src: &Path) -> PathBuf {
    let Ok(rd) = fs::read_dir(src) else {
        return src.to_path_buf();
    };

    let mut only = None;
    for e in rd.flatten() {
        if only.is_some() || !e.path().is_dir() {
            return src.to_path_buf();
        }
        only = Some(e.path());
    }
    only.unwrap_or_else(|| src.to_path_buf())
}

fn tarball(name: &str) -> bool {
    name.contains(".tar")
        || name.ends_with(".tgz")
        || name.ends_with(".txz")
        || name.ends_with(".tbz2")
        || name.ends_with(".tzst")
}

fn pack(root: &Path, p: &Package, t: &str, work: &Path) -> Result<(PathBuf, PathBuf), String> {
    let cache = root.join("var/kiry/cache");
    mkdirs(&cache)?;

    let art = cache.join(format!(
        "{}-{}-{}.{t}.tar.zst",
        p.name, p.version.upstream, p.version.rev
    ));
    let mut part = art.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut c = Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(work.join("dest"))
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tar: {e}"))?;

    let Some(pipe) = c.stdout.take() else {
        return Err("tar gave us no pipe".into());
    };

    let z = Command::new("zstd")
        .arg("-19")
        .arg("-T0")
        .arg("-qf")
        .arg("-o")
        .arg(&part)
        .stdin(pipe)
        .status()
        .map_err(|e| format!("zstd: {e}"))?;

    let tar = c.wait().map_err(|e| format!("tar: {e}"))?;
    if !tar.success() {
        return Err(format!("tar: {tar}"));
    }
    if !z.success() {
        return Err(format!("zstd: {z}"));
    }
    Ok((art, part))
}

fn meta(p: &Package, t: &str, hash: &str, art: &Path) -> Result<(), String> {
    let mut d = art.as_os_str().to_owned();
    d.push(".meta");
    let d = PathBuf::from(d);
    let _ = fs::remove_dir_all(&d);
    mkdirs(&d)?;

    put(&d.join("name"), &format!("{}\n", p.name))?;
    put(&d.join("version"), &format!("{}\n", p.version))?;
    put(&d.join("targets"), &format!("{t}\n"))?;
    put(&d.join("hash"), &format!("{hash}\n"))?;

    let deps = p.dir.join("depends");
    if deps.is_file() {
        fs::copy(&deps, d.join("depends")).map_err(|e| format!("{}: {e}", deps.display()))?;
    }
    Ok(())
}

// no target in here on purpose. every target of a package has to come out with the
// same hash, which is the whole check
fn recipe_hash(p: &Package, srcs: &[(PathBuf, String)]) -> Result<String, String> {
    let rd = fs::read_dir(&p.dir).map_err(|e| format!("{}: {e}", p.dir.display()))?;
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    names.sort();

    let mut blob = Vec::new();
    for n in &names {
        let body = fs::read(p.dir.join(n)).map_err(|e| format!("{n}: {e}"))?;
        blob.extend_from_slice(format!("{n} {}\n", body.len()).as_bytes());
        blob.extend_from_slice(&body);
    }
    // anything the build reads is either one of those files or a listed source
    for (_, sum) in srcs {
        blob.extend_from_slice(sum.as_bytes());
        blob.push(b'\n');
    }

    kiry_core::sha256(&blob[..]).map_err(|e| e.to_string())
}

fn run(c: &mut Command, what: &str) -> Result<(), String> {
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{what}: {s}")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

fn sha(p: &Path) -> Result<String, String> {
    let f = fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    kiry_core::sha256(f).map_err(|e| format!("{}: {e}", p.display()))
}

fn abs(p: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn mkdirs(p: &Path) -> Result<(), String> {
    fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn put(p: &Path, s: &str) -> Result<(), String> {
    fs::write(p, s).map_err(|e| format!("{}: {e}", p.display()))
}

fn took(start: Instant) -> String {
    let s = start.elapsed().as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn install_cmd(args: &[String]) {
    let (root, force, names) = opts(args);
    let archives: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    if archives.is_empty() {
        die("nothing to install".into());
    }
    let jobs = match install::plan(&root, &archives, force) {
        Ok(j) => j,
        Err(e) => die(e.to_string()),
    };
    if let Err(e) = install::apply(&root, &jobs) {
        die(e.to_string());
    }

    for j in &jobs {
        println!("{} {} {} ok", j.name, j.version.upstream, j.target);
    }
}

fn remove_cmd(args: &[String]) {
    let (root, force, names) = opts(args);
    if names.is_empty() {
        die("nothing to remove".into());
    }

    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    for name in &names {
        let mut found = false;
        for t in &targets {
            match db::installed(&root, t) {
                Ok(have) if have.contains(name) => {}
                Ok(_) => continue,
                Err(e) => die(e.to_string()),
            }
            found = true;
            match install::remove(&root, t, name, force) {
                Ok(r) => {
                    let mut notes = Vec::new();
                    if r.kept > 0 {
                        notes.push(format!("{} modified, left alone", r.kept));
                    }
                    if r.missing > 0 {
                        notes.push(format!("{} already gone", r.missing));
                    }
                    let note = if notes.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", notes.join(", "))
                    };
                    let s = if r.gone == 1 { "" } else { "s" };
                    println!("{name} {t} removed {} file{s}{note}", r.gone);
                }
                Err(e) => die(e.to_string()),
            }
        }
        if !found {
            die(format!("{name} is not installed"));
        }
    }
}

fn list_cmd(args: &[String]) {
    let (root, _, _) = opts(args);
    let targets = match db::targets(&root) {
        Ok(t) => t,
        Err(e) => die(e.to_string()),
    };

    for t in &targets {
        let names = match db::installed(&root, t) {
            Ok(n) => n,
            Err(e) => die(e.to_string()),
        };
        for n in names {
            match db::read(&root, t, &n) {
                Ok(r) => println!("{} {} {}", r.name, r.version.upstream, t),
                Err(e) => die(e.to_string()),
            }
        }
    }
}

fn show(dir: &str) {
    let dir = PathBuf::from(dir);
    let p = match pkg::load(&dir) {
        Ok(p) => p,
        Err(e) => die(e.to_string()),
    };

    println!("{} {}", p.name, p.version);
    println!("targets {}", p.targets.join(" "));

    for d in &p.depends {
        if d.make {
            println!("dep {} make", d.name);
        } else {
            println!("dep {}", d.name);
        }
    }

    for (i, src) in p.sources.iter().enumerate() {
        let sum = p
            .checksums
            .get(i)
            .and_then(|s| s.get(..8))
            .unwrap_or("--------");
        println!("src {sum} {src}");
    }
}
