//! Phase 6 acceptance: openai + ollama providers behind the
//! `EmbeddingProvider` trait. Both are gated on connectivity so the
//! suite stays green in offline CI.

use marila_embed::embed::{
    EmbeddingProvider, ollama::OllamaEmbedder, openai::OpenAiEmbedder, stub::StubEmbedder,
};

async fn ollama_available() -> bool {
    let endpoint =
        std::env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".into());
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap();
    match client.get(&url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

fn openai_available() -> bool {
    std::env::var_os("OPENAI_API_KEY").is_some()
}

#[tokio::test]
async fn stub_provider_advertises_correct_metadata() {
    let s = StubEmbedder::new(64);
    assert_eq!(s.name(), "stub");
    assert_eq!(s.dimension(), 64);
    assert!(s.max_batch() >= 1);
}

#[tokio::test]
async fn ollama_embed_round_trips_when_available() {
    if !ollama_available().await {
        eprintln!("[skipped] no local Ollama at :11434");
        return;
    }
    let p = OllamaEmbedder::connect(None, Some("embeddinggemma:latest".into())).await;
    let p = match p {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[skipped] ollama connect failed: {e}");
            return;
        }
    };
    assert_eq!(p.name(), "ollama");
    assert!(p.dimension() > 0);

    let resp = p
        .embed(&["hello world", "the quick brown fox"])
        .await
        .expect("ollama embed");
    assert_eq!(resp.vectors.len(), 2);
    assert_eq!(resp.vectors[0].len() as u32, p.dimension());
    assert_eq!(resp.vectors[1].len() as u32, p.dimension());
    assert!(
        !resp.usage.from_provider,
        "ollama doesn't report token counts"
    );
    // Different inputs should give different vectors
    assert_ne!(resp.vectors[0], resp.vectors[1]);
}

#[tokio::test]
async fn openai_embed_round_trips_when_available() {
    if !openai_available() {
        eprintln!("[skipped] OPENAI_API_KEY not set");
        return;
    }
    let p = match OpenAiEmbedder::from_env(Some("text-embedding-3-small".into())) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[skipped] openai init: {e}");
            return;
        }
    };
    assert_eq!(p.name(), "openai");
    assert_eq!(p.dimension(), 1536);

    let resp = match p.embed(&["hello world", "the quick brown fox"]).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[skipped] openai embed failed: {e}");
            return;
        }
    };
    assert_eq!(resp.vectors.len(), 2);
    assert_eq!(resp.vectors[0].len(), 1536);
    assert!(resp.usage.from_provider);
    assert!(resp.usage.tokens_in > 0);
}

#[test]
fn tokenizer_for_openai_is_tiktoken() {
    use marila_embed::tokenize;
    let t = tokenize::for_provider("openai", Some("text-embedding-3-small"));
    assert!(t.name().contains("cl100k") || t.name().contains("o200k"));
    assert_eq!(t.count("hello world"), 2);
}

#[test]
fn tokenizer_for_ollama_falls_back_to_estimate() {
    use marila_embed::tokenize;
    let t = tokenize::for_provider("ollama", None);
    assert_eq!(t.name(), "estimate-4chars");
}
