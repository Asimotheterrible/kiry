#!/bin/sh
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$HOME/.cache/kiry/root}
[ $# -gt 0 ] && shift
work=${KIRY_QEMU:-$HOME/.cache/kiry/qemu}
mem=${KIRY_QEMU_MEM:-4096}
cpus=${KIRY_QEMU_CPUS:-4}
size=${KIRY_QEMU_SIZE:-8G}
init=${KIRY_QEMU_INIT:-script}
initrd=${KIRY_QEMU_INITRD:-}
# never the passthrough backend: that one hands the guest the real machine's tpm
tpm=${KIRY_QEMU_TPM:-}
swtpm_bin=${KIRY_SWTPM:-$HOME/.cache/kiry/swtpm/prefix/bin/swtpm}

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

if [ "$init" = nitro ]; then
    [ -x "$stage/usr/sbin/nitro" ] || { echo "qemu: no nitro in $root" >&2; exit 1; }
    rm -f "$stage/etc/nitro/ttyS0/down"
    initarg=/usr/sbin/nitro
else
    initarg=/init
fi

if [ -n "$initrd" ]; then
    initopt=
else
    initopt="init=$initarg"
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

if [ -n "$KIRY_QEMU_EFI" ]; then
    esp=$work/esp.img
    disk=$work/disk.img
    espmb=64
    rootmb=$(( $(stat -c %s "$work/root.img") / 1048576 ))

    rm -f "$esp" "$disk"
    mformat -i "$esp" -C -T $(( espmb * 2048 )) -v KIRYESP ::
    mmd -i "$esp" ::/EFI ::/EFI/BOOT
    mcopy -i "$esp" "$kernel" ::/EFI/BOOT/kiry.efi

    # firmware auto-booting the fallback path passes no cmdline, so startup.nsh stands
    # in for the LoadOptions a real boot entry would carry
    shell=$(ls /usr/share/edk2-ovmf/Shell.efi /usr/share/edk2/OvmfX64/Shell.efi 2>/dev/null | head -1)
    if [ -n "$shell" ]; then
        mcopy -i "$esp" "$shell" ::/EFI/BOOT/BOOTX64.EFI
        printf 'FS0:\\EFI\\BOOT\\kiry.efi root=PARTLABEL=kiry-root rw init=%s console=ttyS0\r\n' "$initarg" > "$work/startup.nsh"
        mcopy -i "$esp" "$work/startup.nsh" ::/startup.nsh
    else
        # no shell, so CONFIG_CMDLINE is the only cmdline and the console is the
        # framebuffer rather than this pipe
        mcopy -i "$esp" "$kernel" ::/EFI/BOOT/BOOTX64.EFI
    fi

    # the kernel parses gpt itself, so root=PARTLABEL= needs nothing alive in userspace
    truncate -s $(( (espmb + rootmb + 2) * 1048576 )) "$disk"
    sfdisk -q --label gpt "$disk" >/dev/null <<SFDISK
start=2048, size=$(( espmb * 2048 )), type=uefi, name="kiry-esp"
start=$(( espmb * 2048 + 2048 )), size=$(( rootmb * 2048 )), type=linux, name="kiry-root"
SFDISK

    dd if="$esp" of="$disk" bs=1M seek=1 conv=notrunc status=none
    dd if="$work/root.img" of="$disk" bs=1M seek=$(( espmb + 1 )) conv=notrunc status=none

    acc=tcg
    [ -r /dev/kvm ] && [ -w /dev/kvm ] && acc=kvm
    ovmf=$(ls /usr/share/edk2-ovmf/OVMF_CODE.fd /usr/share/edk2/OvmfX64/OVMF_CODE.fd 2>/dev/null | head -1)
    [ -n "$ovmf" ] || { echo "qemu: no OVMF firmware found" >&2; exit 1; }
    vars=$(ls /usr/share/edk2-ovmf/OVMF_VARS.fd /usr/share/edk2/OvmfX64/OVMF_VARS.fd 2>/dev/null | head -1)
    cp "$vars" "$work/vars.fd"

    echo "booting $acc through uefi"
    exec qemu-system-x86_64 \
    	-m "$mem" -smp "$cpus" -nographic -no-reboot \
    	-accel "$acc" -cpu max \
    	-drive if=pflash,format=raw,unit=0,readonly=on,file="$ovmf" \
    	-drive if=pflash,format=raw,unit=1,file="$work/vars.fd" \
    	-drive file="$disk",format=raw,if=virtio
fi

# -cpu max: the default qemu64 has no avx and a prebuilt gnu binary takes SIGILL on it
acc=tcg
[ -r /dev/kvm ] && [ -w /dev/kvm ] && acc=kvm
tpmargs=""
if [ -n "$tpm" ]; then
    [ -x "$swtpm_bin" ] || { echo "qemu: no swtpm at $swtpm_bin" >&2; exit 1; }
    tpmstate=$work/tpm
    rm -rf "$tpmstate"; mkdir -p "$tpmstate"
    LD_LIBRARY_PATH=$(dirname "$swtpm_bin")/../lib "$swtpm_bin" socket \
        --tpm2 --tpmstate dir="$tpmstate" \
        --ctrl type=unixio,path="$tpmstate/sock" \
        --flags startup-clear --daemon
    trap 'kill %1 2>/dev/null; pkill -f "tpmstate dir=$tpmstate" 2>/dev/null' EXIT
    tpmargs="-chardev socket,id=chrtpm,path=$tpmstate/sock -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-crb,tpmdev=tpm0"
    echo "software tpm at $tpmstate"
fi

echo "booting $acc"
exec qemu-system-x86_64 \
	-m "$mem" -smp "$cpus" -nographic -no-reboot \
	-accel "$acc" -cpu max \
	-kernel "$kernel" \
	-drive file="$work/root.img",format=raw,if=virtio \
	${initrd:+-initrd "$initrd"} \
	$tpmargs \
	-append "root=/dev/vda rw $initopt console=ttyS0 panic=5"
