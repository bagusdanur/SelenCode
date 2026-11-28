//! Tes regresi v0.5.31:
//! 1. `/model <nama_tidak_ada>` (mis. `/model azaz`) TIDAK boleh diam-diam membuat & mengaktifkan
//!    model palsu (`lookup_saved_model` mengembalikan `NotFound`).
//! 2. Alur `/model` Telegram menampilkan daftar Provider dulu (`build_tg_provider_picker_card`),
//!    lalu saat provider ditekan (`mdl:m:<idx>:0`) menampilkan isi model beserta tombol kembali
//!    `◀ Provider` (`mdl:p:0`) walau hanya ada 1 provider.

use selencode::repl::{lookup_saved_model, SavedModelLookup};
use selencode::telegram::{
    build_tg_model_picker_card, build_tg_provider_picker_card, parse_callback_model,
    TgModelCallback, TgModelEntry, TgModelProviderItem,
};

#[test]
fn model_tidak_dikenal_seperti_azaz_ditolak_bukan_dibuat_otomatis() {
    let saved = vec![
        "ag/gemini-3-flash".to_string(),
        "ag/gemini-4-hydrogen-medium".to_string(),
    ];

    // Typo / model tidak terdaftar wajib NotFound (tidak boleh dianggap valid!)
    assert_eq!(lookup_saved_model(&saved, "azaz"), SavedModelLookup::NotFound);
    assert_eq!(lookup_saved_model(&saved, "   "), SavedModelLookup::NotFound);
    assert_eq!(
        lookup_saved_model(&saved, "claude-opus-99"),
        SavedModelLookup::NotFound
    );

    // Cocok persis (case-insensitive)
    assert_eq!(
        lookup_saved_model(&saved, "AG/GEMINI-3-FLASH"),
        SavedModelLookup::Exact(0)
    );

    // Cocok unik lewat potongan nama
    assert_eq!(
        lookup_saved_model(&saved, "hydrogen"),
        SavedModelLookup::UniqueSubstring(1)
    );

    // Cocok lebih dari satu -> Ambiguous
    assert_eq!(
        lookup_saved_model(&saved, "gemini"),
        SavedModelLookup::Ambiguous(vec![0, 1])
    );
}

#[test]
fn telegram_model_picker_provider_dulu_lalu_kembali_ke_provider_selalu_tersedia() {
    // 1. Layar pertama `/model` = daftar Provider (bahkan bila hanya ada 1 provider)
    let provs = vec![TgModelProviderItem {
        name: "router".to_string(),
        kind: "openai".to_string(),
        active_model: "ag/gemini-4-hydrogen-medium".to_string(),
        model_count: 2,
        is_active: true,
        key_ready: true,
    }];
    let pcard = build_tg_provider_picker_card(
        "router",
        "openai",
        "ag/gemini-4-hydrogen-medium",
        1_048_576,
        &provs,
        0,
    );
    assert!(pcard.html.contains("Pilih provider (1–1 dari 1)"));
    assert_eq!(pcard.rows[0][0].0, "✓ router (2)");
    assert_eq!(
        parse_callback_model(&pcard.rows[0][0].1),
        Some(TgModelCallback::ModelPage {
            provider_idx: 0,
            page: 0
        })
    );

    // 2. Setelah tekan provider -> layar isi model selalu punya tombol kembali `◀ Provider` (`mdl:p:0`)
    let models = vec![
        TgModelEntry {
            id: "ag/gemini-3-flash".to_string(),
            context_limit: 1_048_576,
            is_active: false,
        },
        TgModelEntry {
            id: "ag/gemini-4-hydrogen-medium".to_string(),
            context_limit: 1_048_576,
            is_active: true,
        },
    ];
    let mcard = build_tg_model_picker_card(
        "router",
        "openai",
        "ag/gemini-4-hydrogen-medium",
        1_048_576,
        0,
        "router",
        "openai",
        1,
        &models,
        0,
    );
    let flat: Vec<&(String, String)> = mcard.rows.iter().flat_map(|r| r.iter()).collect();
    assert!(
        flat.iter()
            .any(|(lbl, cb)| lbl.contains("Provider") && cb == "mdl:p:0"),
        "tombol kembali ke daftar provider wajib selalu hadir: {flat:?}"
    );
}
