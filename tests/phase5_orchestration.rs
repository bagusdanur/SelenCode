//! Tes Fase 5 — Intelligent Model Orchestration (SQLite learning, eskalasi pasca-verifikasi,
//! budget guard, & alasan routing transparan).

use async_trait::async_trait;
use selencode::providers::router::{
    detect_escalation_needed, DynamicRouterProvider, ModelRoute, RouterTelemetry,
};
use selencode::providers::{AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage};
use selencode::store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

struct EchoModelProvider {
    tag: String,
    cost_tokens: u64,
}

#[async_trait]
impl Provider for EchoModelProvider {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let _ = tx.send(StreamEvent::Token(self.tag.clone())).await;
        Ok(AssistantReply {
            text: self.tag.clone(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: self.cost_tokens,
                output_tokens: self.cost_tokens,
                ..Default::default()
            }),
        })
    }

    fn model_name(&self) -> &str {
        &self.tag
    }
}

#[tokio::test]
async fn router_learns_from_sqlite_history_overriding_model_name_assumptions() {
    let db = std::env::temp_dir().join(format!(
        "selencode-phase5-perf-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Arc::new(Store::open_at(&db).unwrap());

    // Simulasikan histori benchmark/eksekusi: model "alpha-custom" 100% sukses di coding,
    // sedangkan "gpt-5-coder" (walau namanya coder!) gagal 5x berturut-turut!
    for _ in 0..5 {
        store
            .record_model_outcome("p:alpha-custom", "coding", true, 500, 1000, 0.001)
            .unwrap();
        store
            .record_model_outcome("p:gpt-5-coder", "coding", false, 900, 1000, 0.01)
            .unwrap();
    }

    let p_bad = Arc::new(EchoModelProvider {
        tag: "from-gpt-5-coder".into(),
        cost_tokens: 1000,
    });
    let p_good = Arc::new(EchoModelProvider {
        tag: "from-alpha-custom".into(),
        cost_tokens: 1000,
    });

    let telemetry = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("p", "gpt-5-coder", p_bad, Some(200_000), Some(3.0), Some(15.0), true),
            ModelRoute::new("p", "alpha-custom", p_good, Some(128_000), Some(1.0), Some(3.0), false),
        ],
        true,
        HashMap::new(),
        1,
        telemetry.clone(),
    )
    .with_store(store.clone());

    let (tx, _rx) = mpsc::channel(8);
    let reply = router
        .stream_chat(&[Message::text(Role::User, "tambahkan fungsi parser baru")], &[], tx)
        .await
        .unwrap();

    // Performa nyata dari SQLite mengalahkan asumsi nama model!
    assert_eq!(reply.text, "from-alpha-custom");
    let dec = telemetry.lock().unwrap().last_decision.clone().unwrap();
    assert_eq!(dec.route_id, "p:alpha-custom");
    assert!(dec.reason.contains("histori 100%"));

    // Outcome sukses baru otomatis tercatat kembali ke SQLite!
    let perf = store.load_model_perf().unwrap();
    let alpha = perf.iter().find(|r| r.route_id == "p:alpha-custom").unwrap();
    assert_eq!(alpha.successes, 6);

    std::fs::remove_file(&db).ok();
}

#[tokio::test]
async fn router_escalates_after_verification_failure_and_enforces_cost_budget() {
    let msgs_after_fail = vec![
        Message::text(Role::User, "ringkas lalu perbaiki modul"),
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "call_1".into(),
                content: "Verifikasi proyek [Rust (Cargo)]: GAGAL ❌".into(),
                is_error: false,
            }],
        },
    ];
    assert!(detect_escalation_needed(&msgs_after_fail));

    let p_fast = Arc::new(EchoModelProvider {
        tag: "from-flash".into(),
        cost_tokens: 500_000,
    });
    let p_strong = Arc::new(EchoModelProvider {
        tag: "from-opus".into(),
        cost_tokens: 500_000,
    });

    let telemetry = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("p", "gemini-flash", p_fast, Some(1_000_000), Some(0.1), Some(0.4), true),
            ModelRoute::new("p", "claude-opus", p_strong, Some(200_000), Some(10.0), Some(20.0), false),
        ],
        true,
        HashMap::new(),
        1,
        telemetry.clone(),
    )
    .with_max_cost_usd(Some(1.0));

    // Panggilan 1: eskalasi memilih model kuat (`claude-opus`), yang menghabiskan > $1.0
    let (tx1, _rx1) = mpsc::channel(8);
    let r1 = router.stream_chat(&msgs_after_fail, &[], tx1).await.unwrap();
    assert_eq!(r1.text, "from-opus");
    assert!(telemetry.lock().unwrap().last_decision.as_ref().unwrap().escalated);

    // Panggilan 2: budget sesi ($1.0) sudah tercapai -> wajib ditolak bersih oleh budget guard!
    let (tx2, _rx2) = mpsc::channel(8);
    let r2 = router
        .stream_chat(&[Message::text(Role::User, "lanjutkan")], &[], tx2)
        .await;
    assert!(r2.is_err());
    assert!(r2.unwrap_err().to_string().contains("batas biaya"));
}
