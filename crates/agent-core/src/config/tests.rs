use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn parse_full_config() {
    let config = Config::parse(
        r##"
port = 9000
auth_token = "tok123"
[provider]
base_url = "http://127.0.0.1:11434/v1"
api_key = "ollama-needs-none"
model = "llama3"
"##,
    )
    .unwrap();
    assert_eq!(config.port, 9000);
    assert_eq!(config.auth_token(), Some("tok123"));
    assert_eq!(config.provider.model, "llama3");
}

#[test]
fn missing_fields_fall_back_to_defaults() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.provider.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.provider.api_key, "");
    assert_eq!(config.provider.model, "m");
    assert_eq!(config.auth_token(), None);
}

#[test]
fn empty_auth_token_is_disabled() {
    let config = Config::parse("auth_token = \"   \"\n").unwrap();
    assert_eq!(config.auth_token(), None);
}

#[test]
fn base_url_trailing_slash_is_normalized() {
    let config = Config::parse("[provider]\nbase_url = \"http://x/v1/\"\n").unwrap();
    assert_eq!(config.provider.base_url, "http://x/v1");
}

#[test]
fn unknown_fields_are_rejected() {
    let err = Config::parse("port_typo = 1\n").unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Config);
    assert!(err.message.contains("invalid config"));
}

#[test]
fn empty_model_is_rejected() {
    let err = Config::parse("[provider]\nmodel = \"\"\n").unwrap_err();
    assert!(err.message.contains("model"));
}

#[test]
fn context_budget_is_configurable_with_a_default() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert_eq!(config.context.max_prompt_tokens, DEFAULT_MAX_PROMPT_TOKENS);
    let config =
        Config::parse("[provider]\nmodel = \"m\"\n[context]\nmax_prompt_tokens = 100\n").unwrap();
    assert_eq!(config.context.max_prompt_tokens, 100);
}

#[test]
fn search_config_defaults_to_off_and_is_normalized() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert_eq!(config.search.kind(), SearchProviderKind::Off);
    assert_eq!(config.search.max_results, DEFAULT_SEARCH_RESULTS);

    let config =
        Config::parse("[search]\nprovider = \"BRAVE\"\napi_key = \" k \"\nmax_results = 3\n")
            .unwrap();
    assert_eq!(config.search.kind(), SearchProviderKind::Brave);
    assert_eq!(config.search.api_key, "k");
    assert_eq!(config.search.max_results, 3);
}

#[test]
fn agent_and_worker_config_have_defaults() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert_eq!(config.agent.max_steps, DEFAULT_MAX_STEPS);
    assert_eq!(config.workers.summarizer.model, "");
    assert_eq!(
        config.workers.summarizer.max_output_tokens,
        DEFAULT_WORKER_MAX_OUTPUT_TOKENS
    );
    assert_eq!(config.workers.distiller.model, "");
    assert_eq!(
        config.workers.distiller.max_output_tokens,
        DEFAULT_WORKER_MAX_OUTPUT_TOKENS
    );

    let config = Config::parse(
        "[agent]\nmax_steps = 3\n[workers.summarizer]\nmodel = \"cheap/m\"\nmax_output_tokens = 128\n",
    )
    .unwrap();
    assert_eq!(config.agent.max_steps, 3);
    assert_eq!(config.workers.summarizer.model, "cheap/m");
    assert_eq!(config.workers.summarizer.max_output_tokens, 128);
}

#[test]
fn sandbox_and_files_config_have_defaults_and_are_validated() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert_eq!(
        config.sandbox.workspace,
        PathBuf::from(DEFAULT_SANDBOX_WORKSPACE)
    );
    assert!(config.sandbox.read_paths.is_empty());
    assert_eq!(config.sandbox.timeout_secs, DEFAULT_SANDBOX_TIMEOUT_SECS);
    assert_eq!(config.sandbox.memory_mb, DEFAULT_SANDBOX_MEMORY_MB);
    assert_eq!(config.files.max_upload_mb, DEFAULT_MAX_UPLOAD_MB);
    assert_eq!(
        Paths::default().sandbox_envs,
        PathBuf::from("data/sandbox/envs")
    );

    let config = Config::parse(
        "[provider]\nmodel = \"m\"\n[sandbox]\nworkspace = \"/tmp/ws\"\nread_paths = [\"/tmp/ro\"]\ntimeout_secs = 5\nmemory_mb = 64\n[files]\nmax_upload_mb = 7\n",
    )
    .unwrap();
    assert_eq!(config.sandbox.workspace, PathBuf::from("/tmp/ws"));
    assert_eq!(config.sandbox.read_paths, vec![PathBuf::from("/tmp/ro")]);
    assert_eq!(config.sandbox.timeout_secs, 5);
    assert_eq!(config.sandbox.memory_mb, 64);
    assert_eq!(config.files.max_upload_mb, 7);

    // Zero means "unset": normalized back to the defaults, never a
    // division-by-zero-shaped surprise at run time.
    let config = Config::parse(
        "[provider]\nmodel = \"m\"\n[sandbox]\ntimeout_secs = 0\nmemory_mb = 0\n[files]\nmax_upload_mb = 0\n",
    )
    .unwrap();
    assert_eq!(config.sandbox.timeout_secs, DEFAULT_SANDBOX_TIMEOUT_SECS);
    assert_eq!(config.sandbox.memory_mb, DEFAULT_SANDBOX_MEMORY_MB);
    assert_eq!(config.files.max_upload_mb, DEFAULT_MAX_UPLOAD_MB);
}

#[test]
fn reflect_config_defaults_off_and_normalizes_time() {
    let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
    assert!(!config.reflect.enabled);
    assert_eq!(config.reflect.time, DEFAULT_REFLECT_TIME);
    assert_eq!(config.reflect.scheduled_minutes(), 21 * 60);
    assert!(config.reflect.persona_edits);
    assert_eq!(
        config.workers.reflector.max_output_tokens,
        DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS
    );

    let config = Config::parse(
        "[reflect]\nenabled = true\ntime = \" 07:05 \"\npersona_edits = false\n[workers.reflector]\nmodel = \"r/m\"\n",
    )
    .unwrap();
    assert!(config.reflect.enabled);
    assert_eq!(config.reflect.time, "07:05");
    assert_eq!(config.reflect.scheduled_minutes(), 7 * 60 + 5);
    assert!(!config.reflect.persona_edits);
    assert_eq!(config.workers.reflector.model, "r/m");
    assert_eq!(
        config.workers.reflector.max_output_tokens,
        DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS
    );

    let config = Config::parse("[reflect]\ntime = \"bogus\"\n").unwrap();
    assert_eq!(config.reflect.time, DEFAULT_REFLECT_TIME);
}

#[test]
fn load_creates_default_file_with_tight_permissions() {
    let dir = temp_dir("load-default");
    let path = dir.join("data/config.toml");
    let config = Config::load(&path).unwrap();
    assert!(path.is_file());
    assert_eq!(config.provider.base_url, DEFAULT_BASE_URL);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let reloaded = Config::load(&path).unwrap();
    assert_eq!(reloaded, config);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_reports_broken_config_with_path() {
    let dir = temp_dir("load-broken");
    let path = dir.join("config.toml");
    std::fs::write(&path, "port = \"not-a-number\"\n").unwrap();
    let err = Config::load(&path).unwrap_err();
    assert!(err.message.contains("invalid config"));
    std::fs::remove_dir_all(&dir).ok();
}
