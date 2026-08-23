// only the dynamic section. section headers are never read: the loader ignores them
// and they can be stripped off entirely

use std::fs;
use std::path::Path;

use crate::Error;

pub const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_STRSZ: i64 = 10;
const DT_SONAME: i64 = 14;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Elf {
    pub soname: Option<String>,
    pub needed: Vec<String>,
    // rpath applies to transitive loads, runpath does not
    pub rpath: Option<String>,
    pub runpath: Option<String>,
}

pub fn read(p: &Path) -> Result<Elf, Error> {
    // TODO: no cache, so doctor re-reads every library it has already seen
    let b = fs::read(p).map_err(|e| Error::Io(p.to_path_buf(), e))?;
    parse(&b).map_err(|why| Error::Elf {
        path: p.display().to_string(),
        why,
    })
}

pub fn parse(b: &[u8]) -> Result<Elf, &'static str> {
    if !b.starts_with(&MAGIC) {
        return Err("not an elf");
    }
    if b.get(4) != Some(&2) {
        return Err("not 64 bit");
    }
    if b.get(5) != Some(&1) {
        return Err("not little endian");
    }
    if u16at(b, 18) != Some(EM_X86_64) {
        return Err("not x86_64");
    }

    let phoff = at(u64at(b, 32).ok_or("truncated elf header")?)?;
    let phentsize = u16at(b, 54).ok_or("truncated elf header")? as usize;
    let phnum = u16at(b, 56).ok_or("truncated elf header")? as usize;
    if phnum == 0 {
        return Ok(Elf::default());
    }
    // the real count would be in section header 0, which this reader will not read
    if phnum == 0xffff {
        return Err("too many program headers to count here");
    }
    if phentsize < 56 {
        return Err("program header is too small to be one");
    }

    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let start = phoff
            .checked_add(i * phentsize)
            .ok_or("program headers start past the end of memory")?;
        let ph = b
            .get(start..)
            .and_then(|s| s.get(..phentsize))
            .ok_or("program headers run past the end of the file")?;

        let offset = u64at(ph, 8).ok_or("short program header")?;
        let vaddr = u64at(ph, 16).ok_or("short program header")?;
        let filesz = u64at(ph, 32).ok_or("short program header")?;
        match u32at(ph, 0).ok_or("short program header")? {
            PT_LOAD => loads.push((vaddr, filesz, offset)),
            PT_DYNAMIC => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }

    // a static binary, and a musl system is full of them
    let Some((dynoff, dynsz)) = dynamic else {
        return Ok(Elf::default());
    };
    let dynoff = at(dynoff)?;
    let dynsz = at(dynsz)?;
    let entries = b
        .get(dynoff..)
        .and_then(|s| s.get(..dynsz))
        .ok_or("dynamic section runs past the end of the file")?;

    let mut needed = Vec::new();
    let (mut soname, mut rpath, mut runpath) = (None, None, None);
    let (mut strtab, mut strsz) = (None, None);

    for e in entries.chunks_exact(16) {
        let (tag, val) = match (e.get(..8), e.get(8..16)) {
            (Some(t), Some(v)) => (
                i64::from_le_bytes(t.try_into().map_err(|_| "short dynamic entry")?),
                u64::from_le_bytes(v.try_into().map_err(|_| "short dynamic entry")?),
            ),
            _ => return Err("short dynamic entry"),
        };
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed.push(val),
            DT_SONAME => soname = Some(val),
            DT_RPATH => rpath = Some(val),
            DT_RUNPATH => runpath = Some(val),
            DT_STRTAB => strtab = Some(val),
            DT_STRSZ => strsz = Some(val),
            _ => {}
        }
    }

    if needed.is_empty() && soname.is_none() && rpath.is_none() && runpath.is_none() {
        return Ok(Elf::default());
    }

    // dt_strtab is an address, not a file offset. map it through pt_load the way
    // the loader does
    let strtab = at(offset_of(
        &loads,
        strtab.ok_or("names to look up but no string table")?,
    )?)?;
    let strsz = at(strsz.ok_or("a string table of no stated size")?)?;
    let strs = b
        .get(strtab..)
        .and_then(|s| s.get(..strsz))
        .ok_or("string table runs past the end of the file")?;

    Ok(Elf {
        soname: soname.map(|v| string(strs, v)).transpose()?,
        needed: needed
            .into_iter()
            .map(|v| string(strs, v))
            .collect::<Result<_, _>>()?,
        rpath: rpath.map(|v| string(strs, v)).transpose()?,
        runpath: runpath.map(|v| string(strs, v)).transpose()?,
    })
}

fn offset_of(loads: &[(u64, u64, u64)], addr: u64) -> Result<u64, &'static str> {
    for &(vaddr, filesz, offset) in loads {
        if addr >= vaddr && addr - vaddr < filesz {
            return offset
                .checked_add(addr - vaddr)
                .ok_or("segment offset past the end of memory");
        }
    }
    Err("string table is in no loadable segment")
}

fn string(strs: &[u8], from: u64) -> Result<String, &'static str> {
    let from = at(from)?;
    let rest = strs
        .get(from..)
        .ok_or("name starts past the string table")?;
    let end = rest
        .iter()
        .position(|&c| c == 0)
        .ok_or("name runs off the end of the string table")?;
    std::str::from_utf8(&rest[..end])
        .map(String::from)
        .map_err(|_| "name is not utf-8")
}

fn at(v: u64) -> Result<usize, &'static str> {
    usize::try_from(v).map_err(|_| "offset does not fit in this machine")
}

fn u16at(b: &[u8], from: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(from..)?.get(..2)?.try_into().ok()?,
    ))
}

fn u32at(b: &[u8], from: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(from..)?.get(..4)?.try_into().ok()?,
    ))
}

fn u64at(b: &[u8], from: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(from..)?.get(..8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DIRS: &[&str] = &[
        "/lib64",
        "/usr/lib64",
        "/lib",
        "/usr/lib",
        // debian and ubuntu keep them a level down, under the target triplet
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
    ];

    fn libraries() -> Vec<PathBuf> {
        let mut out = Vec::new();
        for d in DIRS {
            let Ok(rd) = fs::read_dir(d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let named_so = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".so"));
                if named_so && p.is_file() {
                    out.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn reads_every_shared_library_here() {
        let libs = libraries();
        if libs.len() < 20 {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "only found {} libraries in {DIRS:?}",
                libs.len()
            );
            return;
        }

        let mut named = 0;
        for p in &libs {
            let Ok(b) = fs::read(p) else { continue };
            if !b.starts_with(&MAGIC) {
                continue; // libc.so and friends are linker scripts, plain text
            }
            match parse(&b) {
                Ok(e) => {
                    if let Some(s) = &e.soname {
                        assert!(!s.is_empty(), "{}: empty soname", p.display());
                        named += 1;
                    }
                }
                // a multilib box has 32 bit copies sitting in the same directories
                Err("not 64 bit") | Err("not x86_64") => {}
                Err(why) => panic!("{}: {why}", p.display()),
            }
        }
        assert!(named > 10, "only {named} of {} had a soname", libs.len());
    }

    // what a binary asks for is the name the library calls itself
    #[test]
    fn sonames_agree_with_whatever_asks_for_them() {
        let me = read(&std::env::current_exe().unwrap()).unwrap();
        let mut checked = 0;
        for want in &me.needed {
            for d in DIRS {
                let p = PathBuf::from(d).join(want);
                if !p.is_file() {
                    continue;
                }
                let lib = read(&p).unwrap();
                assert_eq!(
                    lib.soname.as_deref(),
                    Some(want.as_str()),
                    "{}",
                    p.display()
                );
                checked += 1;
                break;
            }
        }
        if checked == 0 {
            assert!(
                std::env::var("KIRY_TEST_ALLOW_SKIP").is_ok(),
                "resolved none of {:?} in {DIRS:?}",
                me.needed
            );
        }
    }

    // the test binary, so these do not care what the machine has installed
    fn sample() -> Vec<u8> {
        let p = std::env::current_exe().unwrap();
        let b = fs::read(&p).unwrap();
        assert!(b.starts_with(&MAGIC), "{} is not an elf", p.display());
        b
    }

    // doctor walks whatever is on disk, so this has to error rather than panic
    #[test]
    fn a_truncated_elf_never_panics() {
        let b = sample();
        for n in [
            0,
            1,
            3,
            4,
            5,
            6,
            16,
            32,
            52,
            56,
            64,
            100,
            512,
            4096,
            b.len() / 2,
        ] {
            let _ = parse(&b[..n.min(b.len())]);
        }
    }

    #[test]
    fn a_corrupt_elf_never_panics() {
        let b = sample();
        // the headers hold every offset, and a fixed stride reproduces
        for i in (0..b.len().min(8192)).step_by(13) {
            let mut c = b.clone();
            c[i] = c[i].wrapping_add(0x5b);
            let _ = parse(&c);
        }
    }

    // a .o has no program headers and says so with a zero entry size
    #[test]
    fn something_with_no_program_headers_reports_nothing() {
        let mut b = sample();
        b[54] = 0; // e_phentsize
        b[55] = 0;
        b[56] = 0; // e_phnum
        b[57] = 0;
        assert_eq!(parse(&b), Ok(Elf::default()));
    }
}
