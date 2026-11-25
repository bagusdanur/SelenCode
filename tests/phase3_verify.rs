//! Tes Fase 3 — Advanced Verification Engine (targeted test selection, pembedaan status, & evidence gate).

use selencode::agent::corrector::SelfCorrectState;
use selencode::tools::verify::{classify_step_output, detect_targeted_plan, ProjectKind, VerifyStatus};
use serde_json::json;
use std::path::PathBuf;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-phase3-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn classify_step_output_distinguishes_passed_failed_and_no_tests_found() {
    // Rust: 0 passed -> NoTestsFound (TIDAK boleh diklaim lulus!)
    let (st0, n0) = classify_step_output(
        "cargo test",
        Some(0),
        "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored;",
    );
    assert_eq!(st0, VerifyStatus::NoTestsFound);
    assert_eq!(n0, Some(0));

    // Rust: 7 passed -> Passed dengan bukti jumlah test
    let (st1, n1) = classify_step_output(
        "cargo test",
        Some(0),
        "running 7 tests\ntest result: ok. 7 passed; 0 failed; 0 ignored;",
    );
    assert_eq!(st1, VerifyStatus::Passed);
    assert_eq!(n1, Some(7));

    // Pytest exit 5 (no tests ran) -> NoTestsFound
    let (st2, _) = classify_step_output("pytest", Some(5), "collected 0 items\nno tests ran in 0.01s");
    assert_eq!(st2, VerifyStatus::NoTestsFound);

    // Go [no test files] -> NoTestsFound
    let (st3, _) = classify_step_output("go test", Some(0), "?   \tmyapp/pkg\t[no test files]");
    assert_eq!(st3, VerifyStatus::NoTestsFound);

    // Non-zero exit -> Failed
    let (st4, _) = classify_step_output("cargo test", Some(101), "error[E0308]: mismatched types");
    assert_eq!(st4, VerifyStatus::Failed);
}

#[test]
fn targeted_plan_selects_relevant_tests_for_rust_python_and_go() {
    // 1) Rust project: edit `src/sandbox.rs` -> otomatis pilih `tests/phase2_sandbox.rs`!
    let rs_dir = temp_dir("rs");
    std::fs::write(rs_dir.join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
    std::fs::create_dir_all(rs_dir.join("tests")).unwrap();
    std::fs::write(rs_dir.join("tests/phase2_sandbox.rs"), "").unwrap();
    std::fs::write(rs_dir.join("tests/other.rs"), "").unwrap();

    let rs_plan = detect_targeted_plan(&rs_dir, "targeted", &["src/sandbox.rs".to_string()]);
    assert_eq!(rs_plan.kind, ProjectKind::Rust);
    assert!(rs_plan
        .steps
        .iter()
        .any(|s| s.command == "cargo test --test phase2_sandbox"));

    // Format scope
    let fmt_plan = detect_targeted_plan(&rs_dir, "format", &[]);
    assert_eq!(fmt_plan.steps[0].command, "cargo fmt --check");
    std::fs::remove_dir_all(&rs_dir).ok();

    // 2) Python project: edit `billing.py` -> otomatis pilih `tests/test_billing.py`
    let py_dir = temp_dir("py");
    std::fs::write(py_dir.join("pyproject.toml"), "[project]\nname='demo'\n").unwrap();
    std::fs::create_dir_all(py_dir.join("tests")).unwrap();
    std::fs::write(py_dir.join("tests/test_billing.py"), "def test_ok(): pass\n").unwrap();

    let py_plan = detect_targeted_plan(&py_dir, "targeted", &["src/billing.py".to_string()]);
    assert_eq!(py_plan.kind, ProjectKind::Python);
    assert_eq!(py_plan.steps[0].command, "pytest -q tests/test_billing.py");
    std::fs::remove_dir_all(&py_dir).ok();

    // 3) Go project: edit `pkg/auth/token.go` -> otomatis pilih `go test ./pkg/auth`
    let go_dir = temp_dir("go");
    std::fs::write(go_dir.join("go.mod"), "module demo\n").unwrap();
    let go_plan = detect_targeted_plan(&go_dir, "targeted", &["pkg/auth/token.go".to_string()]);
    assert_eq!(go_plan.kind, ProjectKind::Go);
    assert_eq!(go_plan.steps[0].command, "go test ./pkg/auth");
    std::fs::remove_dir_all(&go_dir).ok();
}

#[test]
fn corrector_rejects_no_tests_and_timeout_as_passing_evidence() {
    let mut st = SelfCorrectState::new(Some(1));
    st.observe_tool("edit_file", &json!({"path": "src/lib.rs"}), "OK", false);
    assert!(!st.verified_after_last_edit);

    // NO_TESTS tidak boleh dianggap lulus!
    st.observe_tool(
        "verify_project",
        &json!({"scope": "test"}),
        "Verifikasi proyek [Rust (Cargo)]: TIDAK ADA TEST ⚠️\n  ⚠ cargo test (exit 0) [NO_TESTS]",
        false,
    );
    assert_eq!(st.last_verify_passed, Some(false));

    // TIMEOUT tidak boleh dianggap lulus!
    st.observe_tool(
        "verify_project",
        &json!({"scope": "test"}),
        "Verifikasi proyek [Rust (Cargo)]: TIMEOUT ⏱",
        false,
    );
    assert_eq!(st.last_verify_passed, Some(false));

    // Verifikasi nyata LULUS -> barulah completion_gate mengizinkan selesai!
    st.observe_tool(
        "verify_project",
        &json!({"scope": "targeted"}),
        "Verifikasi proyek [Rust (Cargo)]: LULUS ✅\n  ✓ cargo test (exit 0 · 5 test) [LULUS]",
        false,
    );
    assert_eq!(st.last_verify_passed, Some(true));
    assert!(st.completion_gate(5).is_none());
}
