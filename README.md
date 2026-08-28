# kiry

**a source-based package manager that checks what it built instead of trusting the recipe**

WIP, dont use it in your system yet, or use it idc

no runtime dependencies, packages are plain text, recipes are POSIX `sh`.

## The main idea
**Basically kiry reads the dependencies of a package from the build binary instead of trusting the 
Maintainers list of dependencies**, it scans the compiled programs and shared libraries, checks which
other libraries actually link to it and then uses that information to decide what needs to be installed
and later what needs rebuilding. There will be also more features to kiry.

Long Version:
Source-based package managers run on facts a maintainer declares: what a library provides,
which ABI it has, what it depends on, which compiler flags are safe, what needs rebuilding
when it changes. Those declarations work, and thousands of people maintain them. They are
still claims.

kiry checks them against the artifact instead. An ELF already carries `DT_SONAME`,
`DT_NEEDED`, its rpath and runpath, and its dynamic symbols with their versions, so the
library that was just built can answer the question the metadata was answering on its
behalf.

Build flags get the same treatment. A flag that was supposed to link something either
shows up in `DT_NEEDED` afterwards or it did nothing. And when a compiler failure matches
a signature kiry recognises, the rule that fixed it is written back beside the package
with the date, so the same fight does not happen twice.

WIP, nothing reads a build log yet.

## Current state

The lifecycle works against a real root:

```
kiry b [--root DIR] [--target T] [-v] <recipe>   build
kiry i [--root DIR] [--force] <archive>          install
kiry r [--root DIR] [--force] <package>          remove
kiry l [--root DIR]                              list installed
kiry doctor [--root DIR]                         check linkage
```

`--root` defaults to `/`, or whatever `KIRY_ROOT` says, and it is how you try any of this
without touching your own system.

`b` fetches sources, checks them against `checksums`, runs the recipe with `DESTDIR` set,
and packs a `.tar.zst` with its metadata beside it. `i` plans a whole batch before
applying any of it, refuses conflicts, extracts, and records a manifest with a sha256 per
file. `r` unlinks only the files whose contents still match that manifest, leaves anything
modified where it is and counts it separately, and will not touch a package that something
installed still links against.

`doctor` resolves every `DT_NEEDED` the way that target's loader would, rather than
believing what the metadata says. A clean root prints nothing. A broken one prints one
line per finding:

```
usr/bin/foo x86_64-gnu unresolved libbar.so.1
usr/bin/baz x86_64-musl unreadable
libz x86_64-musl stale-provides
```

Nothing provides libbar anywhere foo looks. The manifest lists baz and the disk does not
have it. And libz's recorded sonames disagree with the library sitting there now.

A build needs a toolchain in the root before any of this runs. The sandbox holds the
declared closure and nothing else, so on an empty root `b` stops at the first package it
cannot find rather than falling back to the host's shell.

## ELF-first package management (WIP)

The reader pulls out `DT_SONAME`, `DT_NEEDED`, rpath, runpath, and the dynamic symbol
table with versions, default flags, bindings and sizes. That is everything an ABI decision
needs:

```
soname change
    ↓
everything that links it
    ↓
symbol-level comparison
    ↓
the consumers that actually broke
    ↓
rebuild
```

The rebuild happens because the artifact says a consumer broke, not because someone
remembered to update another piece of metadata.

WIP, the reader is done. Everything after the first arrow is not written yet.

## Builds are sandboxed (WIP)

A build gets its declared dependency closure and nothing else, through user, mount and
network namespaces and `pivot_root`. A missing header is missing rather than hidden, and a
library that happens to be installed on the host does not become a dependency just because
the compiler found it.

WIP, it runs builds today, but the only closures it has ever assembled are the test suite's
and one hand written package. The toolchain still goes in by hand.

## Packages are directories

```
foo/
├── build
├── version
├── sources
├── checksums
├── depends
└── targets
```

`build` is POSIX shell, and there is no DSL. An archive carries the same fields again in a
`.meta` directory beside it, so one parser reads both. Everything is plain text and
everything survives `grep`.

## The installer never shells out

Extraction, hashing, ELF parsing and the installed database are Rust. kiry is what you
reach for when a libc upgrade has gone wrong and nothing dynamically linked will start, so
it cannot itself need `sh` or `tar` to fix that.

Extraction has two containment layers:

```
plan-time path validation
        +
openat2(RESOLVE_BENEATH | NO_MAGICLINKS)
```

The first rejects hostile paths and symlink chains before anything is written. The kernel
is the layer that still holds when the first one is wrong.

## Testing

kiry checks its parsers against tools that already know the answer, never against fixtures
written by the same code. Archive extraction runs through `tar`, `bsdtar` and busybox
`tar`, and all three have to produce the same tree. ELF parsing is compared against
`readelf` across real shared libraries and executables. Manifest hashes go to `sha256sum`,
and a C compiler supplies the awkward cases, like an address that has to be mapped back to
a file offset.

A missing test tool fails the suite instead of silently reducing coverage.

## Why this exists

Metadata can describe reality. The open question is how much of it a package manager can
stop asking people to maintain, once the artifact is able to answer for itself.

kiry will attempt to figure that out.

## WIP

Interfaces, package metadata, build isolation, ABI handling and the on-disk database all
still move. Some of it is built. Most of the interesting part is not.

## Build

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release
```

x86_64 Linux only.

## License

MIT
