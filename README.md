# kiry

A source-based package manager that checks the artifact it built instead of trusting
what the recipe claimed.

Written in Rust, statically linked against musl, no runtime dependencies. Packages are
directories of plain text; build recipes are POSIX sh.

## Why another one

Package managers rely on maintainers declaring things. Portage has subslots for ABI
compatibility, curated `filter-lto` lists for the compilers that miscompile, IUSE for
which options a package even has. That works, because thousands of people maintain those
declarations.

kiry reads the same facts back out of what it just built.

The ELF already knows its own ABI. `DT_SONAME` and `DT_NEEDED` say what a library
provides and what links against it, so a soname change gives you the exact rebuild set
with nobody writing a subslot. Diffing the exported symbols then cancels most of those
rebuilds again, since a new ABI that turns out to be a superset of the old one cannot
break anything that linked against it.

Compiler flags get decided by building the package and reading the failure. A signature
table matches the errors that recur; anything it misses falls through to a flag ladder.
Whatever survives is written back into the package with the rule that fired and the date
beside it, so the same fight does not happen twice.

Checking whether a build option did anything is the crude one. If the flag was supposed
to link some library, that library is in `DT_NEEDED` afterwards, or the flag did nothing.

That is why one maintainer is enough.

## Status

Early. It reads the package format (`version`, `sources`, `checksums`, `depends`,
`targets`) and prints back what it parsed. Nothing gets installed yet.

Extraction, the installed database and `l` `i` `r` come next. **The soname engine, which
is the part that is actually novel, has not been written, but it WILL BE written.**

Do not point this at your system **yet**.

## Building

```sh
rustup target add x86_64-unknown-linux-musl     # not installed by default
cargo build --release
```

`cargo test` shells out to `tar` and `zstd`. One test builds a real archive with them
rather than a synthetic fixture, because a synthetic fixture is exactly what was happy
with code that could not read a normal tarball. Extraction adds `libarchive-tools`
(bsdtar), `busybox-static` and a `sha256sum`, since the suite then builds one tree with
GNU tar, bsdtar and busybox tar and demands all three extract identically. The target
system runs a busybox userland, and tar implementations disagree about long names,
sparse files and pax records in ways that reach the installed root.

A missing tool fails the suite rather than skipping it. `KIRY_TEST_ALLOW_SKIP=1` is the
override. A suite that reports success while testing a third of what it claims is worse
than a red one, which is how the previous attempt shipped two bugs.

## Package format

A directory of plain text. Greppable, hand-editable, no database.

```
extra/foo/
├── build         POSIX sh, DESTDIR-aware
├── version       "1.2.3 1"          upstream version, package revision
├── sources       one URL per line
├── checksums     sha256, positionally matching sources
├── depends       one name per line; " make" suffix = build-only
└── …             optional: flags, filter, group, tracker, pin, policy
```

An archive carries the same fields again in a `.meta` directory beside it. The name is
appended to the whole file name rather than derived from it, because an archive's name
is a display name and nothing parses it:

```
foo-1.2.3-1.x86_64-musl.tar.zst
foo-1.2.3-1.x86_64-musl.tar.zst.meta/
├── name        required
├── version     required, same "1.2.3 1" as the recipe
├── targets     required, and exactly one line, unlike the recipe's
├── depends     optional, copied from the recipe
└── hash        the source hash, for the provenance check; i does not read it yet
```

Whatever builds the archive writes this directory too. Building one by hand means
writing it by hand, or `i` has nothing to go on.

Nothing generated lives in the repo. The manifest, `provides`, `status` and the soname
index all live under `/usr/lib/kiry/db/`, inside the root subvolume, so they roll back
atomically with the files they describe. `/var/kiry/` holds caches, staging and the
content-addressed store, all of which can be rebuilt from scratch, so deleting it loses
nothing.

## Commands

```
kiry <dir>          print a parsed package
```

Planned, and none of them written yet:

```
kiry l              list installed
kiry i <pkg>        install
kiry r <pkg>        remove
kiry b <pkg>        build into a staging root
kiry doctor         verify every installed ELF's linkage resolves
kiry why <pkg>      reverse-dependency path
kiry owns <path>    which package owns a file
kiry sync           bump job
kiry promote        testing/ to extra/
kiry news           show / acknowledge
kiry rebuild        drain the soname rebuild queue
kiry stats          build times and cache hit rates
```

## Design constraints

The install path never shells out. kiry is what you reach for when a libc upgrade has
gone wrong and nothing dynamically linked will start, so it cannot itself need `sh`,
`tar` or `sha256sum`. Extraction, hashing and the installed database are pure Rust in
`kiry-core`. Fetching, running builds and converting recipes may shell out freely, and
they live in the `kiry` crate. That boundary is the only reason there are two crates.

Recipes stay POSIX sh. The package manager is Rust; packages are shell. No DSL.

Containment is two layers. Path resolution refuses hostile archives at plan time, before
anything is written, including symlink chains where neither link is on disk yet.
Underneath it, `openat2(RESOLVE_BENEATH | NO_MAGICLINKS)` means nothing lands outside
the root even when that logic is wrong. The closest comparable C implementation has had
three remote code execution CVEs in exactly this code path.

## Non-goals

kiry is not a distribution. It is written for one, but that system's design lives
outside this repository.

Nor is it a binary package manager, though it will install a prebuilt artifact where
building one makes no sense. 

Portability stops at x86_64 Linux.

And it does not accept the premise that a package manager should be trusted rather than
checked.

## License

MIT.
