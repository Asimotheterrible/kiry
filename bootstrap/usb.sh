#!/bin/sh
# build a bootable gpt image from a staged root and, if asked, write it to a stick
#
#   bootstrap/usb.sh <root> [out.img]
#
# the image is one esp holding a signed uki and one ext4 partition holding the root.
# no initramfs: the media is unencrypted, so the kernel resolves root=PARTLABEL= off
# the gpt it parses itself. an encrypted install is what needs one
#
#   KIRY_USB_SIGN=1                sign the uki, sbctl's db pair unless told otherwise
#   KIRY_USB_KEY / KIRY_USB_CERT   the pair to sign with
#   KIRY_USB_CMDLINE               override the builtin cmdline
#   KIRY_USB_SERIAL=1              also run a getty on ttyS0
#   KIRY_USB_REPOS                 recipe tree to carry, default ~/kiry-repos
#   KIRY_USB_ESPMB                 esp size, default 128
#   KIRY_USB_SLACKMB               free space left in the root, default 512
set -e

root=${1:?usage: usb.sh <root> [out.img]}
out=${2:-kiry.img}
espmb=${KIRY_USB_ESPMB:-128}
slackmb=${KIRY_USB_SLACKMB:-512}
# rootwait, because the root is on a stick. sata and nvme have probed by the time the
# kernel looks for a root device and usb mass storage has not, so without it the kernel
# panics at two and a half seconds with a partition list the stick is not in yet
#
# no quiet and no loglevel, deliberately. this is install media booting on a machine
# nobody has run it on before, and the printk stream is the only thing that separates
# a kernel that hung from a kernel that came up behind a display that never lit. the
# getty sharing tty1 with printk is real, and SYS/setup turns the console down once
# there is a userland to say so -- after the part worth reading
cmdline=${KIRY_USB_CMDLINE:-"root=PARTLABEL=kiry-root rw rootwait init=/usr/sbin/nitro"}

[ -d "$root" ] || { echo "usb: no root at $root" >&2; exit 1; }
kernel=$(ls "$root"/boot/vmlinuz-* 2>/dev/null | head -1)
stub=$root/usr/lib/stubbyboot/linuxx64.efi.stub
[ -f "$kernel" ] || { echo "usb: no kernel in $root/boot" >&2; exit 1; }
[ -f "$stub" ] || { echo "usb: no stub at $stub" >&2; exit 1; }

# beside the root rather than in /tmp: the tree below is hardlinked, and a hard link
# cannot cross a filesystem
work=$(mktemp -d "$(cd "$(dirname "$root")" && pwd)/.usb.XXXXXX")
trap 'rm -rf "$work"' EXIT

# the vma of each section has to clear everything already in the stub, and each one
# starts on a 2M boundary because that is the alignment the pe loader wants
end=0
for pair in $(objdump -h "$stub" | awk '$2 ~ /^\./ { print $4"+"$3 }'); do
	e=$(( 0x${pair%%+*} + 0x${pair##*+} ))
	[ "$e" -gt "$end" ] && end=$e || true
done
align() { echo $(( ($1 + 0x1fffff) / 0x200000 * 0x200000 )); }

printf '%s' "$cmdline" > "$work/cmdline"
vma_cmdline=$(align $((end + 0x1000)))
vma_linux=$(align $((vma_cmdline + $(wc -c < "$work/cmdline") + 0x1000)))

objcopy \
	--add-section .cmdline="$work/cmdline" --change-section-vma .cmdline=$vma_cmdline \
	--add-section .linux="$kernel"         --change-section-vma .linux=$vma_linux \
	"$stub" "$work/uki.efi"

# unsigned by default: this is install media, and turning secure boot off for one boot
# is less work than enrolling a key for a stick that gets overwritten next month. the
# installed system is where signing matters
if [ -z "$KIRY_USB_SIGN" ]; then
	cp "$work/uki.efi" "$work/boot.efi"
	echo "usb: unsigned -- turn secure boot off to boot it"
else
	key=${KIRY_USB_KEY:-/var/lib/sbctl/keys/db/db.key}
	cert=${KIRY_USB_CERT:-/var/lib/sbctl/keys/db/db.pem}
	# sbctl keeps both halves root-only, and signing is the one step that needs the
	# private one. the certificate is public, so it comes out to a copy the verify can
	# read rather than being reached through doas a second time
	if [ -r "$key" ]; then
		sbsign --key "$key" --cert "$cert" --output "$work/boot.efi" "$work/uki.efi"
		cp "$cert" "$work/cert.pem"
	else
		doas sbsign --key "$key" --cert "$cert" --output "$work/boot.efi" "$work/uki.efi"
		doas chown "$(id -u):$(id -g)" "$work/boot.efi"
		doas cat "$cert" > "$work/cert.pem"
	fi
	sbverify --cert "$work/cert.pem" "$work/boot.efi" >/dev/null
	echo "usb: signed with $cert"
fi

mformat -i "$work/esp.img" -C -T $(( espmb * 2048 )) -v KIRYESP ::
mmd -i "$work/esp.img" ::/EFI ::/EFI/BOOT
mcopy -i "$work/esp.img" "$work/boot.efi" ::/EFI/BOOT/BOOTX64.EFI

# a hardlinked copy, so the staged root is not the thing being changed and nothing is
# duplicated on disk. /var/kiry/stage is build trees and /var/kiry/log is build logs --
# neither belongs on install media. the package cache does: it is what lets an install
# run with no network
tree=$work/root
mkdir -p "$tree/var/kiry"
for e in "$root"/*; do
	[ "$(basename "$e")" = var ] || cp -al "$e" "$tree/"
done
for e in "$root"/var/*; do
	[ "$(basename "$e")" = kiry ] || cp -al "$e" "$tree/var/"
done
[ -n "$KIRY_USB_LEAN" ] || cp -al "$root/var/kiry/cache" "$tree/var/kiry/"

# hardlinked, so this one file has to be broken out of the link before it is edited
cp "$root/etc/shadow" "$work/shadow"
sed -i 's|^root:[^:]*:|root::|' "$work/shadow"
rm -f "$tree/etc/shadow"
cp -p "$work/shadow" "$tree/etc/shadow"

[ -z "$KIRY_USB_SERIAL" ] || rm -f "$tree/etc/nitro/ttyS0/down"

# the recipes, because a medium that can build has to have something to build
repos=${KIRY_USB_REPOS:-$HOME/kiry-repos}
if [ -d "$repos" ]; then
	cp -a "$repos" "$tree/kiry-repos"
	echo "usb: carrying $repos"
fi

# mke2fs -d copies the uid it finds, and kiry stages a root as whoever ran it -- so
# without this every system file in the image is owned by the building user. openntpd
# is the one that says so out loud (`st_uid != 0` on its privsep dir); everything else
# just quietly ships wrong. the tree is hardlinked, so this chowns the staged root too,
# and the trap puts it back even if mke2fs dies
own=$(id -u):$(id -g)
# the work tree is root-owned by the time this runs, so the cleanup needs doas too
restore() { doas chown -R "$own" "$root"; doas rm -rf "$work"; }
trap restore EXIT
doas chown -R 0:0 "$tree"

# doas, because the chown just above made parts of the tree unreadable to the user
# running this. a du that cannot enter /root undercounts, and mke2fs then either runs
# out of blocks or fits the tree with nothing to spare
rootmb=$(( $(doas du -sm "$tree" | cut -f1) + slackmb ))
doas mke2fs -q -t ext4 -d "$tree" -F -m 0 -L kiry "$work/root.img" "${rootmb}m"
doas chown "$own" "$work/root.img"

truncate -s $(( (espmb + rootmb + 2) * 1048576 )) "$out"
sfdisk -q --label gpt "$out" >/dev/null <<SFDISK
start=2048, size=$(( espmb * 2048 )), type=uefi, name="kiry-esp"
start=$(( espmb * 2048 + 2048 )), size=$(( rootmb * 2048 )), type=linux, name="kiry-root"
SFDISK
dd if="$work/esp.img" of="$out" bs=1M seek=1 conv=notrunc status=none
dd if="$work/root.img" of="$out" bs=1M seek=$(( espmb + 1 )) conv=notrunc status=none

echo "usb: $out  esp ${espmb}M  root ${rootmb}M"
echo "usb: root has no password -- set one after the first boot"
echo "usb: write it with  doas dd if=$out of=/dev/sdX bs=4M oflag=direct status=progress"
