#!/bin/sh
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$HOME/.cache/kiry/root}
[ $# -gt 0 ] && shift
work=${KIRY_QEMU:-$HOME/.cache/kiry/qemu}
mem=${KIRY_QEMU_MEM:-4096}
cpus=${KIRY_QEMU_CPUS:-4}
size=${KIRY_QEMU_SIZE:-8G}

kernel=$(ls -1 "$root"/boot/vmlinuz-* 2>/dev/null | tail -1)
[ -n "$kernel" ] || { echo "qemu: no kernel in $root/boot, build core/linux first" >&2; exit 1; }
echo "kernel $kernel"

stage=$work/stage
rm -rf "$stage"
mkdir -p "$work"
cp -al "$root" "$stage"
rm -rf "$stage/var/kiry/cache" "$stage/var/kiry/log"

# no recipe builds kiry yet, so the booted system gets the binary the same way seed.sh
# finds one. a root that cannot run `kiry doctor` on itself is not proving much
kiry=${KIRY:-}
if [ -z "$kiry" ]; then
    for c in "$here/../target/release/kiry" "$here/../target/debug/kiry"; do
        [ -x "$c" ] && { kiry=$c; break; }
    done
fi
[ -n "$kiry" ] && cp "$kiry" "$stage/usr/bin/kiry"

if [ $# -gt 0 ]; then
    printf '%s\n' "$*" > "$stage/.run"
fi

cat > "$stage/init" <<'EOF'
#!/bin/sh
busybox mount -t proc proc /proc
busybox mount -t sysfs sys /sys
export PATH=/usr/local/bin:/usr/bin:/usr/local/sbin:/usr/sbin
export HOME=/root TERM=linux
if [ -f /.run ]; then
	sh -c "$(cat /.run)" || echo "qemu: exited $?"
	busybox poweroff -f
else
	echo
	echo "kiry. exit or poweroff -f to leave"
	setsid busybox sh -c 'exec sh </dev/ttyS0 >/dev/ttyS0 2>&1'
	busybox poweroff -f
fi
EOF
chmod 755 "$stage/init"

echo "packing $size image"
rm -f "$work/root.img"
mke2fs -q -t ext4 -d "$stage" -F -m 0 -L kiry "$work/root.img" "$size"

# -cpu max: the default qemu64 has no avx and a prebuilt gnu binary takes SIGILL on it
acc=tcg
[ -r /dev/kvm ] && [ -w /dev/kvm ] && acc=kvm
echo "booting $acc"
exec qemu-system-x86_64 \
	-m "$mem" -smp "$cpus" -nographic -no-reboot \
	-accel "$acc" -cpu max \
	-kernel "$kernel" \
	-drive file="$work/root.img",format=raw,if=virtio \
	-append "root=/dev/vda rw init=/init console=ttyS0 panic=5"
