直接给你一个 Rust 版可落地的 agent context state 设计。
目标就是：不要把所有信息揉成一段大摘要，而是在程序里维护一个结构化状态，然后每轮渲染成 prompt。

我先给你一个适合你这种 agent loop / 业务排查 / 多轮工具调用 的最小可用版本。

一、核心设计思路

分三层：

Event：原始事件流
用户消息、assistant 输出、tool call、tool result、系统状态变化

State：结构化上下文状态
facts / hypotheses / evidence / working memory / timeline

Prompt View：给大模型看的文本视图
从 state 渲染，不直接拿 event 硬拼

也就是：

events 是真相源，state 是工作记忆，prompt 是投影视图

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextState {
    pub mission: Mission,
    pub working_memory: WorkingMemory,

    pub facts: Vec<Fact>,
    pub hypotheses: Vec<Hypothesis>,
    pub evidence_cards: Vec<EvidenceCard>,
    pub timeline: Vec<TimelineEvent>,
    pub open_questions: Vec<OpenQuestion>,
    pub recent_actions: Vec<ActionRecord>,

    // 原始事件可只保存在外部存储，这里也可以放轻量索引
    pub event_refs: Vec<EventRef>,

    pub meta: ContextMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub goal: String,
    pub phase: AgentPhase,
    pub constraints: Vec<String>,
    pub success_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentPhase {
    Explore,
    Investigate,
    Execute,
    Summarize,
    Conclude,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkingMemory {
    pub current_focus: String,
    pub current_plan: Vec<String>,
    pub blockers: Vec<String>,
    pub next_best_actions: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub text: String,
    pub source: FactSource,
    pub confidence: Confidence,
    pub evidence_ids: Vec<String>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FactSource {
    User,
    Tool,
    System,
    Derived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: String,
    pub text: String,
    pub status: HypothesisStatus,
    pub supporting_evidence_ids: Vec<String>,
    pub rejecting_evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HypothesisStatus {
    New,
    Testing,
    Supported,
    Rejected,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceCard {
    pub id: String,
    pub kind: EvidenceKind,
    pub title: String,
    pub summary: String,
    pub source_ref: String,
    pub key_fields: BTreeMap<String, String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvidenceKind {
    UserMessage,
    ToolResult,
    Log,
    Metric,
    DeployRecord,
    Document,
    Config,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub ts: String,
    pub label: String,
    pub detail: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub id: String,
    pub text: String,
    pub priority: Priority,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Priority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub action: String,
    pub status: ActionStatus,
    pub result_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionStatus {
    Planned,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRef {
    pub event_id: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    SystemNote,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextMeta {
    pub version: u32,
    pub total_turns: u32,
    pub last_updated_at: Option<String>,
}

原始事件结构

你最好单独维护 event log。
不要只保留摘要，不然后面没法追溯。

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: String,
    pub ts: String,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCall {
        tool_name: String,
        args_json: String,
    },
    ToolResult {
        tool_name: String,
        content: String,
        structured: Option<BTreeMap<String, String>>,
    },
    SystemNote {
        text: String,
    },
}

你这个场景里，最重要的是把几类东西分开：

1. 事实 facts

已经确认的内容。
不能和推测混。

2. 假设 hypotheses

模型的判断、排查方向。
必须能被支持或否定。

3. 证据 evidence_cards

不是全文，而是可引用的证据卡片。

4. working_memory

只保留当前轮最重要的东西，不能无限膨胀。

5. timeline

做线上排查时特别有用。

五、给你一个最小的 Context Manager

这个 manager 负责：

接收事件

更新 state

渲染 prompt

pub struct ContextManager {
    pub state: AgentContextState,
    pub recent_events: VecDeque<AgentEvent>,
    pub max_recent_events: usize,
}

impl ContextManager {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            state: AgentContextState {
                mission: Mission {
                    goal: goal.into(),
                    phase: AgentPhase::Explore,
                    constraints: vec![],
                    success_criteria: vec![],
                },
                working_memory: WorkingMemory::default(),
                facts: vec![],
                hypotheses: vec![],
                evidence_cards: vec![],
                timeline: vec![],
                open_questions: vec![],
                recent_actions: vec![],
                event_refs: vec![],
                meta: ContextMeta {
                    version: 1,
                    total_turns: 0,
                    last_updated_at: None,
                },
            },
            recent_events: VecDeque::new(),
            max_recent_events: 50,
        }
    }

    pub fn ingest_event(&mut self, event: AgentEvent) {
        self.state.event_refs.push(EventRef {
            event_id: event.id.clone(),
            kind: match &event.payload {
                EventPayload::UserMessage { .. } => EventKind::UserMessage,
                EventPayload::AssistantMessage { .. } => EventKind::AssistantMessage,
                EventPayload::ToolCall { .. } => EventKind::ToolCall,
                EventPayload::ToolResult { .. } => EventKind::ToolResult,
                EventPayload::SystemNote { .. } => EventKind::SystemNote,
            },
        });

        self.recent_events.push_back(event);
        while self.recent_events.len() > self.max_recent_events {
            self.recent_events.pop_front();
        }
    }

    pub fn add_fact(&mut self, fact: Fact) {
        self.state.facts.push(fact);
    }

    pub fn add_hypothesis(&mut self, hyp: Hypothesis) {
        self.state.hypotheses.push(hyp);
    }

    pub fn add_evidence(&mut self, ev: EvidenceCard) {
        self.state.evidence_cards.push(ev);
    }

    pub fn add_timeline_event(&mut self, item: TimelineEvent) {
        self.state.timeline.push(item);
    }

    pub fn set_working_focus(&mut self, focus: impl Into<String>) {
        self.state.working_memory.current_focus = focus.into();
    }

    pub fn set_phase(&mut self, phase: AgentPhase) {
        self.state.mission.phase = phase;
    }

    pub fn render_prompt_context(&self) -> String {
        let mut out = String::new();

        out.push_str("You are operating inside an agent loop.\n\n");

        out.push_str("[MISSION]\n");
        out.push_str(&format!("Goal: {}\n", self.state.mission.goal));
        out.push_str(&format!("Phase: {:?}\n", self.state.mission.phase));
        if !self.state.mission.constraints.is_empty() {
            out.push_str("Constraints:\n");
            for c in &self.state.mission.constraints {
                out.push_str(&format!("- {}\n", c));
            }
        }
        if !self.state.mission.success_criteria.is_empty() {
            out.push_str("Success Criteria:\n");
            for s in &self.state.mission.success_criteria {
                out.push_str(&format!("- {}\n", s));
            }
        }

        out.push_str("\n[WORKING MEMORY]\n");
        out.push_str(&format!(
            "Current Focus: {}\n",
            self.state.working_memory.current_focus
        ));
        if !self.state.working_memory.current_plan.is_empty() {
            out.push_str("Current Plan:\n");
            for p in &self.state.working_memory.current_plan {
                out.push_str(&format!("- {}\n", p));
            }
        }
        if !self.state.working_memory.blockers.is_empty() {
            out.push_str("Blockers:\n");
            for b in &self.state.working_memory.blockers {
                out.push_str(&format!("- {}\n", b));
            }
        }

        out.push_str("\n[CONFIRMED FACTS]\n");
        for fact in self.state.facts.iter().filter(|f| f.active).take(20) {
            out.push_str(&format!(
                "- [{}] {} (source={:?}, confidence={:?})\n",
                fact.id, fact.text, fact.source, fact.confidence
            ));
        }

        out.push_str("\n[ACTIVE HYPOTHESES]\n");
        for hyp in self.state.hypotheses.iter().filter(|h| {
            matches!(
                h.status,
                HypothesisStatus::New | HypothesisStatus::Testing | HypothesisStatus::Supported
            )
        }).take(10) {
            out.push_str(&format!("- [{}] {} ({:?})\n", hyp.id, hyp.text, hyp.status));
        }

        out.push_str("\n[KEY EVIDENCE]\n");
        for ev in self.state.evidence_cards.iter().rev().take(12).rev() {
            out.push_str(&format!(
                "- [{}] {}: {} | source_ref={}\n",
                ev.id, ev.title, ev.summary, ev.source_ref
            ));
        }

        out.push_str("\n[TIMELINE]\n");
        for item in self.state.timeline.iter().rev().take(10).rev() {
            out.push_str(&format!(
                "- {} | {} | {}\n",
                item.ts, item.label, item.detail
            ));
        }

        out.push_str("\n[OPEN QUESTIONS]\n");
        for q in self.state.open_questions.iter().take(10) {
            out.push_str(&format!("- [{}] {} ({:?})\n", q.id, q.text, q.priority));
        }

        out.push_str("\n[RECENT EVENTS]\n");
        for event in self.recent_events.iter().rev().take(8).rev() {
            out.push_str(&format!("- {} | {:?}\n", event.ts, event.payload_kind()));
        }

        out.push_str("\n[RESPONSE INSTRUCTIONS]\n");
        out.push_str("- Distinguish confirmed facts from hypotheses.\n");
        out.push_str("- Prefer referring to evidence IDs when possible.\n");
        out.push_str("- Propose the next best action.\n");
        out.push_str("- Do not repeat already completed steps unless needed.\n");

        out
    }
}

impl AgentEvent {
    pub fn payload_kind(&self) -> &'static str {
        match &self.payload {
            EventPayload::UserMessage { .. } => "UserMessage",
            EventPayload::AssistantMessage { .. } => "AssistantMessage",
            EventPayload::ToolCall { .. } => "ToolCall",
            EventPayload::ToolResult { .. } => "ToolResult",
            EventPayload::SystemNote { .. } => "SystemNote",
        }
    }
}
六、怎么把“工具结果”变成 evidence card

这里是关键。
不要把工具原始输出直接全塞给模型，要先提取。

例如：

pub fn tool_result_to_evidence(
    id: impl Into<String>,
    tool_name: impl Into<String>,
    raw_content: impl Into<String>,
    fields: BTreeMap<String, String>,
    source_ref: impl Into<String>,
) -> EvidenceCard {
    let tool_name = tool_name.into();
    let raw_content = raw_content.into();

    let summary = summarize_tool_result_for_context(&tool_name, &raw_content, &fields);

    EvidenceCard {
        id: id.into(),
        kind: EvidenceKind::ToolResult,
        title: format!("Tool Result: {}", tool_name),
        summary,
        source_ref: source_ref.into(),
        key_fields: fields,
        timestamp: None,
    }
}

fn summarize_tool_result_for_context(
    tool_name: &str,
    raw_content: &str,
    fields: &BTreeMap<String, String>,
) -> String {
    if !fields.is_empty() {
        let pairs: Vec<String> = fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        return format!("{tool_name}: {}", pairs.join(", "));
    }

    let short = raw_content.chars().take(200).collect::<String>();
    format!("{tool_name}: {}", short.replace('\n', " "))
}
七、主循环里怎么用

你 agent loop 每轮可以这样：

pub fn run_one_iteration(ctx: &mut ContextManager, user_input: &str) {
    // 1. 记录用户事件
    ctx.ingest_event(AgentEvent {
        id: "evt_user_001".into(),
        ts: "2026-03-16T16:00:00+08:00".into(),
        payload: EventPayload::UserMessage {
            text: user_input.to_string(),
        },
    });

    // 2. 根据用户输入更新 mission / working memory
    ctx.set_working_focus("整理系统、对话、工具结果的上下文摘要");
    ctx.state.working_memory.current_plan = vec![
        "Define structured state".into(),
        "Classify facts/hypotheses/evidence".into(),
        "Render prompt view".into(),
    ];

    // 3. 渲染 prompt
    let prompt = ctx.render_prompt_context();

    // 4. 调模型（这里省略）
    println!("=== PROMPT ===\n{}", prompt);

    // 5. 把模型输出再回写成事件、事实、假设等
}
八、最重要的工程原则
1. facts 和 hypotheses 必须分开

不要让模型一句推测，下一轮就混进“已知事实”。

2. recent_events 只保留短窗口

最近几轮原始上下文保留，远历史靠 state 承载。

3. evidence 要有 ID

这样模型能引用 ev_12、ev_18，你也方便审计。

4. 摘要是增量更新，不是每轮全量重算

每次新事件来了，只更新一部分 state。

九、适合你现在的扩展方向

你前面明显不是纯 coding agent，更偏：

业务问题处理

排查线上问题

工具编排

case 调查

那我建议你在这个基础上再加 3 个字段：

Impact
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Impact {
    pub severity: Option<String>,
    pub affected_services: Vec<String>,
    pub affected_tenants: Vec<String>,
    pub affected_regions: Vec<String>,
}
Investigation Status
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvestigationStatus {
    pub current_owner: Option<String>,
    pub status: String,
    pub last_decision: Option<String>,
}
Tool Memory

记录某个工具最近查过什么，避免重复查。

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMemory {
    pub tool_name: String,
    pub last_query: String,
    pub last_result_summary: String,
}
十、给你一个更实用的 prompt 渲染思路

不要直接 serde_json 全塞给模型。
模型更吃这种人类可读模板。

你最终发给模型的内容应该像：

[MISSION]
Goal: Diagnose an agent-loop context summarization design
Phase: Investigate

[WORKING MEMORY]
Current Focus: Structure all system, conversation, and tool information into state
Current Plan:
- Separate facts from hypotheses
- Turn tool results into evidence cards
- Render a stable prompt context

[CONFIRMED FACTS]
- [fact_1] User is building an agent loop
- [fact_2] The use case is business troubleshooting rather than pure coding

[ACTIVE HYPOTHESES]
- [hyp_1] A layered state model is better than a single summary blob (Supported)

[KEY EVIDENCE]
- [ev_1] User conversation: repeatedly emphasizes online troubleshooting context
- [ev_2] Tool result: relay stream was cut after 60s idle gap

[OPEN QUESTIONS]
- [q_1] What Rust data model should be used?
- [q_2] How should tool results be compressed without losing traceability?
十一、最小依赖

Cargo.toml：

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

如果你要时间戳：

chrono = { version = "0.4", features = ["serde"] }
十二、我建议你先这样落地

第一版别做太复杂，先实现这几个函数：

ingest_event

add_fact

add_hypothesis

add_evidence

render_prompt_context

然后让每轮 tool result 都先变成 evidence card。
只要这一步做对了，你的上下文质量会明显比“拼一大坨字符串”高很多。
