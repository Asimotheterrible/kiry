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

    let p = pkg::load(&dir);

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
        let sum = match p.checksums.get(i) {
            Some(s) => &s[..8],
            None => "--------",
        };
        println!("src {sum} {src}");
    }
}
