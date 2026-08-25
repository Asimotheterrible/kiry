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
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_SONAME: i64 = 14;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;
const DT_GNU_HASH: i64 = 0x6fff_fef5;
const DT_VERSYM: i64 = 0x6fff_fff0;
const DT_VERDEF: i64 = 0x6fff_fffc;
const DT_VERDEFNUM: i64 = 0x6fff_fffd;
const DT_VERNEED: i64 = 0x6fff_fffe;
const DT_VERNEEDNUM: i64 = 0x6fff_ffff;

// the version index's high bit doubles as the not-default flag
const VERSYM_HIDDEN: u16 = 0x8000;
// a verdef entry describing the file itself rather than a version it exports
const VER_FLG_BASE: u16 = 1;
const STB_WEAK: u8 = 2;
const STT_OBJECT: u8 = 1;
const SHN_UNDEF: u16 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sym {
    pub name: String,
    pub version: Option<String>,
    // foo@@v2 rather than foo@v1. new links bind to the default, so moving which
    // version is default is an abi change even with both symbols still present
    pub default: bool,
    pub weak: bool,
    // st_size is the object's size here, and code size on a function, too noisy to diff
    pub object: bool,
    pub size: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Elf {
    pub soname: Option<String>,
    pub needed: Vec<String>,
    // rpath applies to transitive loads, runpath does not
    pub rpath: Option<String>,
    pub runpath: Option<String>,
    pub exports: Vec<Sym>,
    pub undefined: Vec<Sym>,
    // carries .gnu.version_d, which decides whether versions mean anything here
    pub versioned: bool,
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
    let (mut symtab, mut syment) = (None, None);
    let (mut hash, mut gnu_hash, mut versym) = (None, None, None);
    let (mut verdef, mut verdefnum) = (None, 0);
    let (mut verneed, mut verneednum) = (None, 0);

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
            DT_SYMTAB => symtab = Some(val),
            DT_SYMENT => syment = Some(val),
            DT_HASH => hash = Some(val),
            DT_GNU_HASH => gnu_hash = Some(val),
            DT_VERSYM => versym = Some(val),
            DT_VERDEF => verdef = Some(val),
            DT_VERDEFNUM => verdefnum = val,
            DT_VERNEED => verneed = Some(val),
            DT_VERNEEDNUM => verneednum = val,
            _ => {}
        }
    }

    if needed.is_empty()
        && soname.is_none()
        && rpath.is_none()
        && runpath.is_none()
        && symtab.is_none()
    {
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

    let names = versions(b, &loads, strs, verdef, verdefnum, verneed, verneednum)?;
    let (exports, undefined) = symbols(
        b, &loads, strs, &names, symtab, syment, hash, gnu_hash, versym,
    )?;

    Ok(Elf {
        soname: soname.map(|v| string(strs, v)).transpose()?,
        needed: needed
            .into_iter()
            .map(|v| string(strs, v))
            .collect::<Result<_, _>>()?,
        rpath: rpath.map(|v| string(strs, v)).transpose()?,
        runpath: runpath.map(|v| string(strs, v)).transpose()?,
        exports,
        undefined,
        versioned: verdef.is_some(),
    })
}

// there is no DT_SYMSZ. the count comes out of whichever hash table the linker
// emitted, and modern ones emit only the gnu flavour
fn count(
    b: &[u8],
    loads: &[(u64, u64, u64)],
    hash: Option<u64>,
    gnu: Option<u64>,
) -> Result<usize, &'static str> {
    if let Some(addr) = gnu {
        let t = b
            .get(at(offset_of(loads, addr)?)?..)
            .ok_or("gnu hash past the file")?;
        let nbuckets = at(u32at(t, 0).ok_or("short gnu hash")?.into())?;
        let symoffset = at(u32at(t, 4).ok_or("short gnu hash")?.into())?;
        let bloom = at(u32at(t, 8).ok_or("short gnu hash")?.into())?;

        let buckets_at = 16 + bloom * 8;
        let mut last = 0usize;
        for i in 0..nbuckets {
            let v = at(u32at(t, buckets_at + i * 4)
                .ok_or("short gnu hash buckets")?
                .into())?;
            last = last.max(v);
        }
        // every bucket empty means nothing is in the table past symoffset
        if last < symoffset {
            return Ok(symoffset);
        }

        let chain_at = buckets_at + nbuckets * 4;
        let mut i = last - symoffset;
        loop {
            let v = u32at(t, chain_at + i * 4).ok_or("gnu hash chain runs off the end")?;
            // the low bit terminates the chain
            if v & 1 == 1 {
                return Ok(symoffset + i + 1);
            }
            i += 1;
        }
    }

    if let Some(addr) = hash {
        let t = b
            .get(at(offset_of(loads, addr)?)?..)
            .ok_or("hash past the file")?;
        // nchain is the symbol count, by definition
        return at(u32at(t, 4).ok_or("short hash table")?.into());
    }

    Err("a symbol table with no hash table to size it")
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

// verdef is what this file exports, verneed what it asks of others. vn_file is
// ignored: glibc stopped using it for symbol search in 2.30
#[allow(clippy::too_many_arguments)]
fn versions(
    b: &[u8],
    loads: &[(u64, u64, u64)],
    strs: &[u8],
    verdef: Option<u64>,
    verdefnum: u64,
    verneed: Option<u64>,
    verneednum: u64,
) -> Result<Vec<(u16, String)>, &'static str> {
    let mut out = Vec::new();

    if let Some(addr) = verdef {
        let t = b
            .get(at(offset_of(loads, addr)?)?..)
            .ok_or("verdef past the end of the file")?;
        let mut at_ = 0usize;
        for _ in 0..verdefnum {
            let flags = u16at(t, at_ + 2).ok_or("short verdef")?;
            let ndx = u16at(t, at_ + 4).ok_or("short verdef")?;
            let aux = at(u32at(t, at_ + 12).ok_or("short verdef")?.into())?;
            let next = at(u32at(t, at_ + 16).ok_or("short verdef")?.into())?;

            if flags & VER_FLG_BASE == 0 {
                let name = u32at(t, at_ + aux).ok_or("short verdaux")?;
                out.push((ndx, string(strs, name.into())?));
            }
            if next == 0 {
                break;
            }
            at_ += next;
        }
    }

    if let Some(addr) = verneed {
        let t = b
            .get(at(offset_of(loads, addr)?)?..)
            .ok_or("verneed past the end of the file")?;
        let mut at_ = 0usize;
        for _ in 0..verneednum {
            let cnt = u16at(t, at_ + 2).ok_or("short verneed")?;
            let aux = at(u32at(t, at_ + 8).ok_or("short verneed")?.into())?;
            let next = at(u32at(t, at_ + 12).ok_or("short verneed")?.into())?;

            let mut a = at_ + aux;
            for _ in 0..cnt {
                let other = u16at(t, a + 6).ok_or("short vernaux")?;
                let name = u32at(t, a + 8).ok_or("short vernaux")?;
                let anext = at(u32at(t, a + 12).ok_or("short vernaux")?.into())?;
                out.push((other, string(strs, name.into())?));
                if anext == 0 {
                    break;
                }
                a += anext;
            }
            if next == 0 {
                break;
            }
            at_ += next;
        }
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn symbols(
    b: &[u8],
    loads: &[(u64, u64, u64)],
    strs: &[u8],
    names: &[(u16, String)],
    symtab: Option<u64>,
    syment: Option<u64>,
    hash: Option<u64>,
    gnu: Option<u64>,
    versym: Option<u64>,
) -> Result<(Vec<Sym>, Vec<Sym>), &'static str> {
    // TODO: everyone gets the whole table, even a caller that only wants linkage
    let Some(addr) = symtab else {
        return Ok((Vec::new(), Vec::new()));
    };
    let entsize = at(syment.unwrap_or(24))?;
    if entsize < 24 {
        return Err("symbol entries too small to be ones");
    }

    let n = count(b, loads, hash, gnu)?;
    let start = at(offset_of(loads, addr)?)?;
    let table = b
        .get(start..)
        .and_then(|s| s.get(..n.checked_mul(entsize)?))
        .ok_or("symbol table runs past the end of the file")?;

    let vers = match versym {
        Some(a) => Some(
            b.get(at(offset_of(loads, a)?)?..)
                .ok_or("versym past the end of the file")?,
        ),
        None => None,
    };

    let (mut exports, mut undefined) = (Vec::new(), Vec::new());
    for (i, e) in table.chunks_exact(entsize).enumerate() {
        let name = u32at(e, 0).ok_or("short symbol")?;
        if name == 0 {
            continue; // the null symbol, and anything else unnamed
        }
        let info = *e.get(4).ok_or("short symbol")?;
        let shndx = u16at(e, 6).ok_or("short symbol")?;
        let size = u64at(e, 16).ok_or("short symbol")?;

        let (version, default) = match vers {
            Some(v) => {
                let raw = u16at(v, i * 2).ok_or("versym is shorter than the symbol table")?;
                let ndx = raw & !VERSYM_HIDDEN;
                // 0 is local, 1 is global, and some linkers write 1 where the spec
                // says 0. this and the VER_FLG_BASE skip hide each other: drop one
                // and nothing changes, drop both and index 1 picks up the soname
                if ndx <= 1 {
                    (None, true)
                } else {
                    let found = names
                        .iter()
                        .find(|(k, _)| *k == ndx)
                        .map(|(_, s)| s.clone());
                    (found, raw & VERSYM_HIDDEN == 0)
                }
            }
            None => (None, true),
        };

        let sym = Sym {
            name: string(strs, name.into())?,
            version,
            default,
            weak: info >> 4 == STB_WEAK,
            object: info & 0xf == STT_OBJECT,
            size,
        };
        if shndx == SHN_UNDEF {
            undefined.push(sym);
        } else {
            exports.push(sym);
        }
    }

    Ok((exports, undefined))
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
