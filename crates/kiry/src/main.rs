use std::path::PathBuf;

use kiry_core::{db, install, pkg};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => usage(),
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
    println!("usage: kiry i [--root DIR] [--force] <archive>...");
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
