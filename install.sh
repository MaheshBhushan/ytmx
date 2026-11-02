#!/bin/sh
# Install ytmx on Linux or macOS:
#   curl -fsSL https://raw.githubusercontent.com/MaheshBhushan/ytmx/main/install.sh | sh
# Picks the build for this OS and CPU, checks its SHA-256, installs to
# ${YTMX_INSTALL_DIR} (default ~/.local/bin). YTMX_VERSION=v0.2.0 pins a release.
set -eu

REPO="MaheshBhushan/ytmx"
DIR="${YTMX_INSTALL_DIR:-${HOME}/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { say "ytmx install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
target="${arch}-${os}"

if [ -n "${YTMX_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/${YTMX_VERSION}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi
asset="ytmx-${target}.tar.gz"

command -v curl >/dev/null || die "curl is required"
tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT

say "Downloading ${asset}..."
curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}" || die "no build for ${target} at ${base}"
curl -fsSL "${base}/${asset}.sha256" -o "${tmp}/sum" || die "missing checksum for ${asset}"
if command -v sha256sum >/dev/null; then
  got=$(sha256sum "${tmp}/${asset}" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "${tmp}/${asset}" | cut -d' ' -f1)
fi
[ "${got}" = "$(tr -d ' \n\r' < "${tmp}/sum")" ] || die "checksum mismatch for ${asset}"

tar -xzf "${tmp}/${asset}" -C "${tmp}"
mkdir -p "${DIR}"
install -m 755 "${tmp}/ytmx" "${DIR}/ytmx"
say "Installed ytmx to ${DIR}/ytmx ($("${DIR}/ytmx" --version))"

case ":${PATH}:" in
  *":${DIR}:"*) ;;
  *) say "Add ${DIR} to your PATH, e.g.: echo 'export PATH=\"${DIR}:\$PATH\"' >> ~/.profile" ;;
esac

missing=""
command -v mpv >/dev/null || missing="${missing} mpv"
command -v yt-dlp >/dev/null || missing="${missing} yt-dlp"
if [ -n "${missing}" ]; then
  say ""
  say "ytmx also needs:${missing}"
  if [ "${os}" = apple-darwin ]; then
    say "  brew install${missing}"
  elif command -v apt-get >/dev/null; then
    say "  sudo apt install mpv && pipx install yt-dlp"
  elif command -v pacman >/dev/null; then
    say "  sudo pacman -S${missing}"
  elif command -v dnf >/dev/null; then
    say "  sudo dnf install${missing}"
  else
    say "  mpv: https://mpv.io/installation/   yt-dlp: pipx install yt-dlp"
  fi
fi

say ""
say "Next: ytmx login    (see the README for the omp browser relay)"
