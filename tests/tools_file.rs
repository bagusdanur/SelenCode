use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;

fn temp_cwd(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("selencode-test-{}-{}", tag, uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

async fn run_tool(reg: &selencode::tools::Registry, name: &str, args: serde_json::Value, ctx: &Ctx) -> String {
    reg.get(name).unwrap().run(args, ctx).await.unwrap()
}

#[tokio::test]
async fn write_read_edit_list_search_roundtrip() {
    let cwd = temp_cwd("roundtrip");
    let ctx = Ctx::new(cwd.clone(), true); // auto-approve
    let reg = default_registry();

    let out = run_tool(&reg, "write_file", json!({"path": "src/hello.txt", "content": "halo dunia\nbaris dua\n"}), &ctx).await;
    assert!(out.contains("ditulis"), "{out}");

    let out = run_tool(&reg, "read_file", json!({"path": "src/hello.txt"}), &ctx).await;
    assert!(out.contains("halo dunia"), "{out}");

    // rentang baris
    let out = run_tool(&reg, "read_file", json!({"path": "src/hello.txt", "start": 2, "end": 2}), &ctx).await;
    assert_eq!(out, "baris dua");

    let out = run_tool(&reg, "edit_file", json!({"path": "src/hello.txt", "old": "baris dua", "new": "baris BARU"}), &ctx).await;
    assert!(out.contains("diedit"), "{out}");
    let out = run_tool(&reg, "read_file", json!({"path": "src/hello.txt"}), &ctx).await;
    assert!(out.contains("baris BARU"), "{out}");

    let out = run_tool(&reg, "list_files", json!({}), &ctx).await;
    assert!(out.contains("hello.txt"), "{out}");

    let out = run_tool(&reg, "search_text", json!({"query": "halo"}), &ctx).await;
    assert!(out.contains("hello.txt:1"), "{out}");

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn edit_rejects_non_unique_old() {
    let cwd = temp_cwd("nonunique");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();
    run_tool(&reg, "write_file", json!({"path": "a.txt", "content": "sama\nsama\n"}), &ctx).await;
    let err = reg
        .get("edit_file")
        .unwrap()
        .run(json!({"path": "a.txt", "old": "sama", "new": "beda"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unik"), "{err}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn sandbox_rejects_escape_and_secret_read() {
    let cwd = temp_cwd("sandbox");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();

    let err = reg
        .get("read_file")
        .unwrap()
        .run(json!({"path": "../di-luar.txt"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("di luar folder project"), "{err}");

    let err = reg
        .get("write_file")
        .unwrap()
        .run(json!({"path": "../kabur.txt", "content": "x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("di luar folder project"), "{err}");

    std::fs::write(cwd.join(".env"), "SECRET=1").unwrap();
    let err = reg
        .get("read_file")
        .unwrap()
        .run(json!({"path": ".env"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rahasia"), "{err}");

    // search tidak membocorkan isi .env
    let out = run_tool(&reg, "search_text", json!({"query": "SECRET"}), &ctx).await;
    assert!(!out.contains(".env"), "{out}");

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn gitignore_is_respected() {
    let cwd = temp_cwd("gitignore");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();
    std::fs::write(cwd.join(".gitignore"), "diabaikan/\n").unwrap();
    run_tool(&reg, "write_file", json!({"path": "diabaikan/x.txt", "content": "sembunyi unik123"}), &ctx).await;
    run_tool(&reg, "write_file", json!({"path": "kelihatan.txt", "content": "unik123"}), &ctx).await;

    let out = run_tool(&reg, "list_files", json!({}), &ctx).await;
    assert!(out.contains("kelihatan.txt"), "{out}");
    assert!(!out.contains("diabaikan"), "{out}");

    let out = run_tool(&reg, "search_text", json!({"query": "unik123"}), &ctx).await;
    assert!(out.contains("kelihatan.txt"), "{out}");
    assert!(!out.contains("diabaikan"), "{out}");
    std::fs::remove_dir_all(&cwd).ok();
}
