#!/bin/sh
# nice is what keeps a game smooth, not the job count: 5 runnable threads still contend
# for 5 of the 16 hardware ones
set -e
root=${KIRY_ROOT:-$HOME/.cache/kiry/root}
k=${KIRY:-$HOME/kiry/target/release/kiry}
jobs=${KIRY_JOBS:-5}
nicelvl=${KIRY_NICE:-19}

# precedence, first match wins, same order /etc/kiry/repos uses
repos=${KIRY_REPOS:-"$HOME/kiry-repos/local $HOME/kiry-repos/core $HOME/kiry-repos/extra $HOME/kiry-repos/testing"}

for n in "$@"; do
	d=
	for r in $repos; do
		[ -d "$r/$n" ] && { d=$r/$n; break; }
	done
	[ -n "$d" ] || { echo "kb: no recipe $n" >&2; exit 1; }
	KIRY_ROOT="$root" KIRY_JOBS="$jobs" nice -n "$nicelvl" "$k" b "$d"

	# by sidecar name, not filename: a glob would match wayland-protocols for wayland
	a=$(ls -t "$root"/var/kiry/cache/*.tar.zst 2>/dev/null | while read -r f; do
		[ "$(cat "$f.meta/name" 2>/dev/null)" = "$n" ] && { echo "$f"; break; }
	done)
	[ -n "$a" ] || { echo "kb: $n built but no archive" >&2; exit 1; }
	KIRY_ROOT="$root" "$k" i --force "$a"
done
