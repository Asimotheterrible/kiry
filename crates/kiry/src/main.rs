use std::path::PathBuf;

use kiry_core::pkg;

fn main() {
    let dir = match std::env::args().nth(1) {
        Some(a) => PathBuf::from(a),
        None => {
            eprintln!("usage: kiry <package dir>");
            std::process::exit(2);
        }
    };

    let p = match pkg::load(&dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kiry: {e}");
            std::process::exit(1);
        }
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
        // dbg!(&p.checksums);
        // get(..8) rather than a slice: nothing has checked these are sha256 yet
        let sum = p
            .checksums
            .get(i)
            .and_then(|s| s.get(..8))
            .unwrap_or("--------");
        println!("src {sum} {src}");
    }
}
