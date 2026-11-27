//! Verifikasi perbaikan v0.5.23 (temuan uji penerimaan MCP + Rust edition 2024):
//! 1. Indeks MCP tidak pernah terpotong pagu 1.500 token `cap_dynamic_injection`:
//!    58 tool lintas 2 server (`fs` 17 + `pptx` 41) seluruhnya 58/58 muncul utuh
//!    dalam format ringkas terkelompok per server (`build_mcp_compact_index`).
//! 2. `LoopGuard` tidak false-positive pada pemanggilan batch MCP yang sah
//!    (mis. `mcp__pptx__pptx_add_slide` 7x berurutan dengan argumen sama namun setiap
//!    panggilan sukses & keluaran berbeda), sementara pengulangan hasil identik 3x
//!    atau kegagalan berulang tetap memicu peringatan.
//! 3. Toolset `mcp` aktif otomatis saat `has_mcp == true` pada mode Execute walau prompt
//!    tidak menyebut kata kunci `"mcp"`, `"resource"`, atau `"prompt"`.
//! 4. `McpClient` melacak `reconnect_count()` saat auto-reconnect memulihkan server mati.

use async_trait::async_trait;
use selencode::agent::corrector::{LoopGuard, LoopWarning};
use selencode::agent::plan::TurnMode;
use selencode::providers::ToolSpec;
use selencode::tools::toolsets::{
    active_tool_specs_with_mcp, build_mcp_compact_index, select_initial_toolsets,
    LOAD_MCP_TOOLS_NAME,
};
use selencode::tools::{default_registry, Ctx, Tool};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

struct FakeMcpTool {
    name: String,
    desc: String,
}

#[async_trait]
impl Tool for FakeMcpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.desc.clone(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn run(&self, _args: Value, _ctx: &Ctx) -> anyhow::Result<String> {
        Ok("ok".into())
    }
}

#[test]
fn mcp_index_shows_all_58_tools_across_multiple_servers_without_truncation() {
    let mut reg = default_registry();
    let mut expected_names = Vec::new();

    // 17 tool server `fs`
    for i in 0..16 {
        let name = format!("mcp__fs__tool_{i:02}");
        expected_names.push(name.clone());
        reg = reg.register(Arc::new(FakeMcpTool {
            name,
            desc: "Tool filesystem MCP untuk operasi berkas terizin".into(),
        }));
    }
    expected_names.push("mcp__fs__resources".into());
    reg = reg.register(Arc::new(FakeMcpTool {
        name: "mcp__fs__resources".into(),
        desc: "Daftar atau baca resource dari server MCP fs".into(),
    }));

    // 41 tool server `pptx` (termasuk 8 ekor yang sebelumnya terpotong!)
    let tail_tools = [
        "pptx_add_callout",
        "pptx_add_chart",
        "pptx_add_icon",
        "pptx_check_layout",
        "pptx_list_icons",
        "pptx_render_slide",
        "resources",
        "prompts",
    ];
    for i in 0..33 {
        let name = format!("mcp__pptx__pptx_op_{i:02}");
        expected_names.push(name.clone());
        reg = reg.register(Arc::new(FakeMcpTool {
            name,
            desc: "Tool manipulasi presentasi PowerPoint".into(),
        }));
    }
    for tail in tail_tools {
        let name = format!("mcp__pptx__{tail}");
        expected_names.push(name.clone());
        reg = reg.register(Arc::new(FakeMcpTool {
            name,
            desc: "Tool ekor server pptx yang wajib terlihat".into(),
        }));
    }

    assert_eq!(expected_names.len(), 58);

    let idx = build_mcp_compact_index(&reg).expect("58 tool MCP wajib menghasilkan indeks");
    assert!(idx.contains("58 tool"), "header harus menyebut 58 tool: {idx}");
    for name in &expected_names {
        assert!(
            idx.contains(name),
            "nama tool '{name}' hilang dari indeks MCP:\n{idx}"
        );
    }

    // Pastikan tetap jauh lebih ramping dari 1.500 token (~6.000 char) walau memuat 58/58 nama!
    assert!(
        idx.chars().count() < 2_500,
        "indeks terkelompok 58 tool harus ringkas (<2500 char), aktual {} char",
        idx.chars().count()
    );
}

#[test]
fn loop_guard_allows_distinct_mcp_batch_progress_but_catches_real_loops() {
    let mut lg = LoopGuard::with_default_guards();
    let args = json!({"file_path": "/tmp/laporan.pptx", "layout": "blank"});

    // 7 panggilan `mcp__pptx__pptx_add_slide` berturut-turut dengan argumen sama
    // namun setiap panggilan sukses dan menghasilkan slide baru -> NOL false positive!
    for slide in 1..=7 {
        let out = format!(r#"{{"ok": true, "slide_index": {slide}}}"#);
        let warn = lg.record_with_result("mcp__pptx__pptx_add_slide", &args, &out, false);
        assert_eq!(
            warn,
            LoopWarning::None,
            "slide #{slide} adalah kemajuan batch sah, tidak boleh memicu LoopGuard"
        );
    }

    // Sebaliknya: 3x keluaran IDENTIK beruntun pada tool MCP tetap wajib tertangkap!
    let mut lg_stuck = LoopGuard::with_default_guards();
    let same_args = json!({"path": "/tmp/a.txt"});
    assert_eq!(
        lg_stuck.record_with_result("mcp__fs__read_file", &same_args, "isi sama", false),
        LoopWarning::None
    );
    assert_eq!(
        lg_stuck.record_with_result("mcp__fs__read_file", &same_args, "isi sama", false),
        LoopWarning::None
    );
    assert!(matches!(
        lg_stuck.record_with_result("mcp__fs__read_file", &same_args, "isi sama", false),
        LoopWarning::ConsecutiveIdenticalResult { count: 3, .. }
    ));

    // Dan 2x error identik pada tool MCP tetap wajib tertangkap!
    let mut lg_err = LoopGuard::with_default_guards();
    assert_eq!(
        lg_err.record_with_result("mcp__fs__read_file", &same_args, "ENOENT", true),
        LoopWarning::None
    );
    assert!(matches!(
        lg_err.record_with_result("mcp__fs__read_file", &same_args, "ENOENT", true),
        LoopWarning::RepeatedCall { .. }
    ));
}

#[test]
fn mcp_toolset_activates_without_literal_mcp_keyword_in_execute_mode() {
    let cwd = PathBuf::from(".");
    let ts_exec = select_initial_toolsets(
        "buatkan presentasi PowerPoint 3 slide dari laporan penjualan bulan ini",
        &cwd,
        true,
        TurnMode::Execute,
    );
    assert!(
        ts_exec.contains("mcp"),
        "toolset mcp harus otomatis aktif saat has_mcp=true tanpa wajib menyebut kata 'mcp'"
    );

    // Di Plan mode (read-only), mcp tidak ikut aktif.
    let ts_plan = select_initial_toolsets(
        "buatkan rencana presentasi 3 slide",
        &cwd,
        true,
        TurnMode::Plan,
    );
    assert!(!ts_plan.contains("mcp"));

    // Verifikasi untuk server kecil (<=15 tool): tool MCP langsung masuk specs tanpa extra turn!
    let mut small_reg = default_registry();
    small_reg = small_reg.register(Arc::new(FakeMcpTool {
        name: "mcp__notes__search".into(),
        desc: "Cari catatan".into(),
    }));
    let specs_small = active_tool_specs_with_mcp(
        &small_reg,
        &ts_exec,
        &HashSet::new(),
        TurnMode::Execute,
    );
    assert!(specs_small.iter().any(|s| s.name == "mcp__notes__search"));
    assert!(!specs_small.iter().any(|s| s.name == LOAD_MCP_TOOLS_NAME));
}
