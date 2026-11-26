//! Uji unit & regresi SelenCode v0.5.16 (3 Butir):
//! - Perbaikan 1: `TypingLoopGuard` menggunakan sinyal berhenti teringat (`watch`) + abort eksplisit
//!   sehingga sinyal yang tiba di tengah `send_typing().await` lambat TIDAK pernah hilang dan tidak
//!   meninggalkan task pengetik yatim sesudah giliran selesai.
//! - Perbaikan 2: Klasifier `prompt_requests_file_change` menangkap bentuk nomina & verba Indonesia
//!   (mis. "Ada perbaikan yang harus dilakukan...", "perlu perubahan...", "butuh penambahan...")
//!   sambil menolak kontrol negatif diskusi/penjelasan ("apa perbaikan terbaik...?",
//!   "jelaskan perubahan...", "kenapa perbaikan kemarin gagal?").
//! - Perbaikan 3: Konvergensi pada pagu kecil (<= 6) tidak menembak prematur di langkah 1–2
//!   (pagu 6 -> langkah 3), sementara pagu 15 -> 10 dan pagu 25 -> 20 tetap persis v0.5.15.

use selencode::agent::{compute_convergence_step, prompt_requests_file_change};
use selencode::telegram::TypingLoopGuard;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn find_python() -> String {
    for cand in ["python3", "python"] {
        if let Ok(out) = Command::new(cand).arg("--version").output() {
            let txt = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if out.status.success() && txt.contains("Python 3") {
                return cand.to_string();
            }
        }
    }
    panic!("python3/python 3 tidak ditemukan di PATH");
}

// =========================================================================
// PERBAIKAN 1: Uji lomba sinyal berhenti di tengah `send_typing()` lambat
// =========================================================================

#[tokio::test]
async fn p1_typing_loop_stop_signal_during_slow_send_never_lost_or_leaked() {
    // 1. Sinyal berhenti tiba TEPAT di tengah `send_typing().await` yang lambat (600ms):
    //    - Loop harus langsung berhenti (tidak menunggu selesai + tidak masuk putaran tidur berikutnya).
    //    - Sesudah `stop_and_join`, tidak boleh ada satu pun panggilan `send_typing` lagi!
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls_c = calls.clone();
    let entered_c = entered.clone();

    let guard = TypingLoopGuard::spawn(Duration::from_millis(80), move || {
        let calls_i = calls_c.clone();
        let entered_i = entered_c.clone();
        async move {
            calls_i.fetch_add(1, Ordering::SeqCst);
            entered_i.notify_one();
            // Simulasikan panggilan HTTP `sendChatAction(typing)` lambat (600ms)
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
    });

    // Tunggu sampai task benar-benar sedang berada di DALAM `send_typing().await`
    entered.notified().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let t0 = Instant::now();
    guard.stop_and_join(Duration::from_millis(500)).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "sinyal berhenti di tengah send_typing harus membatalkan & menghentikan loop seketika, butuh {elapsed:?}"
    );

    // Tunggu melampaui beberapa interval (250ms > 3x 80ms) dan pastikan nol panggilan tambahan!
    let count_after_stop = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        count_after_stop,
        "task pengetik yatim TIDAK boleh terus mengirim sendChatAction sesudah berhenti!"
    );

    // 2. Jalur Drop (mis. giliran berhenti dini / dibatalkan / galat / panik):
    //    menjatuhkan `TypingLoopGuard` tanpa memanggil `stop_and_join` tetap meng-abort task!
    let drop_calls = Arc::new(AtomicUsize::new(0));
    let drop_entered = Arc::new(tokio::sync::Notify::new());
    let dc = drop_calls.clone();
    let de = drop_entered.clone();
    {
        let _guard_dropped = TypingLoopGuard::spawn(Duration::from_millis(40), move || {
            let dci = dc.clone();
            let dei = de.clone();
            async move {
                dci.fetch_add(1, Ordering::SeqCst);
                dei.notify_one();
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        drop_entered.notified().await;
        // `_guard_dropped` di-drop di akhir blok ini!
    }
    let count_on_drop = drop_calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(
        drop_calls.load(Ordering::SeqCst),
        count_on_drop,
        "Drop TypingLoopGuard wajib menghentikan task tanpa tersisa yatim!"
    );
}

// =========================================================================
// PERBAIKAN 2: Klasifier nomina + verba Indonesia & kontrol negatif diskusi
// =========================================================================

#[test]
fn p2_indonesian_nominal_verbal_and_negative_discussion_classifier() {
    // Positif nomina Indonesia (kasus nyata yang sebelumnya terlewat di v0.5.15!)
    let positive_nominal = [
        "Ada perbaikan yang harus dilakukan pada berkas hitung_total.js.",
        "Perlu perubahan pada konfigurasi parser di src/config.rs.",
        "Butuh penambahan validasi input di modul autentikasi.",
        "Sekalian perbaikannya untuk fungsi diskon di calc.js.",
        "Ada penghapusan field lama yang belum dikerjakan di schema.sql.",
        "Mohon penyesuaian format tanggal di laporan bulanan.",
        "Lakukan pembaruan dokumentasi pada config.example.toml.",
        "Bantu pembenahan kode di handler telegram.",
    ];
    for p in positive_nominal {
        assert!(
            prompt_requests_file_change(p),
            "positif nomina Indonesia WAJIB terklasifikasi true: {p:?}"
        );
    }

    // Positif verba (aktif & pasif) Indonesia
    let positive_verbal = [
        "Tolong diperbaiki fungsi pembagian nol di calc.js.",
        "Perbaiki bug diskon di hitung_total.js agar hasilnya 31500.",
        "Ubah nilai default timeout di src/config.rs.",
        "Tambahkan unit test untuk kasus kosong.",
        "Hapus kode duplikat di src/agent/loop.rs.",
        "Kenapa hitung_total.js salah? Tolong perbaiki sekalian.",
    ];
    for p in positive_verbal {
        assert!(
            prompt_requests_file_change(p),
            "positif verba Indonesia WAJIB terklasifikasi true: {p:?}"
        );
    }

    // Kontrol negatif wajib: pertanyaan diskusi / penjelasan tentang perubahan BUKAN tugas perubahan!
    let negative_discussion = [
        "apa perbaikan terbaik untuk fungsi ini?",
        "jelaskan perubahan di berkas ini",
        "kenapa perbaikan kemarin gagal?",
        "Bagaimana cara kerja penambahan fitur ini?",
        "Sebutkan daftar perubahan di commit terakhir.",
        "Ringkas perbaikan yang dilakukan pada rilis kemarin.",
        "Apakah perubahan ini aman untuk produksi?",
        "Jelaskan apa itu borrow checker.",
        "Menurutmu apa penyesuaian yang paling bersih?",
    ];
    for p in negative_discussion {
        assert!(
            !prompt_requests_file_change(p),
            "kontrol negatif diskusi WAJIB false (bukan tugas perubahan): {p:?}"
        );
    }
}

// =========================================================================
// PERBAIKAN 3: Titik tembak konvergensi pagu kecil (6) vs 15 & 25 + laporan
// =========================================================================

#[test]
fn p3_small_budget_convergence_not_premature_and_v0516_report_passes_gate() {
    // Pagu kecil (<= 6): TIDAK boleh menembak prematur di langkah 1–2!
    let at_6 = compute_convergence_step(6);
    assert!(
        at_6 >= 3 && at_6 < 6,
        "pagu 6 TIDAK boleh menembak di langkah 1–2, dapat langkah {at_6}"
    );
    assert_eq!(at_6, 3, "pagu 6 menembak di langkah 3 (50% eksplorasi minimal)");
    assert_eq!(compute_convergence_step(5), 3);
    assert_eq!(compute_convergence_step(4), 3);

    // Pagu 15 & 25 & 100 tetap PERSIS seperti v0.5.15 (tidak bergeser sedikit pun!)
    assert_eq!(compute_convergence_step(15), 10);
    assert_eq!(compute_convergence_step(25), 20);
    assert_eq!(compute_convergence_step(100), 85);
    assert_eq!(compute_convergence_step(0), 0);

    // Verifikasi mandiri laporan rilis v0.5.16 (3 butir + 'pengukuran penerimaan oleh Muse')
    let py = find_python();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts").join("check_release_report.py");
    let report = root
        .join("docs")
        .join("reports")
        .join("HASIL-V0516-TYPING-KLASIFIER-KONVERGENSI.md");
    let out = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.16")
        .arg("--report")
        .arg(&report)
        .arg("--expected-items")
        .arg("3")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "laporan rilis v0.5.16 wajib lolos check_release_report.py! stdout={stdout} stderr={stderr}"
    );
}
