//! Tes v0.1.2 — `selen update`: fungsi murni parse versi, perbandingan,
//! dan pemetaan aset platform. Tanpa jaringan.

use selencode::update::{asset_name_for, is_newer, parse_version};

#[test]
fn parse_version_format_dikenal() {
    assert_eq!(parse_version("v0.1.2"), Some((0, 1, 2)));
    assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
    assert_eq!(parse_version("v10.20.30"), Some((10, 20, 30)));
    assert_eq!(parse_version("0.1.2-rc1"), Some((0, 1, 2)));
    assert_eq!(parse_version("v2.0"), None);
    assert_eq!(parse_version("abc"), None);
    assert_eq!(parse_version(""), None);
    assert_eq!(parse_version("1.2.3.4"), None);
}

#[test]
fn is_newer_perbandingan_semver() {
    assert!(is_newer((0, 1, 2), (0, 1, 1)));
    assert!(is_newer((1, 0, 0), (0, 9, 9)));
    assert!(is_newer((0, 2, 0), (0, 1, 99)));
    assert!(!is_newer((0, 1, 1), (0, 1, 2)));
    assert!(!is_newer((0, 1, 2), (0, 1, 2)), "sama bukan lebih baru");
}

#[test]
fn asset_untuk_platform() {
    assert_eq!(
        asset_name_for("android", "aarch64"),
        Some("selen-termux-arm64")
    );
    assert_eq!(asset_name_for("linux", "x86_64"), Some("selen-linux-x64"));
    // Tidak didukung: tidak ada aset resmi.
    assert_eq!(asset_name_for("windows", "x86_64"), None);
    assert_eq!(asset_name_for("macos", "aarch64"), None);
    assert_eq!(
        asset_name_for("linux", "aarch64"),
        None,
        "glibc ARM tidak dipasok; aset Termux itu bionic"
    );
}
