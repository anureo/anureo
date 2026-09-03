//! Temporary e2e test: real LLM title generation.
//! Loads providers from `~/.anureo/config.toml` and tries each one until one succeeds.

use std::sync::Arc;

use env_config::{default_model, xdg_toml};
use anureo_llm::factory::create_llm_provider;
use anureo_llm::message::Message;
use anureo_llm::LlmProvider;

fn sample_conversation() -> Vec<Message> {
    vec![
        Message::user(
            "帮我调试一下 Rust 的 borrow checker 报错，我的代码在闭包里借用了两个可变引用",
        ),
        Message::assistant(
            "这是典型的闭包捕获冲突。你需要把两个可变借用分开，或者用 Rc<RefCell> 组合。\
             如果是并发场景，考虑 Mutex 或者把状态提取到 struct 里再借用字段。",
        ),
        Message::user("我用了 RefCell 之后又出现运行时 borrow 冲突了，该怎么办？"),
    ]
}

#[tokio::test]
#[ignore = "e2e: needs real LLM providers with api_key+base_url in ~/.anureo/config.toml; run with --run-ignored only"]
async fn e2e_generate_title_real_llm() {
    let full = xdg_toml::load_full_config("anureo").expect("load ~/.anureo/config.toml");
    let conv = sample_conversation();
    let msgs = agent::agent::react::title_generator::build_title_messages(&conv);
    let fallback_model = default_model();

    let mut last_error: Option<String> = None;

    for p in &full.providers {
        let model = p.model.clone().unwrap_or_else(|| fallback_model.clone());
        let Some(api_key) = p.api_key.clone() else {
            println!("[e2e] skip provider={} (no api_key)", p.name);
            continue;
        };
        let Some(base_url) = p.base_url.clone() else {
            println!("[e2e] skip provider={} (no base_url)", p.name);
            continue;
        };

        let mut entry = anureo_llm::ModelEntry::new(&p.name, &model).with_api_key(api_key);
        entry = entry
            .with_base_url(&base_url)
            .with_provider_type(p.provider_type.as_deref().unwrap_or("openai_compat"));
        let provider: Arc<dyn LlmProvider> = match create_llm_provider(&entry) {
            Ok(p) => p,
            Err(e) => {
                println!("[e2e] skip provider={} (create failed: {e})", p.name);
                continue;
            }
        };

        let started = std::time::Instant::now();
        let client = match provider.create_client(provider.default_model()) {
            Ok(c) => c,
            Err(e) => {
                println!("[e2e] provider={} client error: {e}", p.name);
                continue;
            }
        };
        match client.invoke(&msgs).await {
            Ok(resp) => {
                let title =
                    agent::agent::react::title_generator::clamp_summary_chars(resp.content.trim());
                println!(
                    "[e2e] OK provider={} model={} ({:?}): {title:?}",
                    p.name,
                    model,
                    started.elapsed()
                );
                println!("[e2e] char count: {}", title.chars().count());
                assert!(!title.is_empty(), "title must not be empty");
                assert!(title.chars().count() <= 50, "title must be <= 50 chars");
                return;
            }
            Err(e) => {
                let msg = format!("{e}");
                last_error = Some(msg.clone());
                println!("[e2e] FAIL provider={} model={}: {msg}", p.name, model);
            }
        }
    }

    panic!(
        "all providers failed. last error: {}",
        last_error.unwrap_or_else(|| "(no provider tried)".into())
    );
}
