use std::path::PathBuf;

use kiry_core::{install, pkg};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => usage(),
        Some("i") => install_cmd(&args[1..]),
        Some(dir) => show(dir),
        None => {
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    println!("usage: kiry i [--root DIR] [--force] <archive>...");
    println!("       kiry <package dir>");
}

fn die(msg: String) -> ! {
    eprintln!("kiry: {msg}");
    std::process::exit(1);
}

fn install_cmd(args: &[String]) {
    let mut root = std::env::var("KIRY_ROOT").unwrap_or_else(|_| "/".into());
    let mut force = false;
    let mut archives = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" => force = true,
            "--root" => match it.next() {
                Some(r) => root = r.clone(),
                None => die("--root wants a path".into()),
            },
            _ => archives.push(PathBuf::from(a)),
        }
    }

    if archives.is_empty() {
        die("nothing to install".into());
    }

    let root = PathBuf::from(root);
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
