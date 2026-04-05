use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentPhase {
    Explore,
    Investigate,
    Execute,
    Summarize,
    Conclude,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: String,
    pub text: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceCard {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub source_ref: String,
    pub key_fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMemory {
    pub tool_name: String,
    pub last_query: String,
    pub last_result_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextState {
    pub goal: String,
    pub phase: AgentPhase,
    pub current_focus: String,
    pub current_plan: Vec<String>,
    pub facts: Vec<Fact>,
    pub hypotheses: Vec<Hypothesis>,
    pub evidence_cards: Vec<EvidenceCard>,
    pub tool_memory: Vec<ToolMemory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub ts: String,
    pub kind: String,
    pub detail: String,
}

pub struct ContextManager {
    pub state: AgentContextState,
    recent_events: VecDeque<AgentEvent>,
    max_recent_events: usize,
    next_fact_id: usize,
    next_hypothesis_id: usize,
    next_evidence_id: usize,
}

impl ContextManager {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            state: AgentContextState {
                goal: goal.into(),
                phase: AgentPhase::Explore,
                current_focus: "Understand task and avoid repeated probing.".into(),
                current_plan: vec![],
                facts: vec![],
                hypotheses: vec![],
                evidence_cards: vec![],
                tool_memory: vec![],
            },
            recent_events: VecDeque::new(),
            max_recent_events: 16,
            next_fact_id: 1,
            next_hypothesis_id: 1,
            next_evidence_id: 1,
        }
    }

    pub fn set_phase(&mut self, phase: AgentPhase) {
        self.state.phase = phase;
    }

    pub fn set_focus(&mut self, focus: impl Into<String>) {
        self.state.current_focus = focus.into();
    }

    pub fn upsert_plan(&mut self, plan: Vec<String>) {
        self.state.current_plan = plan;
    }

    pub fn ingest_user_message(&mut self, text: &str) {
        self.push_event("UserMessage", truncate_chars(text, 320));
        if self.state.goal.trim().is_empty() {
            self.state.goal = truncate_chars(text, 220);
        }
    }

    pub fn ingest_tool_call(&mut self, tool_name: &str, args_json: &str) {
        let query = truncate_chars(args_json, 220);
        self.push_event("ToolCall", format!("{tool_name}: {query}"));

        if let Some(slot) = self
            .state
            .tool_memory
            .iter_mut()
            .find(|m| m.tool_name == tool_name)
        {
            slot.last_query = query;
            return;
        }
        self.state.tool_memory.push(ToolMemory {
            tool_name: tool_name.to_string(),
            last_query: query,
            last_result_summary: String::new(),
        });
    }

    pub fn ingest_tool_result(&mut self, tool_name: &str, raw_content: &str) {
        let mut fields = BTreeMap::new();
        fields.insert("chars".into(), raw_content.len().to_string());
        fields.insert("lines".into(), raw_content.lines().count().to_string());
        if raw_content.contains("Output truncated") {
            fields.insert("truncated".into(), "true".into());
        }
        let summary = summarize_tool_result_for_context(tool_name, raw_content, &fields);
        let evidence_id = format!("ev_{}", self.next_evidence_id);
        self.next_evidence_id += 1;

        self.state.evidence_cards.push(EvidenceCard {
            id: evidence_id.clone(),
            title: format!("Tool Result: {tool_name}"),
            summary: summary.clone(),
            source_ref: tool_name.to_string(),
            key_fields: fields,
        });
        if self.state.evidence_cards.len() > 32 {
            self.state.evidence_cards.remove(0);
        }

        if let Some(slot) = self
            .state
            .tool_memory
            .iter_mut()
            .find(|m| m.tool_name == tool_name)
        {
            slot.last_result_summary = summary.clone();
        }

        self.push_event(
            "ToolResult",
            format!("{tool_name}: {}", truncate_chars(&summary, 220)),
        );

        if raw_content.starts_with("[DENIED]") {
            self.add_hypothesis(
                "Current approach is blocked by permission or policy.",
                "Testing",
            );
        }
    }

    pub fn add_fact(&mut self, text: &str) {
        let id = format!("fact_{}", self.next_fact_id);
        self.next_fact_id += 1;
        self.state.facts.push(Fact {
            id,
            text: truncate_chars(text, 220),
        });
        if self.state.facts.len() > 24 {
            self.state.facts.remove(0);
        }
    }

    pub fn add_hypothesis(&mut self, text: &str, status: &str) {
        let id = format!("hyp_{}", self.next_hypothesis_id);
        self.next_hypothesis_id += 1;
        self.state.hypotheses.push(Hypothesis {
            id,
            text: truncate_chars(text, 220),
            status: status.to_string(),
        });
        if self.state.hypotheses.len() > 16 {
            self.state.hypotheses.remove(0);
        }
    }

    pub fn render_prompt_context(&self, max_chars: usize) -> String {
        /// Snapshot 中 Goal 展示上限（完整 goal 存于 state，渲染时允许更长以免首轮信息不足）。
        const SNAPSHOT_GOAL_MAX_CHARS: usize = 2000;
        let mut out = String::new();
        out.push_str("[STATE SNAPSHOT]\n");
        out.push_str(&format!(
            "Goal: {}\n",
            truncate_chars(&self.state.goal, SNAPSHOT_GOAL_MAX_CHARS)
        ));
        out.push_str(&format!("Phase: {:?}\n", self.state.phase));
        out.push_str(&format!(
            "Current Focus: {}\n",
            truncate_chars(&self.state.current_focus, 500)
        ));

        if !self.state.current_plan.is_empty() {
            out.push_str("Plan:\n");
            for p in self.state.current_plan.iter().take(5) {
                out.push_str(&format!("- {}\n", truncate_chars(p, 140)));
            }
        }

        out.push_str("\nConfirmed Facts:\n");
        for f in self.state.facts.iter().rev().take(8).rev() {
            out.push_str(&format!("- [{}] {}\n", f.id, f.text));
        }

        out.push_str("\nActive Hypotheses:\n");
        for h in self.state.hypotheses.iter().rev().take(6).rev() {
            out.push_str(&format!("- [{}] {} ({})\n", h.id, h.text, h.status));
        }

        out.push_str("\nKey Evidence:\n");
        for ev in self.state.evidence_cards.iter().rev().take(8).rev() {
            out.push_str(&format!(
                "- [{}] {}\n",
                ev.id,
                truncate_chars(&ev.summary, 200)
            ));
        }

        out.push_str("\nTool Memory:\n");
        for m in self.state.tool_memory.iter().rev().take(4).rev() {
            out.push_str(&format!(
                "- {} | last_query={} | last_result={}\n",
                m.tool_name,
                truncate_chars(&m.last_query, 100),
                truncate_chars(&m.last_result_summary, 100)
            ));
        }

        out.push_str("\nRecent Events:\n");
        for e in self.recent_events.iter().rev().take(6).rev() {
            out.push_str(&format!(
                "- {} | {}\n",
                e.kind,
                truncate_chars(&e.detail, 120)
            ));
        }

        out.push_str("\nRules:\n");
        out.push_str("- Treat facts and hypotheses separately.\n");
        out.push_str("- Reuse existing evidence before calling tools again.\n");
        out.push_str("- Avoid repeated exec with similar purpose.\n");

        truncate_at_line_boundary(&out, max_chars)
    }

    fn push_event(&mut self, kind: &str, detail: impl Into<String>) {
        self.recent_events.push_back(AgentEvent {
            ts: chrono::Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            detail: detail.into(),
        });
        while self.recent_events.len() > self.max_recent_events {
            self.recent_events.pop_front();
        }
    }
}

fn summarize_tool_result_for_context(
    tool_name: &str,
    raw_content: &str,
    fields: &BTreeMap<String, String>,
) -> String {
    let mut key = Vec::new();
    if let Some(chars) = fields.get("chars") {
        key.push(format!("chars={chars}"));
    }
    if let Some(lines) = fields.get("lines") {
        key.push(format!("lines={lines}"));
    }
    if let Some(truncated) = fields.get("truncated") {
        key.push(format!("truncated={truncated}"));
    }
    let first_line = raw_content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(empty)")
        .trim();
    format!(
        "{} [{}] {}",
        tool_name,
        key.join(", "),
        truncate_chars(first_line, 180).replace('\n', " ")
    )
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    match input.char_indices().nth(max_chars) {
        Some((idx, _)) => input[..idx].to_string(),
        None => input.to_string(),
    }
}

/// 在行边界截断：回退到 max_chars 范围内最后一个 '\n'。
/// 避免在行中间截断导致 LLM 误以为需要续写未完成的文本。
fn truncate_at_line_boundary(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    // 如果不需要截断，原样返回
    let byte_limit = match input.char_indices().nth(max_chars) {
        Some((idx, _)) => idx,
        None => return input.to_string(),
    };
    // 找 byte_limit 范围内最后一个换行符
    match input[..byte_limit].rfind('\n') {
        Some(nl_pos) => input[..=nl_pos].to_string(),
        // 如果连一个完整行都没有，退化为字符截断
        None => input[..byte_limit].to_string(),
    }
}
