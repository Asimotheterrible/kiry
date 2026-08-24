// the reader against the tool everyone else already trusts, over whatever this
// machine has installed. same reason the tar suite runs three tars: a fixture
// written by the understanding that wrote the parser agrees with it by
// construction and proves nothing

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use kiry_core::elf;

const DIRS: &[&str] = &[
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
    // debian and ubuntu keep them a level down, under the target triplet
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
    "/usr/bin",
    "/usr/libexec",
];

#[derive(Debug, Default, PartialEq, Eq)]
struct Dyn {
    soname: Option<String>,
    needed: Vec<String>,
    rpath: Option<String>,
    runpath: Option<String>,
}

// the filter is on raw bytes, not on whether our own parser liked the file, or
// the corpus would quietly become "everything we already handle"
fn corpus() -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    // /lib64 and /usr/lib64 are the same directory on a usr-merged system, so the
    // same file turns up twice without this
    let mut seen = BTreeSet::new();
    for d in DIRS {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let real = p.canonicalize().unwrap_or_else(|_| p.clone());
            if !seen.insert(real) {
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            let x86_64 = b.starts_with(&elf::MAGIC)
                && b.get(4) == Some(&2)
                && b.get(5) == Some(&1)
                && b.get(18..20) == Some(&[62, 0][..]);
            if x86_64 {
                out.push((p, b));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn bracketed(line: &str) -> Option<String> {
    let open = line.rfind('[')?;
    let close = line.rfind(']')?;
    Some(line.get(open + 1..close)?.to_string())
}

fn readelf(paths: &[&PathBuf]) -> BTreeMap<PathBuf, Dyn> {
    let out = Command::new("readelf")
        .arg("-d")
        .args(paths)
        .output()
        .expect("readelf failed to run");
    let text = String::from_utf8_lossy(&out.stdout);

    let mut map = BTreeMap::new();
    // readelf prints no File: header when it was handed exactly one file
    let mut cur: Option<PathBuf> = match paths {
        [only] => Some((*only).clone()),
        _ => None,
    };
    let mut d = Dyn::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("File: ") {
            if let Some(p) = cur.take() {
                map.insert(p, std::mem::take(&mut d));
            }
            cur = Some(PathBuf::from(rest.trim()));
        } else if line.contains("(NEEDED)") {
            if let Some(v) = bracketed(line) {
                d.needed.push(v);
            }
        } else if line.contains("(SONAME)") {
            d.soname = bracketed(line);
        } else if line.contains("(RUNPATH)") {
            d.runpath = bracketed(line);
        } else if line.contains("(RPATH)") {
            d.rpath = bracketed(line);
        }
    }
    if let Some(p) = cur {
        map.insert(p, d);
    }
    map
}

#[test]
fn agrees_with_readelf_on_every_library_here() {
    if Command::new("readelf").arg("-v").output().is_err() {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "readelf is not installed"
        );
        return;
    }

    let corpus = corpus();
    if corpus.len() < 100 {
        assert!(
            std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
            "only found {} elf files in {DIRS:?}, which is not a corpus. set \
             KIRY_TEST_ALLOW_SKIP=1 if this machine really keeps them somewhere else",
            corpus.len()
        );
        return;
    }

    let paths: Vec<&PathBuf> = corpus.iter().map(|(p, _)| p).collect();
    let theirs = readelf(&paths);
    assert_eq!(theirs.len(), corpus.len(), "readelf skipped some of them");

    let mut sonames = 0;
    let mut rpaths = 0;
    for (p, bytes) in &corpus {
        let got = elf::parse(bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let want = &theirs[p];

        assert_eq!(got.soname, want.soname, "soname of {}", p.display());
        assert_eq!(got.needed, want.needed, "needed of {}", p.display());
        assert_eq!(got.rpath, want.rpath, "rpath of {}", p.display());
        assert_eq!(got.runpath, want.runpath, "runpath of {}", p.display());

        sonames += usize::from(got.soname.is_some());
        rpaths += usize::from(got.rpath.is_some() || got.runpath.is_some());
    }

    // if the corpus somehow held nothing interesting the comparison above is empty
    assert!(sonames > 10, "only {sonames} sonames in {}", corpus.len());
    assert!(rpaths > 0, "no library carried an rpath or runpath");
}

fn strtab_addr(p: &PathBuf) -> u64 {
    let out = Command::new("readelf").arg("-d").arg(p).output().unwrap();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("(STRTAB)") {
            if let Some(hex) = line.split_whitespace().last() {
                if let Some(h) = hex.strip_prefix("0x") {
                    if let Ok(v) = u64::from_str_radix(h, 16) {
                        return v;
                    }
                }
            }
        }
    }
    panic!("no STRTAB in {}", p.display());
}

// DT_STRTAB is an address, not a file offset, and on a PIE binary the two happen
// to be equal - so a reader that never maps one to the other agrees with readelf
// on every shared library installed here. -no-pie is what pulls them apart
#[test]
fn maps_a_string_table_that_is_not_where_its_address_says() {
    let dir = std::env::temp_dir().join(format!("kiry-elf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.c");
    std::fs::write(&src, "int main(void){return 0;}\n").unwrap();
    let bin = dir.join("t");

    let built = Command::new("cc")
        .arg("-no-pie")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a -no-pie binary"
            );
            return;
        }
    }

    let b = std::fs::read(&bin).unwrap();
    let addr = strtab_addr(&bin);
    assert!(
        addr > b.len() as u64,
        "-no-pie put the string table at {addr:#x} inside a {} byte file, so this \
         test no longer proves the address ever gets mapped",
        b.len()
    );

    let got = elf::parse(&b).unwrap();
    let want = readelf(&[&bin]);
    assert_eq!(
        got.needed, want[&bin].needed,
        "needed of the -no-pie binary"
    );
    assert!(
        !got.needed.is_empty(),
        "a dynamic binary with no DT_NEEDED?"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[derive(Debug, PartialEq, Eq)]
struct RSym {
    name: String,
    weak: bool,
    object: bool,
    size: u64,
    undefined: bool,
}

// Num: Value Size Type Bind Vis Ndx Name[@ver|@@ver][ (n)]
fn dyn_syms(paths: &[&PathBuf]) -> BTreeMap<PathBuf, Vec<RSym>> {
    let out = Command::new("readelf")
        .args(["--dyn-syms", "-W"])
        .args(paths)
        .output()
        .expect("readelf failed to run");
    let text = &String::from_utf8_lossy(&out.stdout);

    let mut map: BTreeMap<PathBuf, Vec<RSym>> = BTreeMap::new();
    // no File: header comes back when readelf was handed exactly one file
    let mut cur: Option<PathBuf> = match paths {
        [only] => {
            map.insert((*only).clone(), Vec::new());
            Some((*only).clone())
        }
        _ => None,
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("File: ") {
            cur = Some(PathBuf::from(rest.trim()));
            map.entry(cur.clone().unwrap()).or_default();
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || !f[0].ends_with(':') {
            continue;
        }
        let Ok(size) = f[2].parse::<u64>() else {
            continue;
        };
        let (kind, bind, ndx) = (f[3], f[4], f[6]);
        // readelf glues the version onto the name. versions get their own commit,
        // so cut it off here
        let name = f[7].split('@').next().unwrap_or(f[7]).to_string();
        if name.is_empty() {
            continue;
        }
        let s = RSym {
            name,
            weak: bind == "WEAK",
            object: kind == "OBJECT",
            size,
            undefined: ndx == "UND",
        };
        if let Some(p) = &cur {
            map.entry(p.clone()).or_default().push(s);
        }
    }
    map
}

fn mine(e: &elf::Elf) -> Vec<RSym> {
    let f = |s: &elf::Sym, undefined: bool| RSym {
        name: s.name.clone(),
        weak: s.weak,
        object: s.object,
        size: s.size,
        undefined,
    };
    let mut v: Vec<RSym> = e.exports.iter().map(|s| f(s, false)).collect();
    v.extend(e.undefined.iter().map(|s| f(s, true)));
    v
}

#[test]
fn agrees_with_readelf_on_dynamic_symbols() {
    let corpus = corpus();
    if corpus.len() < 100 {
        assert!(std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(), "no corpus");
        return;
    }
    // every Nth, so it stays a spread across the tree without running readelf
    // over a million symbols
    let sample: Vec<&(PathBuf, Vec<u8>)> = corpus.iter().step_by(corpus.len() / 120).collect();
    let paths: Vec<&PathBuf> = sample.iter().map(|(p, _)| p).collect();

    let theirs = dyn_syms(&paths);

    let mut compared = 0;
    let mut objects = 0;
    let mut undef = 0;
    for (p, bytes) in &sample {
        let got = elf::parse(bytes).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let want = theirs
            .get(p)
            .unwrap_or_else(|| panic!("readelf skipped {}", p.display()));

        let mut a = mine(&got);
        let mut b: Vec<&RSym> = want.iter().collect();
        a.sort_by(|x, y| (&x.name, x.size).cmp(&(&y.name, y.size)));
        b.sort_by(|x, y| (&x.name, x.size).cmp(&(&y.name, y.size)));

        assert_eq!(a.len(), b.len(), "symbol count for {}", p.display());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(&x, y, "symbol mismatch in {}", p.display());
        }

        compared += a.len();
        objects += a.iter().filter(|s| s.object).count();
        undef += a.iter().filter(|s| s.undefined).count();
    }

    assert!(compared > 5000, "only compared {compared} symbols");
    assert!(objects > 100, "only {objects} data symbols");
    assert!(undef > 100, "only {undef} undefined symbols");
}

// nothing installed here carries DT_HASH any more, the linker has defaulted to the
// gnu table for years. --hash-style=sysv is the only way to reach that path
#[test]
fn counts_symbols_out_of_the_old_hash_table_too() {
    let dir = std::env::temp_dir().join(format!("kiry-sysv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("s.c");
    std::fs::write(
        &src,
        "int alpha(int x){return x+1;}\nint beta(int x){return x+2;}\ndouble gamma_data = 3.0;\n",
    )
    .unwrap();
    let lib = dir.join("libsysv.so");

    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,--hash-style=sysv", "-o"])
        .arg(&lib)
        .arg(&src)
        .status();
    match built {
        Ok(s) if s.success() => {}
        _ => {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "cc cannot build a sysv-hash library"
            );
            return;
        }
    }

    let b = std::fs::read(&lib).unwrap();
    // prove the fixture reaches the branch it exists for
    let tags = Command::new("readelf")
        .arg("-d")
        .arg(&lib)
        .output()
        .unwrap();
    let tags = String::from_utf8_lossy(&tags.stdout);
    assert!(tags.contains("(HASH)"), "no DT_HASH, fixture is pointless");
    assert!(
        !tags.contains("GNU_HASH"),
        "GNU_HASH present too, so the gnu branch wins and this proves nothing"
    );

    let got = elf::parse(&b).unwrap();
    let want = dyn_syms(&[&lib]);
    let want = &want[&lib];

    assert_eq!(got.exports.len() + got.undefined.len(), want.len());
    for n in ["alpha", "beta", "gamma_data"] {
        assert!(
            got.exports.iter().any(|s| s.name == n),
            "{n} missing from {:?}",
            got.exports.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
    assert!(got
        .exports
        .iter()
        .any(|s| s.name == "gamma_data" && s.object && s.size == 8));

    let _ = std::fs::remove_dir_all(&dir);
}
