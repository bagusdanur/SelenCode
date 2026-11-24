use selencode::agent::context::auto_outline;
use selencode::outline::outline_source;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;

fn temp_cwd(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-test-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

const RUST_SRC: &str = "\
const MAX: usize = 10;

struct Config {
    nama: String,
}

enum Mode {
    Cepat,
    Lambat,
}

impl Config {
    fn load() -> Config {
        todo!()
    }

    fn save(&self) {
    }
}

fn main() {
    println!(\"halo\");
}
";

#[test]
fn outline_rust_items_and_lines() {
    let out = outline_source(RUST_SRC, "rs").unwrap();
    assert!(out.contains("const MAX — baris 1"), "{out}");
    assert!(out.contains("struct Config — baris 3-5"), "{out}");
    assert!(out.contains("enum Mode — baris 7-10"), "{out}");
    assert!(out.contains("impl Config — baris 12-19"), "{out}");
    // fn di dalam impl: bersarang (indentasi 2 spasi)
    assert!(out.contains("  fn load() -> Config — baris 13-15"), "{out}");
    assert!(out.contains("  fn save(&self) — baris 17-18"), "{out}");
    assert!(out.contains("fn main() — baris 21-23"), "{out}");
}

const PY_SRC: &str = "\
import os

class Mesin:
    def nyala(self):
        return True

    async def mati(self):
        return False

def bebas(x):
    return x
";

#[test]
fn outline_python_class_methods_nested() {
    let out = outline_source(PY_SRC, "py").unwrap();
    assert!(out.contains("class Mesin — baris 3-8"), "{out}");
    assert!(out.contains("  def nyala(self) — baris 4-5"), "{out}");
    assert!(out.contains("  async def mati(self) — baris 7-8"), "{out}");
    assert!(out.contains("def bebas(x) — baris 10-11"), "{out}");
    // fungsi bebas TIDAK berindentasi
    let line = out.lines().find(|l| l.contains("def bebas")).unwrap();
    assert!(!line.starts_with(' '), "{line}");
}

const JS_SRC: &str = "\
function halo(nama) {
  return \"halo \" + nama;
}

class Kucing {
  suara() {
    return \"meong\";
  }
}

const hitung = (a, b) => a + b;
";

#[test]
fn outline_javascript_items() {
    let out = outline_source(JS_SRC, "js").unwrap();
    assert!(out.contains("function halo(nama) — baris 1-3"), "{out}");
    assert!(out.contains("class Kucing — baris 5-9"), "{out}");
    assert!(out.contains("  suara() — baris 6-8"), "{out}");
    assert!(out.contains("const hitung = (a, b) => — baris 11"), "{out}");
}

const TS_SRC: &str = "\
interface Bentuk {
  luas(): number;
}

type Id = string | number;

enum Warna {
  Merah,
}

function gambar(b: Bentuk): void {
}
";

#[test]
fn outline_typescript_items() {
    let out = outline_source(TS_SRC, "ts").unwrap();
    assert!(out.contains("interface Bentuk — baris 1-3"), "{out}");
    assert!(out.contains("type Id — baris 5"), "{out}");
    assert!(out.contains("enum Warna — baris 7-9"), "{out}");
    assert!(
        out.contains("function gambar(b: Bentuk): void — baris 11-12"),
        "{out}"
    );
}

#[test]
fn outline_unsupported_ext_and_empty_source() {
    let err = outline_source("apa pun isinya", "md").unwrap_err();
    assert!(err.to_string().contains("read_file"), "{err}");
    // sumber kosong -> Ok kosong (bukan error)
    assert_eq!(outline_source("", "rs").unwrap(), "");
    assert_eq!(outline_source("  \n  \n", "py").unwrap(), "");
}

#[test]
fn outline_syntax_error_partial_not_panic() {
    let src = "fn utuh() -> i32 {\n    42\n}\n\nfn rusak( {\n    ini bukan rust sama sekali\n";
    let out = outline_source(src, "rs").unwrap();
    assert!(!out.is_empty(), "outline parsial harus tetap keluar");
    assert!(out.contains("utuh"), "{out}");
}

#[tokio::test]
async fn outline_tool_via_registry_and_guards() {
    let cwd = temp_cwd("outline-tool");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/contoh.rs"),
        "fn dikenal(x: i32) -> i32 {\n    x + 1\n}\n",
    )
    .unwrap();

    let tool = reg.get("outline").expect("tool outline terdaftar");
    assert!(!tool.needs_approval(&json!({"path": "src/contoh.rs"})));

    let out = tool
        .run(json!({"path": "src/contoh.rs"}), &ctx)
        .await
        .unwrap();
    assert!(out.contains("fn dikenal(x: i32) -> i32"), "{out}");

    // file rahasia ditolak
    std::fs::write(cwd.join(".env"), "SECRET=1").unwrap();
    let err = tool
        .run(json!({"path": ".env"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rahasia"), "{err}");

    // di luar sandbox ditolak
    let err = tool
        .run(json!({"path": "../luar.rs"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("di luar folder project"), "{err}");

    // ekstensi tak didukung -> error menyebut read_file
    std::fs::write(cwd.join("catatan.md"), "# halo").unwrap();
    let err = tool
        .run(json!({"path": "catatan.md"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read_file"), "{err}");

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn auto_outline_attaches_mentioned_file() {
    let cwd = temp_cwd("auto-outline");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/contoh.rs"),
        "fn hitung(x: i32) -> i32 {\n    x * 2\n}\n",
    )
    .unwrap();

    let block = auto_outline(&cwd, "perbaiki fungsi hitung di src/contoh.rs ya");
    assert!(block.contains("[Outline otomatis untuk file yang kamu sebut]"), "{block}");
    assert!(block.contains("--- src/contoh.rs ---"), "{block}");
    assert!(block.contains("fn hitung"), "{block}");

    // file tidak ada -> blok kosong, tanpa error
    assert_eq!(auto_outline(&cwd, "lihat src/tidakada.rs dong"), "");
    // path di luar cwd -> blok kosong
    assert_eq!(auto_outline(&cwd, "lihat ../rahasia.rs dong"), "");
    // tanpa penyebutan file -> kosong
    assert_eq!(auto_outline(&cwd, "halo, apa kabar?"), "");

    std::fs::remove_dir_all(&cwd).ok();
}
