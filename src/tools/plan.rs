//! Tool `update_plan` (Fase 6): agent mencatat daftar langkah pekerjaannya
//! ke state bersama di Ctx dan menampilkan checklist-nya.
//! Hanya tersedia di mode eksekusi — di plan mode rencana masih teks bebas.

use super::{schema, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Satu langkah rencana. Status: "pending" | "in_progress" | "done".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub status: String,
}

impl PlanStep {
    pub fn marker(&self) -> &'static str {
        match self.status.as_str() {
            "done" => "[x]",
            "in_progress" => "[>]",
            _ => "[ ]",
        }
    }
}

/// Render checklist teks dari daftar langkah (fungsi murni, mudah dites).
pub fn render_plan(steps: &[PlanStep]) -> String {
    if steps.is_empty() {
        return "Rencana: (kosong)".to_string();
    }
    let mut out = String::from("Rencana:");
    for s in steps {
        out.push_str(&format!("\n{} {}", s.marker(), s.text));
    }
    out
}

pub struct UpdatePlan;

#[async_trait]
impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            description: "Catat / perbarui daftar langkah pekerjaanmu (checklist). \
Panggil di awal pekerjaan besar dan setiap status langkah berubah. \
steps menggantikan daftar sebelumnya secara penuh."
                .into(),
            parameters: schema(
                json!({"steps": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                    },
                    "required": ["text"]
                }}}),
                &["steps"],
            ),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let arr = args
            .get("steps")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("argumen 'steps' wajib berupa array"))?;
        let mut steps: Vec<PlanStep> = Vec::new();
        for (i, item) in arr.iter().enumerate() {
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("steps[{i}].text wajib string non-kosong"))?;
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending")
                .to_string();
            if !matches!(status.as_str(), "pending" | "in_progress" | "done") {
                return Err(anyhow!(
                    "steps[{i}].status tidak valid: '{status}' (pilihan: pending, in_progress, done)"
                ));
            }
            steps.push(PlanStep { text, status });
        }
        let rendered = render_plan(&steps);
        *ctx.plan_state.lock().unwrap() = steps;
        Ok(rendered)
    }
}
