//! The tool-calling agent loop (M3, §6.2). Bounded by `max_steps`; every tool
//! execution and consent decision is audited (ADR-019) and every untrusted
//! result is fenced as data before it re-enters the model's context (ADR-016).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;

use crate::AgentCore;
use crate::audit::AuditEntry;
use crate::context;
use crate::events::{ApprovalSink, CoreEvent, Decision, Risk, Usage};
use crate::llm::{ChatMessage, ChatRequest, ToolCall};
use crate::tools::ToolRegistry;
use crate::tools::{Tool, ToolContext};

use super::Emitter;
use super::fence;

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model produced a plain answer (or the step limit was reached).
    Completed,
    /// The provider failed before finishing.
    Failed,
}

/// Everything a turn needs to run. Assembled by `ChatSession::send`.
pub struct TurnInput {
    pub core: Arc<AgentCore>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// The recent message window (already budget-checked by the session).
    pub window: Vec<ChatMessage>,
    pub summary: Option<String>,
    pub emitter: Emitter,
    /// Assistant text streamed so far (whole turn); read by `abort`.
    pub partial: Arc<Mutex<String>>,
    /// Model thinking streamed so far (display-only); read by `abort`.
    pub reasoning: Arc<Mutex<String>>,
    pub cancelled: Arc<AtomicBool>,
    pub approvals: Arc<dyn ApprovalSink>,
    pub turn_id: u64,
    pub max_steps: u32,
}

/// The outcome of a turn: terminal state plus the messages produced.
pub struct LoopResult {
    pub outcome: TurnOutcome,
    /// New history messages the session must persist (tool calls, tool
    /// results, and the final answer).
    pub new_messages: Vec<ChatMessage>,
    /// Total usage across every provider call in the turn, when reported.
    pub usage: Option<Usage>,
}

/// Run a full agent turn: stream the model, execute any requested tools, and
/// loop until a plain answer, the step bound, or a failure.
pub async fn run(input: TurnInput) -> LoopResult {
    let TurnInput {
        core,
        model,
        reasoning_effort,
        window,
        summary,
        emitter,
        partial,
        reasoning,
        cancelled,
        approvals,
        turn_id,
        max_steps,
    } = input;

    let tools: ToolRegistry = core.tools().clone();
    let tool_schemas = tools.schemas();
    let mut messages = context::assemble(None, summary.as_deref(), &window);
    let mut new_messages: Vec<ChatMessage> = Vec::new();
    let mut total_usage = Usage::default();
    let mut saw_usage = false;

    for _step in 0..max_steps.max(1) {
        if cancelled.load(Ordering::SeqCst) {
            return LoopResult {
                outcome: TurnOutcome::Completed,
                new_messages,
                usage: None,
            };
        }

        let request = ChatRequest::new(model.clone(), messages.clone())
            .with_reasoning_effort(reasoning_effort.clone())
            .with_tools(tool_schemas.clone());

        let mut events = match core.client().chat(request).await {
            Ok(events) => events,
            Err(err) => {
                emitter.emit(CoreEvent::error(err.kind, err.message));
                return LoopResult {
                    outcome: TurnOutcome::Failed,
                    new_messages,
                    usage: usage_of(&total_usage, saw_usage),
                };
            }
        };

        let mut step_text = String::new();
        let mut step_reasoning = String::new();
        let mut step_calls: Vec<ToolCall> = Vec::new();
        let mut step_usage: Option<Usage> = None;
        let mut failed: Option<(crate::ApiErrorKind, String)> = None;

        while let Some(event) = events.recv().await {
            if cancelled.load(Ordering::SeqCst) {
                return LoopResult {
                    outcome: TurnOutcome::Completed,
                    new_messages,
                    usage: usage_of(&total_usage, saw_usage),
                };
            }
            match event {
                CoreEvent::Delta { text } => {
                    step_text.push_str(&text);
                    partial
                        .lock()
                        .expect("partial lock poisoned")
                        .push_str(&text);
                    emitter.emit(CoreEvent::Delta { text });
                }
                CoreEvent::Reasoning { text } => {
                    step_reasoning.push_str(&text);
                    reasoning
                        .lock()
                        .expect("reasoning lock poisoned")
                        .push_str(&text);
                    emitter.emit(CoreEvent::Reasoning { text });
                }
                CoreEvent::ToolCall { id, name, input } => {
                    step_calls.push(ToolCall::new(id, name, input));
                }
                CoreEvent::TurnDone { usage } => {
                    step_usage = usage;
                    break;
                }
                CoreEvent::Error { kind, message } => {
                    failed = Some((kind, message));
                    break;
                }
                other => emitter.emit(other),
            }
        }

        if let Some(usage) = step_usage {
            add_report(&mut total_usage, &usage);
            saw_usage = true;
        }
        if let Some((kind, message)) = failed {
            emitter.emit(CoreEvent::error(kind, message));
            return LoopResult {
                outcome: TurnOutcome::Failed,
                new_messages,
                usage: usage_of(&total_usage, saw_usage),
            };
        }

        // No tool calls: this step is the final answer.
        if step_calls.is_empty() {
            new_messages.push(assistant_message(step_text, step_reasoning));
            emitter.emit(CoreEvent::TurnDone {
                usage: usage_of(&total_usage, saw_usage),
            });
            return LoopResult {
                outcome: TurnOutcome::Completed,
                new_messages,
                usage: usage_of(&total_usage, saw_usage),
            };
        }

        // The assistant asked for tools: record the request, then run them.
        let assistant = ChatMessage::assistant_with_tool_calls(step_text, step_calls.clone());
        messages.push(assistant.clone());
        new_messages.push(assistant);

        let context = core.tool_context(turn_id);
        for call in step_calls {
            if cancelled.load(Ordering::SeqCst) {
                return LoopResult {
                    outcome: TurnOutcome::Completed,
                    new_messages,
                    usage: usage_of(&total_usage, saw_usage),
                };
            }
            let call =
                execute_call(&tools, &context, &approvals, &emitter, &core, turn_id, call).await;
            messages.push(call.clone());
            new_messages.push(call);
        }
    }

    // Step limit reached without a plain answer: say so plainly so the history
    // stays coherent and the user sees why the turn stopped.
    let notice = format!(
        "I reached my tool-call step limit ({max_steps}) without finishing. \
         Here is what I gathered so far; ask me to continue if you want more."
    );
    partial
        .lock()
        .expect("partial lock poisoned")
        .push_str(&notice);
    emitter.emit(CoreEvent::Delta {
        text: notice.clone(),
    });
    new_messages.push(ChatMessage::assistant(notice));
    emitter.emit(CoreEvent::TurnDone {
        usage: usage_of(&total_usage, saw_usage),
    });
    LoopResult {
        outcome: TurnOutcome::Completed,
        new_messages,
        usage: usage_of(&total_usage, saw_usage),
    }
}

/// Execute one requested tool call: consent, run, emit, audit, fence.
async fn execute_call(
    tools: &ToolRegistry,
    context: &ToolContext,
    approvals: &Arc<dyn ApprovalSink>,
    emitter: &Emitter,
    core: &Arc<AgentCore>,
    turn_id: u64,
    call: ToolCall,
) -> ChatMessage {
    emitter.emit(CoreEvent::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        input: call.arguments.clone(),
    });

    let started = Instant::now();
    let mut decision: Option<Decision> = None;
    let (output, is_error, status) = match tools.get(&call.name) {
        None => (
            format!(
                "Unknown tool {:?}; available tools: none matching. Do not retry it.",
                call.name
            ),
            true,
            "unknown_tool".to_owned(),
        ),
        Some(tool) => match tool.risk(&call.arguments) {
            Risk::Safe => run_tool(&tool, &call.arguments, context).await,
            Risk::NeedsApproval(kind) => {
                let summary = tool.approval_summary(&call.arguments);
                let resolved = approvals.request(kind, summary).await;
                decision = Some(resolved);
                if resolved == Decision::Deny {
                    (
                        "The user denied this action. Do not retry it without asking again."
                            .to_owned(),
                        true,
                        "denied".to_owned(),
                    )
                } else {
                    run_tool(&tool, &call.arguments, context).await
                }
            }
        },
    };

    let fenced = fence(&format!("tool:{}", call.name), &output);
    emitter.emit(CoreEvent::ToolResult {
        id: call.id.clone(),
        name: call.name.clone(),
        output: fenced.clone(),
        is_error,
    });

    core.audit().append(&AuditEntry {
        turn_id,
        tool: call.name.clone(),
        input: normalize_input(&call.arguments),
        decision: decision.map(|d| match d {
            Decision::Allow => "allow".to_owned(),
            Decision::Deny => "deny".to_owned(),
        }),
        model: None,
        status,
        duration_ms: started.elapsed().as_millis() as u64,
    });

    ChatMessage::tool(call.id, fenced)
}

async fn run_tool(
    tool: &Arc<dyn Tool>,
    input: &Value,
    context: &ToolContext,
) -> (String, bool, String) {
    match tool.execute(input.clone(), context.clone()).await {
        Ok(output) => (output, false, "ok".to_owned()),
        Err(err) => (
            format!("Tool {:?} failed: {}", tool.name(), err.message),
            true,
            format!("error:{}", err.kind.as_str()),
        ),
    }
}

fn assistant_message(content: String, reasoning: String) -> ChatMessage {
    let message = ChatMessage::assistant(content);
    if reasoning.is_empty() {
        message
    } else {
        message.with_reasoning(reasoning)
    }
}

fn usage_of(total: &Usage, saw_usage: bool) -> Option<Usage> {
    saw_usage.then_some(*total)
}

/// Accumulate a provider report, preserving "unknown" (a field stays `None`
/// when no report ever set it). Unlike `Usage::add`, a single report passes
/// through unchanged, so a one-step turn emits exactly what the provider said.
fn add_report(total: &mut Usage, report: &Usage) {
    fn merge(a: Option<u64>, b: Option<u64>) -> Option<u64> {
        match (a, b) {
            (None, None) => None,
            _ => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        }
    }
    total.input_tokens = merge(total.input_tokens, report.input_tokens);
    total.output_tokens = merge(total.output_tokens, report.output_tokens);
    total.total_tokens = merge(total.total_tokens, report.total_tokens);
}

/// Audit input: `null` stays `null` (the audit log skips it).
fn normalize_input(input: &Value) -> Value {
    input.clone()
}
