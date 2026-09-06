//! Slash command parser: user text -> Option<Command>.

use crate::commands::command::{Command, GoalSubcommand};

pub fn parse(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let token = trimmed.split_whitespace().next()?;
    match token {
        "/reset" | "/clear" | "/new" => Some(Command::ResetContext),
        "/compact" => {
            let instructions = trimmed
                .strip_prefix("/compact")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(Command::Compact { instructions })
        }
        "/summarize" => Some(Command::Summarize),
        "/models" => {
            let rest = trimmed
                .strip_prefix("/models")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            match rest {
                Some(q) if q.starts_with("use ") => Some(Command::ModelsUse {
                    model_id: q[4..].trim().to_string(),
                }),
                Some(q) => Some(Command::Models {
                    query: Some(q.to_string()),
                }),
                None => Some(Command::Models { query: None }),
            }
        }
        "/model" => {
            let id = trimmed
                .strip_prefix("/model")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(Command::Model { model_id: id })
        }
        "/goal" => {
            let rest = trimmed
                .strip_prefix("/goal")
                .map(|s| s.trim())
                .unwrap_or("");
            if rest.is_empty() {
                // Bare `/goal` reads as "show me the current goal".
                Some(Command::Goal {
                    subcommand: GoalSubcommand::Show,
                })
            } else {
                let (head, tail) = match rest.split_once(' ') {
                    Some((head, tail)) => (head, tail.trim()),
                    None => (rest, ""),
                };
                match head {
                    "set" | "edit" => {
                        if tail.is_empty() {
                            // `/goal set` / `/goal edit` without a description
                            // is malformed; drop to normal message handling.
                            None
                        } else {
                            let subcommand = if head == "set" {
                                GoalSubcommand::Set {
                                    description: tail.to_string(),
                                }
                            } else {
                                GoalSubcommand::Edit {
                                    description: tail.to_string(),
                                }
                            };
                            Some(Command::Goal { subcommand })
                        }
                    }
                    "show" => Some(Command::Goal {
                        subcommand: GoalSubcommand::Show,
                    }),
                    "pause" => Some(Command::Goal {
                        subcommand: GoalSubcommand::Pause,
                    }),
                    "resume" => Some(Command::Goal {
                        subcommand: GoalSubcommand::Resume,
                    }),
                    "clear" => Some(Command::Goal {
                        subcommand: GoalSubcommand::Clear,
                    }),
                    // Compatibility: bare `/goal <description>` still arms a goal.
                    _ => Some(Command::Goal {
                        subcommand: GoalSubcommand::Set {
                            description: rest.to_string(),
                        },
                    }),
                }
            }
        }
        "/review-skill" | "/review-skills" | "/rs" => {
            let scope = trimmed
                .strip_prefix("/review-skill")
                .or_else(|| trimmed.strip_prefix("/review-skills"))
                .or_else(|| trimmed.strip_prefix("/rs"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(Command::ReviewSkill { scope })
        }
        // Priority #18 (Hermes parity, `cli.py`): high-value expansion.
        // Each parser branch here is a one-liner because the heavy
        // lifting lives in the executor; we only need to split the
        // argument string.
        "/help" | "/?" => {
            let arg = trimmed
                .strip_prefix("/help")
                .or_else(|| trimmed.strip_prefix("/?"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(Command::Help { command: arg })
        }
        "/tools" => Some(Command::Tools),
        "/resume" => {
            let sel = trimmed
                .strip_prefix("/resume")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(Command::Resume { selector: sel })
        }
        "/undo" => Some(Command::Undo),
        "/retry" => Some(Command::Retry),
        "/history" => {
            let count = trimmed
                .strip_prefix("/history")
                .map(|s| s.trim())
                .and_then(|s| s.parse::<usize>().ok());
            Some(Command::History { count })
        }
        "/exit" | "/quit" => Some(Command::Exit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reset_aliases() {
        assert_eq!(parse("/reset"), Some(Command::ResetContext));
        assert_eq!(parse("/clear"), Some(Command::ResetContext));
        assert_eq!(parse("/new"), Some(Command::ResetContext));
        assert_eq!(parse("  /reset  "), Some(Command::ResetContext));
    }

    #[test]
    fn parse_compact_with_and_without_instructions() {
        assert_eq!(
            parse("/compact"),
            Some(Command::Compact { instructions: None })
        );
        assert_eq!(
            parse("/compact focus on auth module"),
            Some(Command::Compact {
                instructions: Some("focus on auth module".into())
            })
        );
    }

    #[test]
    fn parse_summarize() {
        assert_eq!(parse("/summarize"), Some(Command::Summarize));
    }

    #[test]
    fn parse_models_variants() {
        assert_eq!(parse("/models"), Some(Command::Models { query: None }));
        assert_eq!(
            parse("/models gpt"),
            Some(Command::Models {
                query: Some("gpt".into())
            })
        );
        assert_eq!(
            parse("/models use gpt-4o"),
            Some(Command::ModelsUse {
                model_id: "gpt-4o".into()
            })
        );
    }

    #[test]
    fn parse_goal_subcommands() {
        assert_eq!(
            parse("/goal set fix the login bug"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Set {
                    description: "fix the login bug".into()
                }
            })
        );
        assert_eq!(
            parse("/goal edit  migrate to Pydantic v2  "),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Edit {
                    description: "migrate to Pydantic v2".into()
                }
            })
        );
        assert_eq!(
            parse("/goal show"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Show
            })
        );
        assert_eq!(
            parse("/goal pause"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Pause
            })
        );
        assert_eq!(
            parse("/goal resume"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Resume
            })
        );
        assert_eq!(
            parse("/goal clear"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Clear
            })
        );
    }

    #[test]
    fn parse_goal_bare_compatibility() {
        // Bare `/goal` → Show (previously returned None).
        assert_eq!(
            parse("/goal"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Show
            })
        );
        assert_eq!(
            parse("  /goal   "),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Show
            })
        );
        // Bare `/goal <description>` (no subcommand keyword) → Set.
        assert_eq!(
            parse("/goal fix the login bug"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Set {
                    description: "fix the login bug".into()
                }
            })
        );
        // A description that merely starts with a keyword-like word stays Set.
        assert_eq!(
            parse("/goal settle the migration"),
            Some(Command::Goal {
                subcommand: GoalSubcommand::Set {
                    description: "settle the migration".into()
                }
            })
        );
    }

    #[test]
    fn parse_goal_set_edit_require_description() {
        assert_eq!(parse("/goal set"), None);
        assert_eq!(parse("/goal edit"), None);
        assert_eq!(parse("/goal set   "), None);
    }

    #[test]
    fn parse_non_command_returns_none() {
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse("/unknown"), None);
        assert_eq!(parse(""), None);
    }
}
