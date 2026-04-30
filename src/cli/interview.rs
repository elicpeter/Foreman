//! Design interview mode for `pitboss plan --interview`.
//!
//! Dispatches the configured agent once to generate a numbered list of design
//! questions, then walks the user through them interactively. The compiled Q&A
//! is returned as a formatted spec string that the caller embeds in the planner
//! prompt to produce a more precise plan.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config::Config;
use crate::prompts;
use crate::style::{self, col};

/// Wall-clock cap for the question-generation dispatch. Question generation is
/// a short text-only task; 5 minutes is generous.
const QUESTIONER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Default upper bound on the number of questions the agent may generate.
/// The agent typically produces fewer; this prevents runaway verbosity.
pub const DEFAULT_MAX_QUESTIONS: u32 = 50;

/// Run the design interview and return a formatted Q&A spec string.
///
/// 1. Dispatches the agent with a question-generation prompt.
/// 2. Parses the numbered list from the agent output.
/// 3. Asks each question interactively via stdin/stdout.
/// 4. Returns a formatted spec ready to be appended to the planner goal.
pub async fn conduct<A: Agent>(
    workspace: &Path,
    goal: &str,
    repo_summary: &str,
    cfg: &Config,
    agent: &A,
    max_questions: u32,
) -> Result<String> {
    let c = style::use_color_stderr();
    let fm = col(c, style::BOLD_CYAN, "[pitboss]");

    eprintln!(
        "{fm} {}",
        col(c, style::MAGENTA, "generating design questions...")
    );

    std::fs::create_dir_all(workspace.join(".pitboss/logs"))
        .context("interview: creating logs dir")?;

    let prompt = prompts::questioner(goal, repo_summary, max_questions);
    let request = AgentRequest {
        role: Role::Planner,
        model: cfg.models.planner.clone(),
        system_prompt: prompts::caveman::system_prompt(&cfg.caveman),
        user_prompt: prompt,
        workdir: workspace.to_path_buf(),
        log_path: workspace.join(".pitboss/logs/questioner.log"),
        timeout: QUESTIONER_TIMEOUT,
    };

    let raw = dispatch_questioner(agent, request)
        .await
        .context("interview: question generation failed")?;

    let questions = parse_questions(&raw);
    if questions.is_empty() {
        bail!("interview: agent produced no parseable questions");
    }

    eprintln!(
        "{fm} {} {} {}",
        col(c, style::BOLD_GREEN, "interview:"),
        questions.len(),
        col(
            c,
            style::DIM,
            "questions ready — press Enter to skip any question"
        )
    );

    // Walk the user through each question.
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(questions.len());

    for (i, question) in questions.iter().enumerate() {
        println!("\n[{}/{}] {question}", i + 1, questions.len());
        print!("> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF — stop early
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        let answer = line.trim().to_string();
        if !answer.is_empty() {
            pairs.push((question.clone(), answer));
        }
    }

    if pairs.is_empty() {
        bail!("interview: no answers provided — aborting");
    }

    let answered = pairs.len();
    eprintln!(
        "{fm} {} ({answered} answered)",
        col(c, style::BOLD_GREEN, "interview complete")
    );

    Ok(format_spec(&pairs))
}

/// Dispatch the agent to generate questions and return its stdout output.
async fn dispatch_questioner<A: Agent>(agent: &A, request: AgentRequest) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let cancel = CancellationToken::new();

    let collector = tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Stdout(chunk) = ev {
                buf.push_str(&chunk);
            }
        }
        buf
    });

    let outcome = agent.run(request, tx, cancel).await?;
    let body = collector.await.unwrap_or_default();

    match outcome.stop_reason {
        StopReason::Completed if outcome.exit_code == 0 => Ok(body),
        StopReason::Completed => {
            bail!("questioner agent exited with code {}", outcome.exit_code)
        }
        StopReason::Timeout => bail!("questioner agent timed out"),
        StopReason::Cancelled => bail!("questioner agent was cancelled"),
        StopReason::Error(msg) => bail!("questioner agent failed: {msg}"),
    }
}

/// Parse a numbered list from raw agent output.
/// Recognises lines of the form `1. Question text` (leading whitespace allowed).
fn parse_questions(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some((prefix, rest)) = trimmed.split_once(". ") {
            if prefix.parse::<u32>().is_ok() {
                let q = rest.trim().to_string();
                if !q.is_empty() {
                    out.push(q);
                }
            }
        }
    }
    out
}

/// Format Q&A pairs as a compact numbered spec.
fn format_spec(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (q, a)) in pairs.iter().enumerate() {
        out.push_str(&format!("Q{n}: {q}\nA{n}: {a}\n\n", n = i + 1));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_questions_extracts_numbered_lines() {
        let raw = "1. What is the output format?\n2. Should it be async?\n3. Any auth needed?\n";
        let qs = parse_questions(raw);
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0], "What is the output format?");
        assert_eq!(qs[2], "Any auth needed?");
    }

    #[test]
    fn parse_questions_skips_non_numbered_lines() {
        let raw = "Here are your questions:\n\n1. First?\n\nSome commentary.\n2. Second?\n";
        let qs = parse_questions(raw);
        assert_eq!(qs.len(), 2);
    }

    #[test]
    fn parse_questions_returns_empty_for_blank_output() {
        assert!(parse_questions("").is_empty());
        assert!(parse_questions("no numbers here at all").is_empty());
    }

    #[test]
    fn format_spec_produces_qa_pairs() {
        let pairs = vec![
            ("Output format?".into(), "JSON".into()),
            ("Async?".into(), "Yes, tokio".into()),
        ];
        let spec = format_spec(&pairs);
        assert!(spec.contains("Q1: Output format?"));
        assert!(spec.contains("A1: JSON"));
        assert!(spec.contains("Q2: Async?"));
        assert!(spec.contains("A2: Yes, tokio"));
    }
}
