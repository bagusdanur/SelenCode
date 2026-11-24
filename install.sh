#!/bin/sh
# Installer satu baris untuk SelenCode (Termux Android & Linux x86_64 / VPS / WSL)
# Pakai:
#   curl -fsSL https://raw.githubusercontent.com/bagusdanur/SelenCode/main/install.sh | sh

set -eu

REPO="${SELENCODE_REPO:-bagusdanur/SelenCode}"

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

# 1. Deteksi OS, Termux, dan arsitektur
OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"
IS_TERMUX=0

if [ -n "${TERMUX_VERSION:-}" ] || [ "${PREFIX:-}" = "/data/data/com.termux/files/usr" ]; then
    IS_TERMUX=1
elif uname -o 2>/dev/null | grep -qi android; then
    IS_TERMUX=1
fi

ASSET=""
INSTALL_DIR="${SELENCODE_INSTALL_DIR:-}"

if [ "$IS_TERMUX" -eq 1 ]; then
    case "$ARCH" in
        aarch64|arm64)
            ASSET="selen-termux-arm64"
            ;;
        *)
            err "Arsitektur Termux '$ARCH' belum didukung (hanya aarch64/arm64)."
            ;;
    esac
    if [ -z "$INSTALL_DIR" ]; then
        if [ -n "${PREFIX:-}" ]; then
            INSTALL_DIR="$PREFIX/bin"
        else
            INSTALL_DIR="$HOME/.local/bin"
        fi
    fi
elif [ "$OS" = "Linux" ]; then
    case "$ARCH" in
        x86_64|amd64)
            ASSET="selen-linux-x64"
            ;;
        *)
            err "Arsitektur Linux '$ARCH' belum didukung rilis binary resmi (hanya x86_64). Untuk ARM64 glibc, compile dari source dengan: cargo build --release"
            ;;
    esac
    if [ -z "$INSTALL_DIR" ]; then
        if [ "$(id -u)" -eq 0 ] || [ -w "/usr/local/bin" ]; then
            INSTALL_DIR="/usr/local/bin"
        else
            INSTALL_DIR="$HOME/.local/bin"
        fi
    fi
else
    err "Platform '$OS/$ARCH' belum didukung oleh install.sh. Untuk Windows gunakan install.ps1, atau unduh dari https://github.com/$REPO/releases"
fi

URL="https://github.com/$REPO/releases/latest/download/$ASSET"

# 2. Pastikan folder tujuan ada
mkdir -p "$INSTALL_DIR" || err "Gagal membuat folder $INSTALL_DIR"

TARGET="$INSTALL_DIR/selen"
TMP_FILE="$INSTALL_DIR/.selen.tmp.$$"

cleanup() {
    rm -f "$TMP_FILE"
}
trap cleanup EXIT INT TERM

# 3. Unduh binary terbaru (curl atau fallback wget)
say "Mengunduh SelenCode ($ASSET) dari GitHub Releases..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$TMP_FILE" || err "Unduhan gagal dari $URL. Periksa koneksi internet atau ketersediaan rilis."
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP_FILE" "$URL" || err "Unduhan gagal dari $URL. Periksa koneksi internet atau ketersediaan rilis."
else
    err "Tidak menemukan 'curl' maupun 'wget'. Pasang salah satunya terlebih dahulu."
fi

[ -s "$TMP_FILE" ] || err "File hasil unduhan kosong."

chmod 755 "$TMP_FILE" || err "Gagal mengatur izin eksekusi pada $TMP_FILE"
mv -f "$TMP_FILE" "$TARGET" || err "Gagal memindahkan binary ke $TARGET"
trap - EXIT INT TERM

# 4. Verifikasi pemasangan
say ""
if VER="$("$TARGET" --version 2>/dev/null)"; then
    say "Berhasil memasang $VER di $TARGET"
else
    say "Berhasil memasang binary di $TARGET"
fi

# 5. Cek PATH agar `selen` bisa langsung dipanggil
case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*)
        say "Siap dipakai! Jalankan: selen"
        ;;
    *)
        say ""
        say "Catatan: $INSTALL_DIR belum ada di PATH kamu."
        say "Tambahkan baris berikut ke ~/.bashrc atau ~/.zshrc, lalu buka ulang terminal:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

say ""
say "Langkah cepat pertama kali:"
say "  1. Buat config: ~/.config/selencode/config.toml (atau ./selencode.toml)"
say "  2. Set API key: export SELENCODE_ROUTER_KEY=\"api-key-kamu\""
say "  3. Jalankan:    selen"
