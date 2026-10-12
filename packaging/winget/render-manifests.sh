#!/bin/sh
# Render the winget manifests for complot from the templates in this directory.
#
# Usage: render-manifests.sh VERSION SHA256SUMS.txt OUTDIR
#
# VERSION is the release version without the leading "v", e.g. 0.2.0.
# SHA256SUMS.txt is in sha256sum format, as written by the release workflow.
# The three manifests are written into OUTDIR, which is created if needed.
# The installer SHA-256 is written in upper-case hex, as winget expects.
set -eu

die() {
  printf 'render-manifests.sh: error: %s\n' "$*" >&2
  exit 1
}

if [ "$#" -ne 3 ]; then
  die "usage: $0 VERSION SHA256SUMS.txt OUTDIR"
fi
version=$1
sums=$2
outdir=$3

case "$version" in
  '' | *[!0-9A-Za-z.+-]*) die "invalid version '$version'" ;;
esac
[ -r "$sums" ] || die "cannot read '$sums'"

script_dir=$(cd "$(dirname "$0")" && pwd)

mkdir -p "$outdir"
outdir_abs=$(cd "$outdir" && pwd)
if [ "$outdir_abs" = "$script_dir" ]; then
  die "OUTDIR must not be the template directory itself"
fi

# sum_for FILE: print the SHA-256 recorded in $sums for the exact name FILE.
# Called through a plain command substitution (never in a pipeline), so a
# failure here aborts the script through set -e.
sum_for() {
  matches=$(awk -v f="$1" '{ name = $2; sub(/^\*/, "", name); if (name == f) print $1 }' "$sums")
  count=$(printf '%s\n' "$matches" | awk 'NF { n++ } END { print n + 0 }')
  if [ "$count" -ne 1 ]; then
    die "expected exactly one entry for '$1' in '$sums', found $count"
  fi
  if ! printf '%s\n' "$matches" | grep -Eq '^[0-9A-Fa-f]{64}$'; then
    die "malformed SHA-256 for '$1' in '$sums'"
  fi
  printf '%s\n' "$matches"
}

sha_zip=$(sum_for "complot-v$version-x86_64-pc-windows-msvc.zip")
sha_zip=$(printf '%s\n' "$sha_zip" | tr 'a-f' 'A-F')

# '|' cannot occur in a validated version or in a hex digest, so it is safe
# as the sed delimiter.
for name in \
  PaoloBosetti.complot.yaml \
  PaoloBosetti.complot.locale.en-US.yaml \
  PaoloBosetti.complot.installer.yaml
do
  src="$script_dir/$name"
  [ -r "$src" ] || die "template not found: $src"
  rendered=$(sed \
    -e "s|@VERSION@|$version|g" \
    -e "s|@SHA256_ZIP@|$sha_zip|g" \
    "$src")
  if printf '%s\n' "$rendered" | grep -q '@[A-Z0-9_][A-Z0-9_]*@'; then
    die "a placeholder was left unreplaced in $name"
  fi
  printf '%s\n' "$rendered" > "$outdir/$name"
  printf 'wrote %s\n' "$outdir/$name"
done
