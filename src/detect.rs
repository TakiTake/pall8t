use crate::config::{AgentPatternsConfig, Config};
use regex::Regex;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    Working,
    Waiting,
    Idle,
    Done,
}

impl TabState {
    pub fn label(&self) -> &'static str {
        match self {
            TabState::Working => "● working",
            TabState::Waiting => "⚠ waiting",
            TabState::Idle => "○ idle",
            TabState::Done => "✓ done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Agent,
    Shell,
}

pub struct AgentPatterns {
    waiting: Vec<Regex>,
    working: Vec<Regex>,
}

const DEFAULT_WAITING: &[&str] = &[
    r"Do you want",
    r"❯ 1\. Yes",
    r"[Ww]aiting for your input",
    r"Would you like",
];
const DEFAULT_WORKING: &[&str] = &[r"[Ee]sc to interrupt"];

fn compile(patterns: &[String], fallback: &[&str]) -> Vec<Regex> {
    let source: Vec<String> = if patterns.is_empty() {
        fallback.iter().map(|s| s.to_string()).collect()
    } else {
        patterns.to_vec()
    };
    source
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
}

impl AgentPatterns {
    /// Patterns for the agent named by the first token of `agent_command`,
    /// from config `[agents.<name>]`, falling back to built-in defaults.
    pub fn from_config(cfg: &Config) -> Self {
        let agent_name = cfg
            .agent_command
            .split_whitespace()
            .next()
            .unwrap_or("claude")
            .to_string();
        let empty = AgentPatternsConfig::default();
        let pc = cfg.agents.get(&agent_name).unwrap_or(&empty);
        Self {
            waiting: compile(&pc.waiting_patterns, DEFAULT_WAITING),
            working: compile(&pc.working_patterns, DEFAULT_WORKING),
        }
    }
}

/// Classify a tab from its exit flag, recent-output age, and the bottom
/// rows of its screen. See DESIGN.md §6.
pub fn classify(
    kind: TabKind,
    exited: bool,
    since_output: Duration,
    bottom_text: &str,
    patterns: &AgentPatterns,
) -> TabState {
    if exited {
        return TabState::Done;
    }
    if kind == TabKind::Agent {
        if patterns.waiting.iter().any(|re| re.is_match(bottom_text)) {
            return TabState::Waiting;
        }
        if patterns.working.iter().any(|re| re.is_match(bottom_text)) {
            return TabState::Working;
        }
    }
    if since_output < Duration::from_secs(2) {
        TabState::Working
    } else {
        TabState::Idle
    }
}
