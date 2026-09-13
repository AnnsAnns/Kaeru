use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kaeru-sb-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sandbox(name: &str) -> Sandbox {
    let root = temp_dir(name);
    let config = SandboxConfig {
        workspace: root.join("ws"),
        read_paths: Vec::new(),
        timeout_secs: 10,
        memory_mb: 128,
    };
    Sandbox::new(&config, root.join("envs")).unwrap()
}

#[test]
fn new_creates_the_workspace_and_envs_dirs() {
    let sandbox = sandbox("dirs");
    assert!(sandbox.workspace().is_dir());
    assert!(sandbox.envs().is_dir());
    assert_eq!(sandbox.limits().wall_secs, 10);
    assert_eq!(sandbox.limits().memory_bytes, 128 * 1024 * 1024);
}

fn sandbox_with(name: &str, timeout_secs: u64, memory_mb: u64) -> Sandbox {
    let root = temp_dir(name);
    let config = SandboxConfig {
        workspace: root.join("ws"),
        read_paths: Vec::new(),
        timeout_secs,
        memory_mb,
    };
    Sandbox::new(&config, root.join("envs")).unwrap()
}

/// True when the host can actually sandbox; integration tests skip
/// (with a note) rather than fail where bwrap cannot run.
fn usable(sandbox: &Sandbox) -> bool {
    match sandbox.check_host() {
        Ok(()) => true,
        Err(err) => {
            eprintln!("skipping sandbox integration test: {err}");
            false
        }
    }
}

#[tokio::test]
async fn a_script_runs_offline_and_the_workspace_is_the_only_writable_path() {
    let sandbox = sandbox_with("run", 20, 512);
    if !usable(&sandbox) {
        return;
    }
    let script = r#"
results = []
try:
    open('/etc/kaeru-escape.txt', 'w').write('x')
    results.append('ESCAPED-ETC')
except Exception:
    results.append('blocked-etc')
try:
    open('../kaeru-escape.txt', 'w').write('x')
    results.append('ESCAPED-PARENT')
except Exception:
    results.append('blocked-parent')
open('ok.txt', 'w').write('x')
results.append('wrote-workspace')
print('|'.join(results))
"#;
    let outcome = sandbox.run_script(script, &[], false).await.unwrap();
    assert!(outcome.success(), "{outcome:?}");
    assert!(outcome.stdout.contains("blocked-etc"), "{outcome:?}");
    assert!(outcome.stdout.contains("blocked-parent"), "{outcome:?}");
    assert!(outcome.stdout.contains("wrote-workspace"), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(sandbox.workspace().join("ok.txt")).unwrap(),
        "x"
    );
    assert!(!std::path::Path::new("/etc/kaeru-escape.txt").exists());
}

#[tokio::test]
async fn a_script_cannot_open_a_network_connection() {
    let sandbox = sandbox_with("net", 20, 512);
    if !usable(&sandbox) {
        return;
    }
    let script = r#"
import socket
try:
    socket.create_connection(('1.1.1.1', 80), timeout=3)
    print('NETWORK-OPEN')
except Exception as err:
    print('no-network:', type(err).__name__)
"#;
    let outcome = sandbox.run_script(script, &[], false).await.unwrap();
    assert!(outcome.stdout.contains("no-network"), "{outcome:?}");
    assert!(!outcome.stdout.contains("NETWORK-OPEN"), "{outcome:?}");
}

#[tokio::test]
async fn the_wall_clock_timeout_kills_a_hung_script() {
    let sandbox = sandbox_with("timeout", 1, 256);
    if !usable(&sandbox) {
        return;
    }
    let outcome = sandbox
        .run_script(
            "import time\nprint('started', flush=True)\ntime.sleep(60)\n",
            &[],
            false,
        )
        .await
        .unwrap();
    assert!(outcome.timed_out, "{outcome:?}");
    assert!(!outcome.success());
    assert!(
        outcome.stdout.contains("started"),
        "output before the kill is kept: {outcome:?}"
    );
    assert!(
        outcome.duration_ms < 15_000,
        "killed promptly, took {} ms",
        outcome.duration_ms
    );
}

#[tokio::test]
async fn extra_read_paths_are_visible_and_read_only() {
    let root = temp_dir("reads");
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("notes.txt"), b"hello-read-path").unwrap();
    let config = SandboxConfig {
        workspace: root.join("ws"),
        read_paths: vec![shared.clone()],
        timeout_secs: 20,
        memory_mb: 512,
    };
    let sandbox = Sandbox::new(&config, root.join("envs")).unwrap();
    if !usable(&sandbox) {
        return;
    }
    let script = format!(
        r#"
print(open('{path}/notes.txt').read())
try:
    open('{path}/notes.txt', 'a').write('x')
    print('WROTE-READ-PATH')
except Exception:
    print('read-only')
"#,
            path = shared.display()
        );
        let outcome = sandbox.run_script(&script, &[], false).await.unwrap();
        assert!(outcome.stdout.contains("hello-read-path"), "{outcome:?}");
        assert!(!outcome.stdout.contains("WROTE-READ-PATH"), "{outcome:?}");
        assert!(outcome.stdout.contains("read-only"), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(shared.join("notes.txt")).unwrap(),
            "hello-read-path",
            "the host copy is untouched"
        );
    }

    #[tokio::test]
    async fn the_memory_limit_stops_an_allocation() {
        let sandbox = sandbox_with("memory", 20, 128);
        if !usable(&sandbox) {
            return;
        }
        let outcome = sandbox
            .run_script(
                "x = bytearray(400 * 1024 * 1024)\nprint('ALLOCATED')\n",
                &[],
                false,
            )
            .await
            .unwrap();
        assert!(!outcome.success(), "{outcome:?}");
        assert!(!outcome.stdout.contains("ALLOCATED"), "{outcome:?}");
    }

    #[test]
    fn snapshot_lists_files_relative_and_skips_internal_names() {
        let sandbox = sandbox("snapshot");
        std::fs::write(sandbox.workspace().join("plot.png"), b"png").unwrap();
        std::fs::create_dir_all(sandbox.workspace().join("sub")).unwrap();
        std::fs::write(sandbox.workspace().join("sub/data.csv"), b"a,b").unwrap();
        std::fs::write(sandbox.workspace().join(".hidden"), b"x").unwrap();
        std::fs::create_dir_all(sandbox.workspace().join("__pycache__")).unwrap();
        std::fs::write(sandbox.workspace().join("__pycache__/m.pyc"), b"x").unwrap();

        let paths: Vec<String> = sandbox
            .snapshot()
            .into_iter()
            .map(|stamp| stamp.path)
            .collect();
        assert_eq!(paths, vec!["plot.png", "sub/data.csv"]);
    }
