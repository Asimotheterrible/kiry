#!/bin/sh
set -e

root=${1:?usage: usb.sh <root> [out.img]}
out=${2:-kiry.img}
espmb=${KIRY_USB_ESPMB:-128}
slackmb=${KIRY_USB_SLACKMB:-512}
# rootwait, because usb mass storage has not probed by the time the kernel looks for a
# root device and it panics at two and a half seconds without it. no quiet either: on a
# machine nobody has booted this on, printk is what separates a hang from a dark display
cmdline=${KIRY_USB_CMDLINE:-"root=PARTLABEL=kiry-root rw rootwait init=/usr/sbin/nitro"}

[ -d "$root" ] || { echo "usb: no root at $root" >&2; exit 1; }
kernel=$(ls "$root"/boot/vmlinuz-* 2>/dev/null | head -1)
stub=$root/usr/lib/stubbyboot/linuxx64.efi.stub
[ -f "$kernel" ] || { echo "usb: no kernel in $root/boot" >&2; exit 1; }
[ -f "$stub" ] || { echo "usb: no stub at $stub" >&2; exit 1; }

# beside the root, not /tmp: the tree below is hardlinked and a link cannot cross a fs
work=$(mktemp -d "$(cd "$(dirname "$root")" && pwd)/.usb.XXXXXX")
trap 'rm -rf "$work"' EXIT

# each section clears everything already in the stub and starts on the 2M boundary the
# pe loader wants
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

if [ -z "$KIRY_USB_SIGN" ]; then
	cp "$work/uki.efi" "$work/boot.efi"
	echo "usb: unsigned -- turn secure boot off to boot it"
else
	key=${KIRY_USB_KEY:-/var/lib/sbctl/keys/db/db.key}
	cert=${KIRY_USB_CERT:-/var/lib/sbctl/keys/db/db.pem}
	# sbctl keeps both halves root-only. the certificate is public, so it comes out to
	# a copy sbverify can read rather than a second trip through doas
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

# stage and log are build leftovers. the cache is what lets an install run with no
# network, so it is the one thing under /var/kiry that comes along
tree=$work/root
mkdir -p "$tree/var/kiry"
for e in "$root"/*; do
	[ "$(basename "$e")" = var ] || cp -al "$e" "$tree/"
done
for e in "$root"/var/*; do
	[ "$(basename "$e")" = kiry ] || cp -al "$e" "$tree/var/"
done
[ -n "$KIRY_USB_LEAN" ] || cp -al "$root/var/kiry/cache" "$tree/var/kiry/"

# hardlinked, so break the link before editing
cp "$root/etc/shadow" "$work/shadow"
sed -i 's|^root:[^:]*:|root::|' "$work/shadow"
rm -f "$tree/etc/shadow"
cp -p "$work/shadow" "$tree/etc/shadow"

[ -z "$KIRY_USB_SERIAL" ] || rm -f "$tree/etc/nitro/ttyS0/down"

repos=${KIRY_USB_REPOS:-$HOME/kiry-repos}
if [ -d "$repos" ]; then
	cp -a "$repos" "$tree/kiry-repos"
	echo "usb: carrying $repos"
fi

# mke2fs -d copies the uid it finds, so without this every system file in the image is
# owned by the building user. openntpd says so out loud, everything else ships wrong.
# the tree is hardlinked, so this reaches the staged root and the trap puts it back
own=$(id -u):$(id -g)
restore() { doas chown -R "$own" "$root"; doas rm -rf "$work"; }
trap restore EXIT
doas chown -R 0:0 "$tree"

# doas: the chown above made parts unreadable, and a du that cannot enter /root
# undercounts until mke2fs runs out of blocks
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
