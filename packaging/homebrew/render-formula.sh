#!/bin/sh
# Render the Homebrew formula for complot from complot.rb.in.
#
# Usage: render-formula.sh VERSION SHA256SUMS.txt > complot.rb
#
# VERSION is the release version without the leading "v", e.g. 0.2.0.
# SHA256SUMS.txt is in sha256sum format ("<hex>  <file>"), as written by the
# release workflow. The formula is printed on stdout.
set -eu

die() {
  printf 'render-formula.sh: error: %s\n' "$*" >&2
  exit 1
}

if [ "$#" -ne 2 ]; then
  die "usage: $0 VERSION SHA256SUMS.txt"
fi
version=$1
sums=$2

case "$version" in
  '' | *[!0-9A-Za-z.+-]*) die "invalid version '$version'" ;;
esac
[ -r "$sums" ] || die "cannot read '$sums'"

script_dir=$(cd "$(dirname "$0")" && pwd)
template="$script_dir/complot.rb.in"
[ -r "$template" ] || die "template not found: $template"

# sum_for FILE: print the SHA-256 recorded in $sums for the exact name FILE.
# Runs in a command substitution, so a failure here aborts the caller through
# set -e after the message has been printed.
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

sha_mac_arm=$(sum_for "complot-v$version-aarch64-apple-darwin.tar.gz")
sha_mac_intel=$(sum_for "complot-v$version-x86_64-apple-darwin.tar.gz")
sha_linux_arm=$(sum_for "complot-v$version-aarch64-unknown-linux-gnu.tar.gz")
sha_linux_intel=$(sum_for "complot-v$version-x86_64-unknown-linux-gnu.tar.gz")

# '|' cannot occur in a validated version or in a hex digest, so it is safe
# as the sed delimiter.
rendered=$(sed \
  -e "s|@VERSION@|$version|g" \
  -e "s|@SHA_AARCH64_APPLE_DARWIN@|$sha_mac_arm|g" \
  -e "s|@SHA_X86_64_APPLE_DARWIN@|$sha_mac_intel|g" \
  -e "s|@SHA_AARCH64_LINUX@|$sha_linux_arm|g" \
  -e "s|@SHA_X86_64_LINUX@|$sha_linux_intel|g" \
  "$template")

if printf '%s\n' "$rendered" | grep -q '@[A-Z0-9_][A-Z0-9_]*@'; then
  die "a placeholder was left unreplaced in the rendered formula"
fi
printf '%s\n' "$rendered"
