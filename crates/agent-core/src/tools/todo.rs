//! The `todo` tool (M7, ADR-029): the agent's edit path for named TODO lists.
//! Reads (`list`, `show`) are safe; every mutation is consent-gated because the
//! lists persist across sessions (ADR-016), and the agent loop audits each call.

use serde_json::{Value, json};

use crate::error::{ApiError, Result};
use crate::events::{ApprovalKind, Risk};
use crate::todos::{TodoList, TodoStore};
use crate::tools::{Tool, ToolContext, ToolFuture};

/// The `todo` tool: create, read and edit named TODO lists.
pub struct TodoTool {
    store: TodoStore,
}

impl TodoTool {
    pub fn new(store: TodoStore) -> Self {
        Self { store }
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Manage the user's TODO lists (shopping, projects, …). Use action=\"list\" \
         to see the lists and \"show\" to read one, then create/add/check/uncheck/\
         rename/rename_item/remove/delete_list to edit. Edits need the user's \
         consent because lists persist across sessions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list", "show", "create", "add", "check", "uncheck",
                        "rename", "rename_item", "remove", "delete_list"
                    ],
                    "description": "What to do."
                },
                "list": {
                    "type": "string",
                    "description": "List slug (from `list`); required for show/add/check/uncheck/rename/rename_item/remove/delete_list."
                },
                "title": {
                    "type": "string",
                    "description": "List title (create), or the new title (rename)."
                },
                "text": {
                    "type": "string",
                    "description": "Item text (add), or the new item text (rename_item)."
                },
                "item": {
                    "type": "string",
                    "description": "Item id (check, uncheck, rename_item, remove)."
                }
            },
            "required": ["action"]
        })
    }

    /// Reads are safe; every mutation is consent-gated (ADR-016/ADR-029).
    fn risk(&self, input: &Value) -> Risk {
        match action_of(input) {
            "list" | "show" => Risk::Safe,
            _ => Risk::NeedsApproval(ApprovalKind::TodoWrite {
                list: target_list(input),
            }),
        }
    }

    fn approval_summary(&self, input: &Value) -> String {
        let action = action_of(input);
        match action {
            "create" => format!(
                "Create a TODO list {:?}",
                field(input, "title").unwrap_or_default()
            ),
            "delete_list" => format!("Delete the TODO list {:?}", target_list(input)),
            "rename" => format!(
                "Rename the TODO list {:?} to {:?}",
                target_list(input),
                field(input, "title").unwrap_or_default()
            ),
            "add" => format!(
                "Add a TODO item to {:?}: {:?}",
                target_list(input),
                field(input, "text").unwrap_or_default()
            ),
            "check" | "uncheck" => format!(
                "Mark a TODO item in {:?} as {}done",
                target_list(input),
                if action == "uncheck" { "not " } else { "" }
            ),
            "rename_item" => format!("Rename a TODO item in {:?}", target_list(input)),
            "remove" => format!("Remove a TODO item from {:?}", target_list(input)),
            other => format!("Apply TODO action {other:?}"),
        }
    }

    fn execute(&self, input: Value, _ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move { run(&store, &input) })
    }
}

/// Run one action against the store, returning text the model can read.
fn run(store: &TodoStore, input: &Value) -> Result<String> {
    let action = action_of(input);
    match action {
        "list" => {
            let lists = store.list()?;
            if lists.is_empty() {
                return Ok("No TODO lists yet. Use action=\"create\" to make one.".to_owned());
            }
            let mut out = format!("{} TODO list(s):", lists.len());
            for list in lists {
                out.push_str(&format!(
                    "\n- {} — {} ({} open, {} done)",
                    list.slug, list.title, list.open, list.done
                ));
            }
            Ok(out)
        }
        "show" => {
            let slug = required(input, "list", action)?;
            Ok(render_list(&store.get(slug)?))
        }
        "create" => {
            let title = required(input, "title", action)?;
            let list = store.create(title)?;
            Ok(format!(
                "Created list {:?} (slug: {}).",
                list.title, list.slug
            ))
        }
        "add" => {
            let slug = required(input, "list", action)?;
            let text = required(input, "text", action)?;
            let item = store.add_item(slug, text)?;
            Ok(format!("Added to {slug}: [ ] {} {}", item.id, item.text))
        }
        "check" | "uncheck" => {
            let slug = required(input, "list", action)?;
            let id = required(input, "item", action)?;
            let done = action == "check";
            let item = store.set_done(slug, id, done)?;
            Ok(format!(
                "{slug}: [{}] {} {}",
                if done { "x" } else { " " },
                item.id,
                item.text
            ))
        }
        "rename" => {
            let slug = required(input, "list", action)?;
            let title = required(input, "title", action)?;
            let list = store.rename(slug, title)?;
            Ok(format!("Renamed list {slug} to {:?}.", list.title))
        }
        "rename_item" => {
            let slug = required(input, "list", action)?;
            let id = required(input, "item", action)?;
            let text = required(input, "text", action)?;
            let item = store.rename_item(slug, id, text)?;
            Ok(format!("{slug}: renamed {} to {:?}.", item.id, item.text))
        }
        "remove" => {
            let slug = required(input, "list", action)?;
            let id = required(input, "item", action)?;
            store.remove_item(slug, id)?;
            Ok(format!("Removed item {id} from {slug}."))
        }
        "delete_list" => {
            let slug = required(input, "list", action)?;
            store.delete(slug)?;
            Ok(format!("Deleted list {slug}."))
        }
        "" => Err(ApiError::config("todo requires an \"action\"")),
        other => Err(ApiError::config(format!("unknown todo action {other:?}"))),
    }
}

/// Render one list for the model: a header plus `[x] id text` lines.
fn render_list(list: &TodoList) -> String {
    let mut out = format!(
        "{} — {} ({} open, {} done)",
        list.slug,
        list.title,
        list.open_count(),
        list.done_count()
    );
    if list.items.is_empty() {
        out.push_str("\n(empty)");
        return out;
    }
    for item in &list.items {
        out.push_str(&format!(
            "\n[{}] {} {}",
            if item.done { "x" } else { " " },
            item.id,
            item.text
        ));
    }
    out
}

fn action_of(input: &Value) -> &str {
    input
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

fn field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn target_list(input: &Value) -> String {
    field(input, "list").unwrap_or("new list").to_owned()
}

fn required<'a>(input: &'a Value, key: &str, action: &str) -> Result<&'a str> {
    field(input, key)
        .ok_or_else(|| ApiError::config(format!("todo {action:?} requires a non-empty \"{key}\"")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::search::DisabledSearch;
    use std::sync::Arc;

    fn tool(name: &str) -> (TodoTool, TodoStore) {
        let store = TodoStore::new(crate::util::temp_dir("todo-tool", name));
        (TodoTool::new(store.clone()), store)
    }

    fn ctx() -> ToolContext {
        let client: Arc<dyn crate::llm::LlmClient> = Arc::new(crate::llm::FakeProvider::builtin());
        ToolContext {
            client: Arc::clone(&client),
            search: Arc::new(DisabledSearch),
            workers: Arc::new(crate::agent::workers::Workers::disabled(client)),
            audit: AuditLog::disabled(),
            turn_id: 1,
            artifacts: None,
        }
    }

    #[test]
    fn reads_are_safe_and_mutations_need_consent() {
        let (tool, _store) = tool("risk");
        assert!(matches!(tool.risk(&json!({"action": "list"})), Risk::Safe));
        assert!(matches!(
            tool.risk(&json!({"action": "show", "list": "shopping"})),
            Risk::Safe
        ));
        match tool.risk(&json!({"action": "add", "list": "shopping", "text": "milk"})) {
            Risk::NeedsApproval(ApprovalKind::TodoWrite { list }) => assert_eq!(list, "shopping"),
            other => panic!("expected TodoWrite approval, got {other:?}"),
        }
        // A create has no list yet; the card still names the intent.
        match tool.risk(&json!({"action": "create", "title": "Groceries"})) {
            Risk::NeedsApproval(ApprovalKind::TodoWrite { list }) => {
                assert_eq!(list, "new list")
            }
            other => panic!("expected TodoWrite approval, got {other:?}"),
        }
        assert!(
            tool.approval_summary(&json!({"action": "add", "list": "shopping", "text": "milk"}))
                .contains("shopping")
        );
    }

    #[tokio::test]
    async fn create_add_show_check_and_remove_through_the_tool() {
        let (tool, store) = tool("flow");
        let ctx = ctx();

        let created = tool
            .execute(
                json!({"action": "create", "title": "Shopping"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert!(created.contains("shopping"));

        let added = tool
            .execute(
                json!({"action": "add", "list": "shopping", "text": "oat milk"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert!(added.contains("oat milk"));
        let item_id = store.get("shopping").unwrap().items[0].id.clone();

        let shown = tool
            .execute(json!({"action": "show", "list": "shopping"}), ctx.clone())
            .await
            .unwrap();
        assert!(shown.contains("oat milk"));
        assert!(shown.contains("[ ]"));

        tool.execute(
            json!({"action": "check", "list": "shopping", "item": item_id}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert!(store.get("shopping").unwrap().items[0].done);

        let listed = tool
            .execute(json!({"action": "list"}), ctx.clone())
            .await
            .unwrap();
        assert!(listed.contains("shopping"));
        assert!(listed.contains("0 open, 1 done"));

        tool.execute(
            json!({"action": "remove", "list": "shopping", "item": item_id}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert!(store.get("shopping").unwrap().items.is_empty());

        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[tokio::test]
    async fn missing_arguments_are_structured_errors() {
        let (tool, store) = tool("errors");
        let ctx = ctx();
        assert!(tool.execute(json!({}), ctx.clone()).await.is_err());
        assert!(
            tool.execute(json!({"action": "frobnicate"}), ctx.clone())
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"action": "add", "list": "shopping"}), ctx.clone())
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"action": "show", "list": "ghost"}), ctx)
                .await
                .is_err()
        );
        std::fs::remove_dir_all(store.dir()).ok();
    }
}
