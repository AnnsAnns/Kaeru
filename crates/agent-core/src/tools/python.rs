//! The `python` tool (M5, ADR-012/013): run a script in the bubblewrap
//! sandbox. Dependency-free scripts are `Safe`; unprepared deps ask for
//! `PackageInstall` consent (the only networked phase), and asking for
//! network escalates to `NetworkAccess`, the only consent that ever grants
//! it. Workspace files a script creates surface as `Artifact` events
//! (ADR-017).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{ApprovalKind, Risk};
use crate::sandbox::{
    FileStamp, MAX_STREAM_BYTES, Sandbox, ScriptOutcome, mime_hint, normalize_deps,
};
use crate::tools::{Tool, ToolContext, ToolFuture};
use crate::util::truncate_chars;

/// Most artifact events one run surfaces.
const MAX_ARTIFACTS: usize = 8;
/// Longest stdout/stderr text handed to the model (the capture itself is
/// bounded by the supervisor; this keeps the context readable).
const MAX_RESULT_STREAM_CHARS: usize = 16 * 1024;

pub struct PythonTool {
    sandbox: Arc<Sandbox>,
}

impl PythonTool {
    pub fn new(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox }
    }

    fn deps_of(input: &Value) -> Vec<String> {
        let raw: Vec<String> = input
            .get("deps")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        normalize_deps(&raw)
    }

    fn wants_network(input: &Value) -> bool {
        input
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

impl Tool for PythonTool {
    fn name(&self) -> &'static str {
        "python"
    }

    fn description(&self) -> &'static str {
        "Run a Python script for data processing in the sandboxed workspace. \
         The script runs offline with the workspace as its working directory; \
         it can only write there, and files it writes can be shown to the \
         user. Request `deps` to use third-party packages; installing them \
         asks the user first. Set `network` only when the script truly needs \
         the internet (the user is asked; it is denied by default)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The complete Python program to run. Relative paths \
                                    resolve inside the workspace; use print() for results."
                },
                "deps": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "PyPI package names the script imports (optional). \
                                    Installing them from the configured index requires \
                                    the user's consent; once prepared, later runs are \
                                    approved automatically."
                },
                "network": {
                    "type": "boolean",
                    "description": "Set true only if the script must access the network. \
                                    Requires explicit user consent and is off by default."
                }
            },
            "required": ["script"]
        })
    }

    fn risk(&self, input: &Value) -> Risk {
        let deps = Self::deps_of(input);
        // Network is the strongest escalation: the card names both the
        // network grant and any install that granting it would also perform.
        if Self::wants_network(input) {
            let reason = if deps.is_empty() {
                "run a Python script with network access".to_owned()
            } else {
                format!(
                    "run a Python script with network access and install {}",
                    deps.join(", ")
                )
            };
            return Risk::NeedsApproval(ApprovalKind::NetworkAccess { reason });
        }
        if !deps.is_empty() && !self.sandbox.env_prepared(&deps) {
            return Risk::NeedsApproval(ApprovalKind::PackageInstall { packages: deps });
        }
        // Prepared env (or no deps): execution is offline and sandboxed and
        // runs without friction (ADR-014).
        Risk::Safe
    }

    fn approval_summary(&self, input: &Value) -> String {
        let deps = Self::deps_of(input);
        if Self::wants_network(input) {
            return match deps.is_empty() {
                true => "Run a Python script with network access inside the sandbox?".to_owned(),
                false => format!(
                    "Run a Python script with network access and install {} from PyPI?",
                    deps.join(", ")
                ),
            };
        }
        format!(
            "Install {} from PyPI for the Python tool? (cached afterwards, no repeated asks)",
            deps.join(", ")
        )
    }

    fn execute(&self, input: Value, ctx: ToolContext) -> ToolFuture {
        let script = match input.get("script").and_then(Value::as_str) {
            Some(script) if !script.trim().is_empty() => script.to_owned(),
            _ => {
                return Box::pin(async {
                    Err(ApiError::config("python requires a non-empty \"script\""))
                });
            }
        };
        let deps = Self::deps_of(&input);
        let network = Self::wants_network(&input);
        let sandbox = Arc::clone(&self.sandbox);
        Box::pin(async move {
            let before = sandbox.snapshot();
            let outcome = sandbox.run_script(&script, &deps, network).await?;
            let after = sandbox.snapshot();
            let artifacts = changed_files(&before, &after);
            if let Some(sink) = &ctx.artifacts {
                for artifact in artifacts.iter().take(MAX_ARTIFACTS) {
                    sink.emit(
                        &artifact.path,
                        mime_hint(std::path::Path::new(&artifact.path)),
                    );
                }
            }
            let report = describe(&outcome, &artifacts)?;
            Ok(report)
        })
    }
}

/// Files created or rewritten by the run, in workspace order.
fn changed_files<'a>(before: &[FileStamp], after: &'a [FileStamp]) -> Vec<&'a FileStamp> {
    let known: HashMap<&str, (u64, Option<SystemTime>)> = before
        .iter()
        .map(|stamp| (stamp.path.as_str(), (stamp.size, stamp.modified)))
        .collect();
    after
        .iter()
        .filter(|stamp| {
            known
                .get(stamp.path.as_str())
                .is_none_or(|(size, modified)| *size != stamp.size || *modified != stamp.modified)
        })
        .collect()
}

/// Render the run for the model; a failed run becomes a structured tool error
/// carrying everything the script printed (M5, §6.5).
fn describe(outcome: &ScriptOutcome, artifacts: &[&FileStamp]) -> Result<String> {
    let mut report = String::new();
    match (outcome.timed_out, outcome.exit_code, outcome.signal) {
        (true, _, _) => report.push_str(&format!(
            "script timed out and was stopped after {} ms\n",
            outcome.duration_ms
        )),
        (_, Some(code), _) => report.push_str(&format!("exit code {code}\n")),
        (_, None, Some(signal)) => {
            report.push_str(&format!("script killed by signal {signal}\n"));
        }
        (_, None, None) => report.push_str("script did not report an exit status\n"),
    }
    report.push_str(&format!("duration {} ms\n", outcome.duration_ms));
    push_stream(&mut report, "stdout", &outcome.stdout);
    push_stream(&mut report, "stderr", &outcome.stderr);
    if outcome.truncated {
        report.push_str(&format!(
            "[output truncated at {} KiB per stream]\n",
            MAX_STREAM_BYTES / 1024
        ));
    }
    if !artifacts.is_empty() {
        report.push_str("workspace files written:\n");
        for artifact in artifacts.iter().take(MAX_ARTIFACTS) {
            match mime_hint(std::path::Path::new(&artifact.path)) {
                Some(mime) => report.push_str(&format!("- {} ({mime})\n", artifact.path)),
                None => report.push_str(&format!("- {}\n", artifact.path)),
            }
        }
        if artifacts.len() > MAX_ARTIFACTS {
            report.push_str(&format!("(and {} more)\n", artifacts.len() - MAX_ARTIFACTS));
        }
    }
    if outcome.success() {
        Ok(report)
    } else {
        Err(ApiError::new(
            ApiErrorKind::Internal,
            format!("python script failed:\n{report}"),
        ))
    }
}

fn push_stream(report: &mut String, name: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    report.push_str(&format!("--- {name} ---\n"));
    report.push_str(&truncate_chars(text, MAX_RESULT_STREAM_CHARS));
    if !text.ends_with('\n') {
        report.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-py-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(name: &str) -> (PythonTool, PathBuf) {
        let root = temp_dir(name);
        let config = SandboxConfig {
            workspace: root.join("ws"),
            read_paths: Vec::new(),
            timeout_secs: 10,
            memory_mb: 256,
        };
        let sandbox = Arc::new(Sandbox::new(&config, root.join("envs")).unwrap());
        (PythonTool::new(sandbox), root)
    }

    fn complete_env(root: &std::path::Path, deps: &[&str]) {
        let deps: Vec<String> = deps.iter().map(|dep| (*dep).to_owned()).collect();
        let dir = root
            .join("envs")
            .join(crate::sandbox::env_id(&deps).as_str());
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join(".complete"), b"").unwrap();
        std::fs::write(dir.join("bin/python"), b"stub").unwrap();
    }

    #[test]
    fn dependency_free_scripts_need_no_consent() {
        let (tool, _) = tool("safe");
        assert_eq!(
            tool.risk(&json!({"script": "print(1)"})),
            Risk::Safe,
            "a plain script runs without friction"
        );
    }

    #[test]
    fn unprepared_deps_ask_for_install_consent_with_the_package_list() {
        let (tool, _) = tool("install");
        assert_eq!(
            tool.risk(&json!({"script": "import pandas", "deps": [" pandas ", "numpy"]})),
            Risk::NeedsApproval(ApprovalKind::PackageInstall {
                packages: vec!["pandas".into(), "numpy".into()],
            })
        );
        let summary = tool.approval_summary(&json!({"script": "x", "deps": ["pandas"]}));
        assert!(summary.contains("pandas"), "{summary}");
    }

    #[test]
    fn a_prepared_dep_set_is_approved_automatically() {
        let (tool, root) = tool("prepared");
        complete_env(&root, &["pandas"]);
        assert_eq!(
            tool.risk(&json!({"script": "import pandas", "deps": ["pandas"]})),
            Risk::Safe,
            "a cached dep set must not ask again (ADR-014)"
        );
    }

    #[test]
    fn network_requests_escalate_and_mention_the_install() {
        let (tool, root) = tool("network");
        complete_env(&root, &["requests"]);
        let risk = tool.risk(&json!({"script": "x", "deps": ["requests"], "network": true}));
        match risk {
            Risk::NeedsApproval(ApprovalKind::NetworkAccess { reason }) => {
                assert!(reason.contains("requests"), "{reason}");
            }
            other => panic!("network must escalate, got {other:?}"),
        }
    }

    #[test]
    fn changed_files_reports_new_and_rewritten_files_only() {
        let stamp = |path: &str, size: u64| FileStamp {
            path: path.to_owned(),
            size,
            modified: None,
        };
        let before = vec![stamp("kept.csv", 3), stamp("edit.txt", 1)];
        let after = vec![
            stamp("kept.csv", 3),
            stamp("edit.txt", 2),
            stamp("new.png", 5),
        ];
        let changed: Vec<&str> = changed_files(&before, &after)
            .into_iter()
            .map(|stamp| stamp.path.as_str())
            .collect();
        assert_eq!(changed, vec!["edit.txt", "new.png"]);
    }

    #[test]
    fn describe_reports_failure_with_output_for_the_model() {
        let outcome = ScriptOutcome {
            stdout: "hello\n".into(),
            stderr: "Traceback\nValueError\n".into(),
            exit_code: Some(1),
            signal: None,
            truncated: false,
            timed_out: false,
            duration_ms: 12,
        };
        let err = describe(&outcome, &[]).unwrap_err();
        assert!(err.message.contains("exit code 1"), "{}", err.message);
        assert!(err.message.contains("hello"), "{}", err.message);
        assert!(err.message.contains("ValueError"), "{}", err.message);
    }

    #[test]
    fn describe_reports_timeouts_and_artifacts() {
        let outcome = ScriptOutcome {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            signal: Some(9),
            truncated: false,
            timed_out: true,
            duration_ms: 60_000,
        };
        let artifact = FileStamp {
            path: "plot.png".into(),
            size: 3,
            modified: None,
        };
        let err = describe(&outcome, &[&artifact]).unwrap_err();
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(err.message.contains("plot.png"), "{}", err.message);
        assert!(err.message.contains("image/png"), "{}", err.message);
    }

    /// Full run through bubblewrap: script writes a file, the tool reports it
    /// and emits one artifact event. Skipped (with a note) where the host
    /// cannot sandbox, so the offline suite still passes everywhere.
    #[tokio::test]
    async fn script_writes_a_workspace_file_and_surfaces_it_as_an_artifact() {
        let (tool, _root) = tool("run");
        if tool.sandbox.check_host().is_err() {
            eprintln!("skipping: bubblewrap is not usable on this host");
            return;
        }
        let input = json!({
            "script": "with open('out.txt', 'w') as f:\n    f.write('hi')\nprint('done')"
        });
        assert_eq!(tool.risk(&input), Risk::Safe);

        let emitted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&emitted);
        let sink = crate::tools::ArtifactSink::new(move |path, mime| {
            sink_events
                .lock()
                .unwrap()
                .push((path.to_owned(), mime.map(str::to_owned)));
        });
        let ctx = ToolContext {
            client: Arc::new(crate::llm::FakeProvider::builtin()),
            search: Arc::new(crate::search::DisabledSearch),
            workers: Arc::new(crate::agent::Workers::from_config(
                Arc::new(crate::llm::FakeProvider::builtin()),
                "test",
                &crate::config::WorkersConfig::default(),
            )),
            audit: crate::audit::AuditLog::disabled(),
            turn_id: 7,
            artifacts: Some(sink),
        };
        let report = tool.execute(input, ctx).await.expect("script runs");
        assert!(report.contains("done"), "{report}");
        assert!(report.contains("out.txt"), "{report}");
        let events = emitted.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "out.txt");

        // The workspace really holds the file (and never the script itself).
        assert!(tool.sandbox.workspace().join("out.txt").is_file());
        assert!(!tool.sandbox.workspace().join("script.py").exists());
    }
}
