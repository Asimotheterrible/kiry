#!/bin/sh
# boot a kiry root under qemu, because a package that installs is not a system that
# starts. the kernel comes from the root's own /boot, so this tests what was built
#
# ./bootstrap/qemu.sh [root] [command...]
# with no command it drops to a shell, otherwise it runs one and powers off
set -e
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$HOME/.cache/kiry/root}
[ $# -gt 0 ] && shift
work=${KIRY_QEMU:-$HOME/.cache/kiry/qemu}
mem=${KIRY_QEMU_MEM:-4096}
cpus=${KIRY_QEMU_CPUS:-4}
size=${KIRY_QEMU_SIZE:-8G}
# KIRY_QEMU_INIT=nitro boots what the root actually installed, rather than the throwaway
# script below. the serial console service ships `down`, so it gets turned on here
init=${KIRY_QEMU_INIT:-script}
# KIRY_QEMU_INITRD=<file> boots through an initramfs, which is the only way to reach a
# luks root: cryptsetup is userspace and the kernel cannot open one on its own
initrd=${KIRY_QEMU_INITRD:-}
# KIRY_QEMU_TPM=1 attaches a software tpm with its own state directory. never
# passthrough: that backend hands the guest /dev/tpm0, which is the real machine's tpm,
# and sealing against it would persist into hardware
tpm=${KIRY_QEMU_TPM:-}
# KIRY_QEMU_SECUREBOOT=1 boots the firmware that actually enforces db. it starts in
# setup mode with no keys, which is where a real machine starts too
secureboot=${KIRY_QEMU_SECUREBOOT:-}
# KIRY_QEMU_REUSE=1 boots the disk and the nvram a previous run left behind. enrolling
# keys and then proving they are enforced takes two boots, and the second one has to
# see what the first one wrote
reuse=${KIRY_QEMU_REUSE:-}
swtpm_bin=${KIRY_SWTPM:-$HOME/.cache/kiry/swtpm/prefix/bin/swtpm}

kernel=$(ls -1 "$root"/boot/vmlinuz-* 2>/dev/null | tail -1)
[ -n "$kernel" ] || { echo "qemu: no kernel in $root/boot, build core/linux first" >&2; exit 1; }
echo "kernel $kernel"

# hardlinked, so staging a 3GB root costs nothing and the caches can be dropped from
# the copy without touching the real one
stage=$work/stage
if [ -n "$reuse" ]; then
    [ -r "$work/disk.img" ] || { echo "qemu: nothing to reuse in $work" >&2; exit 1; }
    echo "reusing $work/disk.img"
else
rm -rf "$stage"
mkdir -p "$work"
cp -al "$root" "$stage"
rm -rf "$stage/var/kiry/cache" "$stage/var/kiry/log"

# a root with core/kiry installed already has one and it is the musl build. only a root
# that does not gets the host binary, the way seed.sh finds one -- and rm first, because
# the stage is hardlinked and a cp would write through to the real root
kiry=${KIRY:-}
if [ -z "$kiry" ] && [ ! -x "$stage/usr/bin/kiry" ]; then
    for c in "$here/../target/release/kiry" "$here/../target/debug/kiry"; do
        [ -x "$c" ] && { kiry=$c; break; }
    done
fi
if [ -n "$kiry" ]; then
    rm -f "$stage/usr/bin/kiry"
    cp "$kiry" "$stage/usr/bin/kiry"
fi

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

# with an initramfs the kernel runs its /init and that decides what comes next, so
# naming one here would only confuse the two
if [ -n "$initrd" ]; then
    initopt=
else
    initopt="init=$initarg"
fi

cat > "$stage/init" <<'EOF'
#!/bin/sh
# devtmpfs is already mounted, the kernel does it before handing over
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
# mke2fs -d copies the uid it finds and the stage is hardlinked to a root staged by
# whoever ran kiry, so without this every file in the image belongs to that user and a
# boot test disagrees with the image usb.sh builds. openntpd is the only thing that
# says so out loud; everything else is quietly wrong
own=$(id -u):$(id -g)
restore() { doas chown -R "$own" "$root" "$stage" 2>/dev/null; }
trap restore EXIT INT TERM
doas chown -R 0:0 "$stage"
doas mke2fs -q -t ext4 -d "$stage" -F -m 0 -L kiry "$work/root.img" "$size"
doas chown "$own" "$work/root.img"
restore
trap - EXIT INT TERM
fi

tpmargs=""
if [ -n "$tpm" ]; then
    [ -x "$swtpm_bin" ] || { echo "qemu: no swtpm at $swtpm_bin" >&2; exit 1; }
    tpmstate=$work/tpm
    [ -n "$reuse" ] || rm -rf "$tpmstate"
    mkdir -p "$tpmstate"
    LD_LIBRARY_PATH=$(dirname "$swtpm_bin")/../lib "$swtpm_bin" socket \
        --tpm2 --tpmstate dir="$tpmstate" \
        --ctrl type=unixio,path="$tpmstate/sock" \
        --flags startup-clear --daemon
    trap 'kill %1 2>/dev/null; pkill -f "tpmstate dir=$tpmstate" 2>/dev/null' EXIT
    tpmargs="-chardev socket,id=chrtpm,path=$tpmstate/sock -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-crb,tpmdev=tpm0"
    echo "software tpm at $tpmstate"
fi

# KIRY_QEMU_EFI=1 wraps that filesystem in a gpt disk with an esp and boots it the way
# a real machine does: firmware, EFI/BOOT/BOOTX64.EFI, the kernel's own efi stub. no -kernel
# shortcut, so the boot path under test is the one the install guide describes
if [ -n "$KIRY_QEMU_EFI" ]; then
    esp=$work/esp.img
    disk=$work/disk.img
    espmb=${KIRY_QEMU_ESPMB:-128}
    if [ -n "$reuse" ]; then
        rootmb=0
    else
    rootmb=$(( $(stat -c %s "$work/root.img") / 1048576 ))

    rm -f "$esp" "$disk"
    mformat -i "$esp" -C -T $(( espmb * 2048 )) -v KIRYESP ::
    mmd -i "$esp" ::/EFI ::/EFI/BOOT
    mcopy -i "$esp" "$kernel" ::/EFI/BOOT/kiry.efi

    # a real install points a uefi boot entry at the kernel and puts the cmdline in that
    # entry's LoadOptions. firmware auto-booting the fallback path passes none, so the
    # shell stands in for efibootmgr here and startup.nsh is the LoadOptions. it means
    # the stub is tested taking a cmdline, which is what the machine will actually do
    shell=$(ls /usr/share/edk2-ovmf/Shell.efi /usr/share/edk2/OvmfX64/Shell.efi 2>/dev/null | head -1)
    if [ -n "$shell" ]; then
        mcopy -i "$esp" "$shell" ::/EFI/BOOT/BOOTX64.EFI
        printf 'FS0:\\EFI\\BOOT\\kiry.efi root=PARTLABEL=kiry-root rw init=%s console=ttyS0\r\n' "$initarg" > "$work/startup.nsh"
        mcopy -i "$esp" "$work/startup.nsh" ::/startup.nsh
    else
        # no shell, so the kernel is the fallback binary and CONFIG_CMDLINE is the only
        # cmdline there is. it boots, the console is the framebuffer and not this pipe
        mcopy -i "$esp" "$kernel" ::/EFI/BOOT/BOOTX64.EFI
    fi

    # the partition name is what CONFIG_CMDLINE's root=PARTLABEL= resolves, and the
    # kernel reads gpt itself, so nothing in userspace has to be alive to find the root
    truncate -s $(( (espmb + rootmb + 2) * 1048576 )) "$disk"
    sfdisk -q --label gpt "$disk" >/dev/null <<SFDISK
start=2048, size=$(( espmb * 2048 )), type=uefi, name="kiry-esp"
start=$(( espmb * 2048 + 2048 )), size=$(( rootmb * 2048 )), type=linux, name="kiry-root"
SFDISK

    dd if="$esp" of="$disk" bs=1M seek=1 conv=notrunc status=none
    dd if="$work/root.img" of="$disk" bs=1M seek=$(( espmb + 1 )) conv=notrunc status=none
    fi

    acc=tcg
    [ -r /dev/kvm ] && [ -w /dev/kvm ] && acc=kvm
    code=OVMF_CODE.fd
    varsname=OVMF_VARS.fd
    # the secboot firmware is built SMM_REQUIRE, so it wants a q35 with smm and a
    # pflash the world outside smm cannot write. on a plain pc machine it hangs before
    # it prints anything at all
    machine=
    if [ -n "$secureboot" ]; then
        code=OVMF_CODE.secboot.fd
        # not OVMF_VARS.secboot.fd -- that one ships with microsoft's keys already
        # enrolled, so the firmware is enforcing from the first boot and there is no
        # setup mode to enroll into. the empty varstore is where a cleared machine is
        varsname=OVMF_VARS.fd
        machine="-machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on -global ICH9-LPC.disable_s3=1"
    fi
    ovmf=$(ls /usr/share/edk2-ovmf/$code /usr/share/edk2/OvmfX64/$code 2>/dev/null | head -1)
    [ -n "$ovmf" ] || { echo "qemu: no $code found" >&2; exit 1; }
    if [ -z "$reuse" ]; then
        vars=$(ls /usr/share/edk2-ovmf/$varsname /usr/share/edk2/OvmfX64/$varsname 2>/dev/null | head -1)
        cp "$vars" "$work/vars.fd"
    fi

    echo "booting $acc through uefi"
    exec qemu-system-x86_64 \
    	-m "$mem" -smp "$cpus" -nographic -no-reboot \
    	-accel "$acc" -cpu max $machine \
    	-drive if=pflash,format=raw,unit=0,readonly=on,file="$ovmf" \
    	-drive if=pflash,format=raw,unit=1,file="$work/vars.fd" \
    	$tpmargs \
    	-drive file="$disk",format=raw,if=virtio ${KIRY_QEMU_DISK:+-drive file=$KIRY_QEMU_DISK,format=raw,if=virtio}
fi

# KIRY_QEMU_NET=1 gives the guest qemu's user-mode network, which is enough to prove a
# tls trust store and a dns lookup without touching a real interface
net=${KIRY_QEMU_NET:+-netdev user,id=n0 -device virtio-net-pci,netdev=n0}

# KIRY_QEMU_DISK=path attaches a second raw disk as vdb. an install writes to a disk
# that is not the one it booted from, so rehearsing one needs somewhere to write
disk2=${KIRY_QEMU_DISK:+-drive file=$KIRY_QEMU_DISK,format=raw,if=virtio}

# -cpu max because the default qemu64 has no avx and a prebuilt gnu binary compiled for
# a real machine takes SIGILL on it. kvm when the machine allows it
acc=tcg
[ -r /dev/kvm ] && [ -w /dev/kvm ] && acc=kvm
echo "booting $acc"
exec qemu-system-x86_64 \
	-m "$mem" -smp "$cpus" -nographic -no-reboot \
	-accel "$acc" -cpu max \
	-kernel "$kernel" \
	-drive file="$work/root.img",format=raw,if=virtio \
	$net $disk2 \
	${initrd:+-initrd "$initrd"} \
	$tpmargs \
	-append "root=/dev/vda rw $initopt console=ttyS0 panic=5"
