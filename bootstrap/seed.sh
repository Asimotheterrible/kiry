#!/bin/sh
here=$(cd "$(dirname "$0")" && pwd)
list=${KIRY_SEED_LIST:-$here/seed.list}
work=${KIRY_SEED:-$HOME/.cache/kiry/seed}
root=${1:-$HOME/.cache/kiry/root}
mirror=${KIRY_MIRROR:-https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64}
target=x86_64-musl

kiry=${KIRY:-}
if [ -z "$kiry" ]; then
    for c in "$here/../target/release/kiry" "$here/../target/debug/kiry" kiry; do
        command -v "$c" >/dev/null 2>&1 && { kiry=$c; break; }
    done
fi
[ -n "$kiry" ] || { echo "seed: no kiry binary, set KIRY" >&2; exit 1; }
echo "using  $kiry"

mkdir -p "$work/apk" "$work/stage" "$work/pkg" "$root"

echo "fetching"
while read -r name ver rev sha deps; do
    f="$name-$ver-r$rev.apk"
    [ -f "$work/apk/$f" ] || curl -fsS --max-time 600 -o "$work/apk/$f" "$mirror/$f"
    have=$(sha256sum "$work/apk/$f" | awk '{print $1}')
    if [ "$have" != "$sha" ]; then
        echo "seed: $f is not what seed.list pins" >&2
        exit 1
    fi
done < "$list"

echo "unpacking"
while read -r name ver rev sha deps; do
    st="$work/stage/$name"
    rm -rf "$st"
    mkdir -p "$st"
    tar xzf "$work/apk/$name-$ver-r$rev.apk" -C "$st" 2>/dev/null || true
   
    rm -f "$st/.PKGINFO" "$st"/.SIGN.* "$st"/.pre-* "$st"/.post-* "$st/.trigger"
done < "$list"

echo "busybox applets"
loader="$work/stage/musl/lib/ld-musl-x86_64.so.1"
for a in $("$loader" "$work/stage/busybox/bin/busybox" --list); do
    [ -e "$work/stage/busybox/bin/$a" ] || ln -s busybox "$work/stage/busybox/bin/$a"
done

rm -f "$work/stage/busybox/bin/patch"

# the root is usrmerged, so /bin /sbin /lib /lib64 are symlinks and nothing may own a
# path through one. alpine is not merged, so its trees are moved before they are packed
echo "usrmerge"
for st in "$work"/stage/*; do
    for d in bin sbin lib lib64; do
        [ -d "$st/$d" ] || continue
        mkdir -p "$st/usr/$d"
        cp -a "$st/$d/." "$st/usr/$d/"
        rm -rf "$st/$d"
    done
done

echo "toolchain names"
gccdir=$(cd "$work/stage/libgcc-static" && echo usr/lib/gcc/*/*)
llvmbin=$(cd "$work/stage/llvm20" && echo usr/lib/llvm*/bin)
tl="$work/stage/toollinks"
rm -rf "$tl"
mkdir -p "$tl/usr/bin" "$tl/$gccdir" "$tl/usr/lib"

for pair in "cc" "c++ --driver-mode=g++"; do
    set -- $pair
    n=$1; shift
    cat > "$tl/usr/bin/$n" <<EOF
#!/bin/sh
exec clang-20 $* --gcc-install-dir=/$gccdir "\$@"
EOF
    chmod 755 "$tl/usr/bin/$n"
done
ln -s cc "$tl/usr/bin/gcc"
ln -s c++ "$tl/usr/bin/g++"
ln -s ld.lld "$tl/usr/bin/ld"

for t in ar ranlib nm strip objcopy readelf; do
    ln -s "/$llvmbin/llvm-$t" "$tl/usr/bin/llvm-$t"
    ln -s "llvm-$t" "$tl/usr/bin/$t"
done

ln -s crtbeginS.o "$tl/$gccdir/crtbegin.o"
ln -s crtbeginS.o "$tl/$gccdir/crtbeginT.o"
ln -s crtendS.o "$tl/$gccdir/crtend.o"
ln -s libgcc.a "$tl/$gccdir/libgcc_eh.a"
ln -s libgcc_s.so.1 "$tl/usr/lib/libgcc_s.so"

echo "packing"
pack() {
    ar="$work/pkg/$1-$2-$3.$target.tar.zst"
    tar -cf - -C "$work/stage/$1" . | zstd -q -f -o "$ar"
    mkdir -p "$ar.meta"
    echo "$1" > "$ar.meta/name"
    echo "$2 $3" > "$ar.meta/version"
    echo "$target" > "$ar.meta/targets"
    echo "$5" > "$ar.meta/hash"
    if [ "$4" = "-" ]; then
        : > "$ar.meta/depends"
    else
        echo "$4" | sed 's/,/\
/g' > "$ar.meta/depends"
    fi
}
while read -r name ver rev sha deps; do
    pack "$name" "$ver" "$rev" "$deps" "$sha"
done < "$list"
pack toollinks 1 0 clang20,lld20,llvm20,libgcc,libgcc-static 0

echo "installing"
# the four have to exist before anything lands, because PT_INTERP says
# /lib/ld-musl-x86_64.so.1 and every third script says /usr/bin/env
mkdir -p "$root/usr/bin" "$root/usr/sbin" "$root/usr/lib"
for d in bin sbin lib; do
    [ -e "$root/$d" ] || ln -s "usr/$d" "$root/$d"
done
# /usr/lib is musl and /usr/lib64 is gnu, so lib64 is not another name for lib. it
# dangles until the gnu tier lands, which is the point: nothing musl may resolve through it
[ -e "$root/lib64" ] || ln -s usr/lib64 "$root/lib64"

"$kiry" i --root "$root" "$work"/pkg/*.tar.zst

mkdir -p "$root/etc/kiry"
cat > "$root/etc/kiry/toolchain" <<'EOF'
busybox
musl
musl-dev
linux-headers
clang20
clang20-headers
lld20
llvm20
toollinks
libgcc
libgcc-static
libstdc++
libstdc++-dev
make
pkgconf
patch
cmake
samurai
python3
EOF

echo
echo "root  $root"
echo "next  $kiry doctor --root $root"
