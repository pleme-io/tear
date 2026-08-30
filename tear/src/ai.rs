//! `tear ai` — LLM proxy. Assembles context from the latest
//! captured pane block + forwards to a configured LLM. Local-first
//! (defaults to Ollama on 127.0.0.1:11434), no telemetry, no
//! cloud requirement.
//!
//! ## Provider trait
//!
//! [`LlmProvider`] hides the wire shape so callers don't care
//! whether we're talking to Ollama, an OpenAI-compatible /v1/chat,
//! Anthropic /v1/messages, or a future addition. Three concrete
//! impls ship today:
//!
//! - [`OllamaProvider`] — Ollama's `/api/generate`.
//! - [`OpenAiProvider`] — any `/v1/chat/completions` endpoint
//!   (OpenAI, Together, Groq, Mistral, …).
//! - [`MockProvider`] — test-only; canned responses.
//!
//! ## Context assembly
//!
//! [`assemble_prompt`] is pure (no IO) so tests cover it directly.
//! Shape:
//!
//! ```text
//! <operator prompt>
//!
//! ## context
//! cwd: /Users/me/code
//! command: cargo test
//! exit_code: 101
//! output (tail, 2000 bytes):
//! …
//! ```

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use tear_config::AiConfig;
use tear_types::Block;

/// Trait every concrete provider satisfies. Sync because the CLI
/// is one-shot — no point pulling in tokio.
pub trait LlmProvider {
    fn generate(&self, prompt: &str) -> Result<String>;
}

/// Compose the final user-facing prompt: operator's question +
/// typed context drawn from the latest block. Pure — no IO.
#[must_use]
pub fn assemble_prompt(user_prompt: &str, block: Option<&Block>, context_bytes: usize) -> String {
    let mut out = String::with_capacity(user_prompt.len() + 1024);
    out.push_str(user_prompt);
    out.push_str("\n\n## context\n");
    match block {
        Some(b) => {
            if let Some(cwd) = &b.cwd {
                out.push_str(&format!("cwd: {cwd}\n"));
            }
            out.push_str(&format!("command: {}\n", b.command.trim_end()));
            if let Some(c) = b.exit_code {
                out.push_str(&format!("exit_code: {c}\n"));
            }
            out.push_str(&format!("output (tail, up to {context_bytes} bytes):\n"));
            // Tail of the output (LLMs benefit more from the
            // last N bytes than the first N — errors land at
            // the end).
            let total = b.output.len();
            let start = total.saturating_sub(context_bytes);
            out.push_str(&b.output[start..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        None => {
            out.push_str("(no block context — pane has no captured blocks yet)\n");
        }
    }
    out
}

/// Pick the right provider impl from a [`AiConfig`].
pub fn provider_from_config(cfg: &AiConfig) -> Result<Box<dyn LlmProvider>> {
    match cfg.provider.as_str() {
        "ollama" => Ok(Box::new(OllamaProvider::new(
            cfg.endpoint.clone(),
            cfg.model.clone(),
        ))),
        "openai" | "openai-compatible" => Ok(Box::new(OpenAiProvider::new(
            cfg.endpoint.clone(),
            cfg.model.clone(),
            resolve_api_key(cfg.api_key_env.as_deref())?,
        ))),
        other => Err(anyhow!(
            "unknown ai.provider `{other}` — accepted: ollama | openai | openai-compatible"
        )),
    }
}

fn resolve_api_key(env_name: Option<&str>) -> Result<String> {
    let name = env_name
        .ok_or_else(|| anyhow!("ai.api_key_env must be set for openai-compatible providers"))?;
    std::env::var(name)
        .map_err(|_| anyhow!("env var `{name}` not set (configured via ai.api_key_env)"))
}

// ── Ollama ──────────────────────────────────────────────────────

pub struct OllamaProvider {
    endpoint: String,
    model: String,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(endpoint: String, model: String) -> Self {
        Self { endpoint, model }
    }
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

impl LlmProvider for OllamaProvider {
    fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/api/generate", self.endpoint.trim_end_matches('/'));
        let body = OllamaRequest {
            model: &self.model,
            prompt,
            stream: false,
        };
        let resp = ureq::post(&url)
            .send_json(&body)
            .map_err(|e| anyhow!("ollama HTTP error: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow!("ollama read body: {e}"))?;
        let parsed: OllamaResponse =
            serde_json::from_str(&resp).map_err(|e| anyhow!("ollama parse: {e}\nraw: {resp}"))?;
        Ok(parsed.response)
    }
}

// ── OpenAI-compatible /v1/chat/completions ────────────────────

pub struct OpenAiProvider {
    endpoint: String,
    model: String,
    api_key: String,
}

impl OpenAiProvider {
    #[must_use]
    pub fn new(endpoint: String, model: String, api_key: String) -> Self {
        Self {
            endpoint,
            model,
            api_key,
        }
    }
}

#[derive(Serialize)]
struct OaiRequest<'a> {
    model: &'a str,
    messages: Vec<OaiMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct OaiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiResponseMessage,
}

#[derive(Deserialize)]
struct OaiResponseMessage {
    content: String,
}

impl LlmProvider for OpenAiProvider {
    fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!(
            "{}/v1/chat/completions",
            self.endpoint.trim_end_matches('/')
        );
        let body = OaiRequest {
            model: &self.model,
            messages: vec![OaiMessage {
                role: "user",
                content: prompt,
            }],
            stream: false,
        };
        let resp = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(&body)
            .map_err(|e| anyhow!("openai HTTP error: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow!("openai read body: {e}"))?;
        let parsed: OaiResponse =
            serde_json::from_str(&resp).map_err(|e| anyhow!("openai parse: {e}\nraw: {resp}"))?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mock provider lives next to the tests that consume it ──

    pub struct MockProvider {
        pub canned: String,
    }

    impl LlmProvider for MockProvider {
        fn generate(&self, _prompt: &str) -> Result<String> {
            Ok(self.canned.clone())
        }
    }

    fn sample_block(cwd: Option<&str>) -> Block {
        Block {
            index: 7,
            prompt: "$ ".into(),
            command: "cargo test".into(),
            output: "thread 'main' panicked at lib.rs:42\n".into(),
            exit_code: Some(101),
            started_at_unix_ms: 1_000,
            ended_at_unix_ms: Some(2_000),
            cwd: cwd.map(String::from),
            // A fixture block has no attested connection behind
            // it, so Unknown is the honest value — not Human.
            yurai: tear_types::Yurai::Unknown,
        }
    }

    #[test]
    fn assemble_includes_user_prompt_first() {
        let p = assemble_prompt("why did this fail", Some(&sample_block(None)), 2000);
        assert!(p.starts_with("why did this fail"));
    }

    #[test]
    fn assemble_emits_command_and_exit_code() {
        let p = assemble_prompt("?", Some(&sample_block(None)), 2000);
        assert!(p.contains("command: cargo test"));
        assert!(p.contains("exit_code: 101"));
    }

    #[test]
    fn assemble_truncates_output_to_context_bytes() {
        let mut b = sample_block(None);
        b.output = "x".repeat(5_000);
        let p = assemble_prompt("?", Some(&b), 200);
        // The tail-of-output xs are the dominant contribution;
        // the header text has a couple of incidental 'x's (e.g.
        // "exit_code"). The intent is "the tail got truncated to
        // ~context_bytes", not exact equality.
        let xs = p.matches('x').count();
        assert!(
            (200..=205).contains(&xs),
            "expected ~200 x's, got {xs}\nprompt:\n{p}"
        );
    }

    #[test]
    fn assemble_omits_cwd_line_when_none() {
        let p = assemble_prompt("?", Some(&sample_block(None)), 2000);
        assert!(!p.contains("cwd:"));
    }

    #[test]
    fn assemble_emits_cwd_line_when_present() {
        let p = assemble_prompt("?", Some(&sample_block(Some("/tmp/foo"))), 2000);
        assert!(p.contains("cwd: /tmp/foo"));
    }

    #[test]
    fn assemble_handles_no_block() {
        let p = assemble_prompt("?", None, 2000);
        assert!(p.contains("(no block context"));
    }

    #[test]
    fn provider_from_config_resolves_ollama() {
        let cfg = AiConfig::default();
        let p = provider_from_config(&cfg).unwrap();
        let _ = p; // Provider trait object; can't introspect type
        // safely without downcast. Just verify it
        // resolves without error.
    }

    #[test]
    fn provider_from_config_rejects_unknown_provider() {
        let mut cfg = AiConfig::default();
        cfg.provider = "groq-but-not-openai-compatible".into();
        let err = match provider_from_config(&cfg) {
            Ok(_) => panic!("expected provider error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("unknown ai.provider"));
    }

    #[test]
    fn provider_from_config_openai_without_key_env_errors() {
        let mut cfg = AiConfig::default();
        cfg.provider = "openai".into();
        // api_key_env is None.
        let err = match provider_from_config(&cfg) {
            Ok(_) => panic!("expected provider error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("ai.api_key_env"));
    }

    #[test]
    fn provider_from_config_openai_with_missing_env_errors() {
        let mut cfg = AiConfig::default();
        cfg.provider = "openai".into();
        cfg.api_key_env = Some("DEFINITELY_NOT_SET_TEAR_TEST_KEY_zzz".into());
        let err = match provider_from_config(&cfg) {
            Ok(_) => panic!("expected provider error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("not set"));
    }

    #[test]
    fn mock_provider_returns_canned() {
        let p = MockProvider {
            canned: "ok".into(),
        };
        assert_eq!(p.generate("anything").unwrap(), "ok");
    }
}
