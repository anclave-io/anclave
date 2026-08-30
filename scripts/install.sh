#!/bin/sh
# Install anclave from a GitHub release.
#
# POSIX sh on purpose: this is piped into whatever shell the user has, and
# bashisms fail on dash, ash, and macOS's older sh in ways that are hard to
# diagnose from the other end of a pipe.
#
#   curl -fsSL https://raw.githubusercontent.com/anclave-io/anclave/main/scripts/install.sh | sh
#
# Environment:
#   VERSION      tag to install (default: the latest release)
#   INSTALL_DIR  where the binaries go (default: ~/.local/bin)

set -eu

REPO="${REPO:-anclave-io/anclave}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Color only when stderr is a terminal that wants it. A pipe-to-shell install
# often lands in a log, where escape codes are noise.
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
  BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
  RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m'); RESET=$(printf '\033[0m')
else
  BOLD=''; DIM=''; RED=''; GREEN=''; RESET=''
fi

say()  { printf '%s\n' "$*" >&2; }
info() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*" >&2; }
warn() { printf '%swarning:%s %s\n' "$RED" "$RESET" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

# --- platform -------------------------------------------------------------

detect_target() {
  dt_os=$(uname -s)
  dt_arch=$(uname -m)
  case "$dt_os" in
    Linux)
      case "$dt_arch" in
        x86_64)          echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64)   echo "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported Linux architecture: $dt_arch" ;;
      esac
      ;;
    Darwin)
      case "$dt_arch" in
        arm64)   echo "aarch64-apple-darwin" ;;
        x86_64)  echo "x86_64-apple-darwin" ;;
        *) die "unsupported macOS architecture: $dt_arch" ;;
      esac
      ;;
    *)
      die "unsupported operating system: $dt_os

anclave's daemon needs a Unix socket and a POSIX process model.
Windows support is not built yet."
      ;;
  esac
}

# --- download -------------------------------------------------------------

fetch() {
  # curl and wget disagree about everything except that one of them is present.
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "either curl or wget is required"
  fi
}

fetch_stdout() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1"
  else
    wget -qO- "$1"
  fi
}

latest_version() {
  # The API answer is JSON; sed rather than jq so the installer keeps its
  # promise of needing nothing unusual.
  fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n 1
}

# POSIX sh has no function-local variables: a bare assignment here writes the
# *caller's* variable. This function used `archive`, `name` and `sums`, which
# silently overwrote the caller's `$archive` with a full path and made the
# later extraction look for `$tmp/$tmp/<archive>`. Every name below is
# prefixed so it cannot collide with a caller's.
verify_checksum() {
  vc_file="$1"; vc_sums="$2"; vc_name="$3"
  vc_expected=$(grep " $vc_name\$" "$vc_sums" | awk '{print $1}' | head -n 1)
  [ -n "$vc_expected" ] || die "no checksum published for $vc_name"

  if command -v sha256sum >/dev/null 2>&1; then
    vc_actual=$(sha256sum "$vc_file" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    vc_actual=$(shasum -a 256 "$vc_file" | awk '{print $1}')
  else
    warn "no sha256 tool found: skipping checksum verification"
    return 0
  fi

  [ "$vc_actual" = "$vc_expected" ] || die "checksum mismatch for $vc_name
  expected $vc_expected
  actual   $vc_actual"
  info "checksum verified"
}

# --- install --------------------------------------------------------------

main() {
  need uname
  need tar

  target=$(detect_target)
  version="${VERSION:-$(latest_version)}"
  [ -n "$version" ] || die "could not determine the latest version; set VERSION explicitly"

  archive="anclave-$version-$target.tar.gz"
  base="https://github.com/$REPO/releases/download/$version"

  info "anclave $version ($target)"

  tmp=$(mktemp -d)
  # Clean up on every exit path, including the failures above this point.
  trap 'rm -rf "$tmp"' EXIT INT TERM

  info "downloading $archive"
  fetch "$base/$archive" "$tmp/$archive" \
    || die "download failed: is $version a published release for $target?"

  if fetch "$base/anclave-$version-checksums.txt" "$tmp/sums.txt" 2>/dev/null; then
    verify_checksum "$tmp/$archive" "$tmp/sums.txt" "$archive"
  else
    warn "no checksum file published for $version: skipping verification"
  fi

  tar -xzf "$tmp/$archive" -C "$tmp"

  mkdir -p "$INSTALL_DIR"
  for binary in anclaved anclave anclave-cli; do
    [ -f "$tmp/$binary" ] || die "$binary missing from the archive"
    install -m 755 "$tmp/$binary" "$INSTALL_DIR/$binary" 2>/dev/null \
      || { cp "$tmp/$binary" "$INSTALL_DIR/$binary"; chmod 755 "$INSTALL_DIR/$binary"; }
  done

  say ""
  info "${GREEN}installed${RESET} anclaved, anclave, anclave-cli to $INSTALL_DIR"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      say ""
      warn "$INSTALL_DIR is not on your PATH. Add it:"
      say "  ${DIM}export PATH=\"\$PATH:$INSTALL_DIR\"${RESET}"
      ;;
  esac

  say ""
  say "Start the daemon, then talk to it:"
  say "  ${DIM}anclaved --socket /tmp/anclaved.sock &${RESET}"
  say "  ${DIM}anclave-cli daemon status${RESET}"
  say "  ${DIM}anclave-cli daemon sandbox${RESET}   # what containment this host can provide"
}

main "$@"
