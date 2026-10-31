use crate::args::{ReviewArgs, ReviewCommand};
use crate::review_history::{ReviewHistory, ReviewRecord};
use crate::review_skill_cmd::build_review_react_config;
use crate::session::SessionManager;
use anureo_curator::{
    run_review, ReviewConfig as BgReviewConfig, ReviewOutcome, TokenUsageSummary,
};
use chrono::{Duration, Utc};
use std::time::Instant;

pub(crate) async fn handle_review_command(
    args: &ReviewArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match &args.command {
        ReviewCommand::Session {
            session_id,
            trigger,
        } => do_review_single(session_id, trigger, args, json).await,
        ReviewCommand::Sessions {
            recent,
            all_unreviewed,
            query,
        } => do_review_batch(recent, all_unreviewed, query, args, json).await,
        ReviewCommand::History { trigger, limit } => show_history(trigger, *limit, json),
        ReviewCommand::Show { session_id } => show_review(session_id, json),
        ReviewCommand::Pending { limit } => show_pending(*limit, json),
    }
}

async fn do_review_single(
    session_id: &str,
    trigger: &str,
    args: &ReviewArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let anureo_home = config::home::anureo_home();
    let history = ReviewHistory::new(&anureo_home);

    let mgr = SessionManager::with_default_path();
    let text = mgr
        .extract_session_text(session_id)
        .map_err(|e| format!("Failed to load session '{}': {}", session_id, e))?;

    if args.dry_run {
        if json {
            let dry = serde_json::json!({
                "session_id": session_id,
                "text_length": text.len(),
                "dry_run": true,
            });
            println!("{}", serde_json::to_string_pretty(&dry)?);
        } else {
            println!("[DRY RUN] Would review session: {}", session_id);
            println!("  Text length: {} chars", text.len());
            if args.verbose && !text.is_empty() {
                let preview: String = text.chars().take(2000).collect();
                println!("\n--- Session Content (first 2000 chars) ---\n");
                println!("{}", preview);
            }
        }
        return Ok(());
    }

    if text.chars().count() < 200 {
        let record = ReviewRecord {
            review_id: Some(uuid::Uuid::new_v4().to_string()),
            session_id: session_id.to_string(),
            reviewed_at: Utc::now(),
            trigger: trigger.to_string(),
            model: String::new(),
            memory_update_count: 0,
            skill_update_count: 0,
            skipped: true,
            skip_reason: Some("insufficient_content".to_string()),
            duration_ms: start.elapsed().as_millis() as u64,
            actions: Vec::new(),
        };
        history.append(&record)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&record)?);
        } else {
            println!(
                "Skipped: session content too short ({} chars, minimum 200)",
                text.len()
            );
        }
        return Ok(());
    }

    let model_name = args.model.as_deref().unwrap_or("(default)");

    let mut review_config = BgReviewConfig::default();
    if args.memory_only {
        review_config.review_skills = false;
    }
    if args.skills_only {
        review_config.review_memory = false;
    }

    if !json {
        eprintln!("Reviewing session: {}", session_id);
        eprintln!("  Model:    {}", model_name);
        eprintln!("  Content:  {} chars", text.len());
        eprintln!("  Trigger:  {}", trigger);
    }

    let react_config = build_review_react_config(args.model.as_deref())?;
    let checkpoint_id = session_id.to_string();

    match run_review(react_config, checkpoint_id, &text, &review_config).await {
        Ok(outcome) => {
            let record = ReviewRecord {
                review_id: Some(uuid::Uuid::new_v4().to_string()),
                session_id: session_id.to_string(),
                reviewed_at: Utc::now(),
                trigger: trigger.to_string(),
                model: String::new(),
                memory_update_count: outcome.memory_count,
                skill_update_count: outcome.skill_count,
                skipped: outcome.skipped,
                skip_reason: outcome.skip_reason.clone(),
                duration_ms: start.elapsed().as_millis() as u64,
                actions: outcome.actions.clone(),
            };
            history.append(&record)?;

            if json {
                let result = serde_json::json!({
                    "record": record,
                    "outcome": {
                        "actions": outcome.actions,
                        "summary": outcome.summary,
                        "memory_count": outcome.memory_count,
                        "skill_count": outcome.skill_count,
                        "tool_violations": outcome.tool_violations,
                        "duration_ms": outcome.duration_ms,
                        "skipped": outcome.skipped,
                        "skip_reason": outcome.skip_reason,
                        "tokens": &outcome.tokens,
                    },
                });
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_review_outcome(&outcome, args.verbose);
                eprintln!(
                    "Duration: {}",
                    format_duration_ms(start.elapsed().as_millis() as u64)
                );
            }
        }
        Err(e) => {
            let record = ReviewRecord {
                review_id: Some(uuid::Uuid::new_v4().to_string()),
                session_id: session_id.to_string(),
                reviewed_at: Utc::now(),
                trigger: trigger.to_string(),
                model: String::new(),
                memory_update_count: 0,
                skill_update_count: 0,
                skipped: true,
                skip_reason: Some(format!("llm_error: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                actions: Vec::new(),
            };
            history.append(&record)?;
            return Err(e.to_string().into());
        }
    }
    Ok(())
}

async fn do_review_batch(
    recent: &Option<String>,
    all_unreviewed: &bool,
    query: &Option<String>,
    args: &ReviewArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let anureo_home = config::home::anureo_home();
    let history = ReviewHistory::new(&anureo_home);
    let mgr = SessionManager::with_default_path();

    let sessions = if let Some(q) = query {
        mgr.search_sessions(q, 100)?
    } else if let Some(dur_str) = recent {
        let days = parse_duration_days(dur_str)?;
        let since = Utc::now() - Duration::days(days as i64);
        mgr.list_sessions_filtered(0, Some(since), None, false)?
    } else if *all_unreviewed {
        let reviewed = history.reviewed_session_ids()?;
        mgr.list_sessions_filtered(0, None, None, false)?
            .into_iter()
            .filter(|s| !reviewed.contains(&s.session_id))
            .collect()
    } else {
        return Err("Specify --recent <Nd>, --all-unreviewed, or --query <text>".into());
    };

    if sessions.is_empty() {
        if json {
            println!("{}", serde_json::json!({"sessions_found": 0}));
        } else {
            println!("No sessions to review.");
        }
        return Ok(());
    }

    if args.dry_run {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "sessions_to_review": sessions.len(),
                    "session_ids": sessions.iter().map(|s| &s.session_id).collect::<Vec<_>>(),
                    "dry_run": true,
                }))?
            );
        } else {
            println!("Found {} sessions to review (dry run):", sessions.len());
            for (i, s) in sessions.iter().enumerate() {
                let title = s.title.as_deref().unwrap_or("(untitled)");
                let time = s
                    .last_updated
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                println!(
                    "  [{}] {} | {} | {}",
                    i + 1,
                    &s.session_id[..8.min(s.session_id.len())],
                    time,
                    title
                );
            }
        }
        return Ok(());
    }

    eprintln!("Reviewing {} sessions...", sessions.len());

    let mut reviewed_count = 0usize;
    let mut skipped_count = 0usize;

    for (i, session) in sessions.iter().enumerate() {
        let short_id = &session.session_id[..8.min(session.session_id.len())];
        eprint!("  [{}/{}] {} — ", i + 1, sessions.len(), short_id);

        let single_args = ReviewArgs {
            command: ReviewCommand::Session {
                session_id: session.session_id.clone(),
                trigger: "batch".to_string(),
            },
            model: args.model.clone(),
            verbose: false,
            dry_run: false,
            memory_only: args.memory_only,
            skills_only: args.skills_only,
        };

        let single_start = Instant::now();
        match do_review_single(&session.session_id, "batch", &single_args, true).await {
            Ok(()) => {
                eprintln!(
                    "OK ({}s)",
                    single_start.elapsed().as_millis() as f64 / 1000.0
                );
                reviewed_count += 1;
            }
            Err(e) => {
                eprintln!("ERROR: {}", e);
                skipped_count += 1;
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "total": sessions.len(),
                "reviewed": reviewed_count,
                "skipped": skipped_count,
            })
        );
    } else {
        eprintln!(
            "\nSummary: {} reviewed, {} skipped",
            reviewed_count, skipped_count
        );
    }
    Ok(())
}

fn show_history(
    trigger: &Option<String>,
    limit: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let anureo_home = config::home::anureo_home();
    let history = ReviewHistory::new(&anureo_home);
    let records = history.list(limit)?;

    let filtered: Vec<_> = if let Some(t) = trigger {
        records.into_iter().filter(|r| r.trigger == *t).collect()
    } else {
        records
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
    } else if filtered.is_empty() {
        println!("No review history found.");
    } else {
        for r in &filtered {
            let status = if r.skipped { "SKIP" } else { "OK" };
            let short_id = &r.session_id[..8.min(r.session_id.len())];
            println!(
                "[{}] {} | {} | {} | mem:{} skills:{} | {}ms",
                status,
                short_id,
                r.reviewed_at.format("%Y-%m-%d %H:%M"),
                r.trigger,
                r.memory_update_count,
                r.skill_update_count,
                r.duration_ms,
            );
            if let Some(reason) = &r.skip_reason {
                println!("       reason: {}", reason);
            }
        }
    }
    Ok(())
}

fn show_review(session_id: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let anureo_home = config::home::anureo_home();
    let history = ReviewHistory::new(&anureo_home);
    match history.find_by_session(session_id)? {
        Some(record) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("Session: {}", record.session_id);
                println!(
                    "Reviewed: {}",
                    record.reviewed_at.format("%Y-%m-%d %H:%M:%S")
                );
                println!("Trigger: {}", record.trigger);
                println!("Model: {}", record.model);
                println!("Memory updates: {}", record.memory_update_count);
                println!("Skill updates: {}", record.skill_update_count);
                println!("Skipped: {}", record.skipped);
                if let Some(reason) = &record.skip_reason {
                    println!("Skip reason: {}", reason);
                }
                println!("Duration: {}ms", record.duration_ms);
            }
        }
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"session_id": session_id, "status": "pending"})
                );
            } else {
                println!("Session: {}", session_id);
                println!("Status: pending (no review record yet)");
            }
        }
    }
    Ok(())
}

fn show_pending(limit: usize, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let anureo_home = config::home::anureo_home();
    let history = ReviewHistory::new(&anureo_home);
    let mgr = SessionManager::with_default_path();

    let reviewed = history.reviewed_session_ids()?;
    let all = mgr.list_sessions_filtered(0, None, None, false)?;
    let pending: Vec<_> = all
        .into_iter()
        .filter(|s| !reviewed.contains(&s.session_id))
        .take(limit)
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
    } else if pending.is_empty() {
        println!("All sessions have been reviewed.");
    } else {
        println!("{} pending sessions:", pending.len());
        for s in &pending {
            let short_id = &s.session_id[..8.min(s.session_id.len())];
            let time = s
                .last_updated
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            println!("  {} | {} | steps:{}", short_id, time, s.latest_step);
        }
    }
    Ok(())
}

fn parse_duration_days(s: &str) -> Result<usize, String> {
    let s = s.trim().to_lowercase();
    if let Some(num) = s.strip_suffix('d') {
        num.parse::<usize>()
            .map_err(|e| format!("Invalid duration '{}': {}", s, e))
    } else {
        s.parse::<usize>()
            .map_err(|_| format!("Invalid duration '{}': expected 'Nd' format (e.g. '7d')", s))
    }
}

fn print_review_outcome(outcome: &ReviewOutcome, verbose: bool) {
    if outcome.skipped {
        println!(
            "  Skipped: {}",
            outcome.skip_reason.as_deref().unwrap_or("unknown")
        );
        return;
    }

    if outcome.actions.is_empty() {
        println!("  No actions taken.");
    } else {
        for action in &outcome.actions {
            let status = if action.succeeded { "OK" } else { "FAIL" };
            let detail = if action.summary.is_empty() {
                String::new()
            } else {
                format!(" — {}", action.summary)
            };
            println!(
                "  [{}] {} ({}){}",
                status, action.target, action.kind, detail
            );
            if verbose && !action.summary.is_empty() {
                // Verbose already shows the summary inline above; reserve this slot
                // for future richer context (e.g. raw tool result, provenance).
            }
        }
    }

    if !outcome.tool_violations.is_empty() {
        println!("  Violations: {}", outcome.tool_violations.len());
        for v in &outcome.tool_violations {
            println!("    - {}", v);
        }
    }

    if !outcome.tokens.is_empty() {
        println!("  Tokens: {}", format_token_summary(&outcome.tokens));
    }
}

fn format_token_summary(t: &TokenUsageSummary) -> String {
    // Lays out cached vs non-cached split so a single glance answers
    // "did the cache help?" — e.g. "1200 in (800 cached, 400 fresh) + 100 out = 1300 total (1 LLM call)".
    // When the provider does not report cache hits (cached_tokens == 0) we drop
    // the parenthetical split so the line is not noisy.
    let call_word = if t.llm_calls == 1 { "call" } else { "calls" };
    if t.cached_tokens == 0 {
        format!(
            "{} in + {} out = {} total ({} LLM {})",
            t.prompt_tokens, t.completion_tokens, t.total_tokens, t.llm_calls, call_word
        )
    } else {
        format!(
            "{} in ({} cached, {} fresh) + {} out = {} total ({} LLM {})",
            t.prompt_tokens,
            t.cached_tokens,
            t.non_cached_prompt(),
            t.completion_tokens,
            t.total_tokens,
            t.llm_calls,
            call_word
        )
    }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anureo_curator::TokenUsageSummary;

    #[test]
    fn format_token_summary_omits_cache_split_when_zero() {
        // When no cache hit was reported we don't want to print "(0 cached, N fresh)"
        // — it's noise. Verify the line stays clean.
        let t = TokenUsageSummary {
            llm_calls: 1,
            prompt_tokens: 500,
            completion_tokens: 50,
            total_tokens: 550,
            ..Default::default()
        };
        let s = format_token_summary(&t);
        assert!(s.contains("500 in"), "got: {s}");
        assert!(s.contains("50 out"), "got: {s}");
        assert!(s.contains("550 total"), "got: {s}");
        assert!(s.contains("1 LLM call"), "got: {s}");
        assert!(
            !s.contains("cached"),
            "should not mention cache when none: {s}"
        );
    }

    #[test]
    fn format_token_summary_shows_cache_split_when_present() {
        // The whole point of the feature: when the provider reports a cache
        // hit, the CLI must surface it as "X cached, Y fresh" so the user
        // can see whether prefix caching is actually saving them money.
        let t = TokenUsageSummary {
            llm_calls: 2,
            prompt_tokens: 3_500,
            cached_tokens: 2_000,
            completion_tokens: 180,
            total_tokens: 3_680,
        };
        let s = format_token_summary(&t);
        assert!(s.contains("3500 in"), "got: {s}");
        assert!(s.contains("(2000 cached, 1500 fresh)"), "got: {s}");
        assert!(s.contains("180 out"), "got: {s}");
        assert!(s.contains("3680 total"), "got: {s}");
        assert!(s.contains("2 LLM calls"), "got: {s}");
    }

    #[test]
    fn format_token_summary_singular_vs_plural_calls() {
        // 1 LLM call vs N LLM calls — simple grammar check, but it's user-facing.
        let one = TokenUsageSummary {
            llm_calls: 1,
            prompt_tokens: 10,
            completion_tokens: 1,
            total_tokens: 11,
            ..Default::default()
        };
        assert!(format_token_summary(&one).contains("1 LLM call"));
        assert!(!format_token_summary(&one).contains("1 LLM calls"));

        let many = TokenUsageSummary {
            llm_calls: 3,
            prompt_tokens: 30,
            completion_tokens: 3,
            total_tokens: 33,
            ..Default::default()
        };
        assert!(format_token_summary(&many).contains("3 LLM calls"));
    }

    #[test]
    fn format_token_summary_with_only_cached_prompt_saturates_fresh_at_zero() {
        // Edge case: provider reports 100% cache hit. The "fresh" portion
        // should be 0, not negative. (non_cached_prompt saturates in
        // TokenUsageSummary; we just verify the formatting doesn't break.)
        let t = TokenUsageSummary {
            llm_calls: 1,
            prompt_tokens: 1_000,
            cached_tokens: 1_000,
            completion_tokens: 50,
            total_tokens: 1_050,
        };
        let s = format_token_summary(&t);
        assert!(s.contains("(1000 cached, 0 fresh)"), "got: {s}");
    }
}
