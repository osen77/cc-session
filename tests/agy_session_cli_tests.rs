use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

struct AgyFixture {
    home: TempDir,
    config: TempDir,
    logs: TempDir,
}

impl AgyFixture {
    fn new() -> Self {
        let fixture = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            config: tempfile::tempdir().expect("config tempdir"),
            logs: tempfile::tempdir().expect("logs tempdir"),
        };
        // Keep maintenance disabled so hardcoded fixture timestamps stay visible.
        fs::write(
            fixture.config.path().join("config.toml"),
            "[session_maintenance]\nenabled = false\n",
        )
        .expect("write config.toml");
        fixture
    }

    fn agy_root(&self) -> std::path::PathBuf {
        self.home.path().join(".gemini/antigravity-cli")
    }

    fn init_db(&self) -> Connection {
        let root = self.agy_root();
        fs::create_dir_all(&root).expect("create agy root");
        let conn = Connection::open(root.join("conversation_summaries.db")).expect("open sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE conversation_summaries (
                 conversation_id text PRIMARY KEY,
                 title text NOT NULL DEFAULT '',
                 preview text NOT NULL DEFAULT '',
                 step_count integer NOT NULL DEFAULT 0,
                 last_modified_time datetime NOT NULL,
                 workspace_uris text NOT NULL DEFAULT '',
                 status text NOT NULL DEFAULT '',
                 source text NOT NULL DEFAULT '',
                 project_id text NOT NULL DEFAULT '',
                 agent_name text NOT NULL DEFAULT '',
                 parent_conversation_id text NOT NULL DEFAULT '',
                 nesting_depth integer NOT NULL DEFAULT 0,
                 battle_id text NOT NULL DEFAULT '',
                 winning_conversation_id text NOT NULL DEFAULT '',
                 not_fully_idle numeric NOT NULL DEFAULT false,
                 killed numeric NOT NULL DEFAULT false,
                 last_user_input_time datetime NOT NULL DEFAULT '0001-01-01 00:00:00+00:00',
                 last_user_input_step_index integer NOT NULL DEFAULT -1,
                 app_data_dir text NOT NULL DEFAULT '',
                 raw_summary blob,
                 group_id text NOT NULL DEFAULT ''
             );
             CREATE VIRTUAL TABLE conversations_fts USING fts5(
                 conversation_id UNINDEXED,
                 chunk_index UNINDEXED,
                 chunk_first_step UNINDEXED,
                 chunk_last_step UNINDEXED,
                 last_modified_time UNINDEXED,
                 step_count UNINDEXED,
                 last_step_created_at UNINDEXED,
                 archived UNINDEXED,
                 user_step_byte_offsets UNINDEXED,
                 agent_step_byte_offsets UNINDEXED,
                 title,
                 workspace,
                 project,
                 user_body,
                 agent_body,
                 tokenize = 'unicode61 remove_diacritics 2',
                 prefix = '1 2 3'
             );",
        )
        .expect("init schema");
        conn
    }

    fn write_transcript(&self, conv_id: &str, filename: &str, lines: &str) {
        let dir = self
            .agy_root()
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&dir).expect("create logs dir");
        fs::write(dir.join(filename), lines).expect("write transcript");
    }

    fn write_claude_session(&self, id: &str, project_dir_name: &str, cwd: &str, text: &str) {
        let dir = self
            .home
            .path()
            .join(".claude/projects")
            .join(project_dir_name);
        fs::create_dir_all(&dir).expect("create claude dir");
        let line = serde_json::json!({
            "type": "user",
            "sessionId": id,
            "cwd": cwd,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "message": {"role": "user", "content": text},
        });
        fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).expect("write claude");
    }

    fn run(&self, label: &str, args: &[&str]) -> Output {
        let log_path = self.logs.path().join(format!("{label}.log"));
        Command::new(env!("CARGO_BIN_EXE_ccs"))
            .arg("--log-file")
            .arg(&log_path)
            .args(args)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.config.path())
            .env_remove("RUST_LOG")
            .output()
            .expect("run ccs")
    }

    fn run_ok(&self, label: &str, args: &[&str]) -> String {
        let output = self.run(label, args);
        assert!(
            output.status.success(),
            "ccs {args:?} failed: status={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn populate_standard_agy_data(fixture: &AgyFixture) {
    let now = chrono::Utc::now();
    let recent_sqlite = now.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string();
    let recent_rfc = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let recent_ns = now.timestamp_nanos_opt().unwrap_or(0);
    let old_sqlite = "2020-01-01 00:00:00.000000+00:00";
    let old_rfc = "2020-01-01T00:00:00Z";

    let conn = fixture.init_db();

    // 1. Recent Chinese-project session with both FTS5 index and transcript_full.jsonl
    let id1 = "fda33c60-94ee-424f-8b59-bda78f88e945";
    conn.execute(
        "INSERT INTO conversation_summaries \
         (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, project_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'default-cli-project')",
        rusqlite::params![
            id1,
            "Using Agy with Claude Code",
            "介绍下我如何用好 agy",
            6,
            recent_sqlite,
            r#"["file:///Volumes/Data/Projects/%E5%AE%89%E8%A3%85%E7%8E%AF%E5%A2%83"]"#,
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversations_fts \
         (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
         VALUES (?1, -1, 0, 0, ?2, 6, ?2, 0, '', '', 'Using Agy with Claude Code', '%E5%AE%89%E8%A3%85%E7%8E%AF%E5%A2%83', 'CLI', '', '')",
        rusqlite::params![id1, recent_ns],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversations_fts \
         (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
         VALUES (?1, 0, 0, 5, 0, 0, 0, 0, '', '', '', '', '', '如何利用检索库迁移配置\n配置 exact phrase alpha beta 规则', '可以通过 SessionStart hook 自动加载\n回复里包含 assistant_only_token')",
        rusqlite::params![id1],
    )
    .unwrap();

    let transcript1 = format!(
        concat!(
            "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"created_at\":\"{ts}\",\"content\":\"<USER_REQUEST>\\n如何利用检索库迁移配置\\n</USER_REQUEST>\\n<ADDITIONAL_METADATA>\\ntime\\n</ADDITIONAL_METADATA>\"}}\n",
            "{{\"step_index\":1,\"source\":\"SYSTEM_SDK\",\"type\":\"SYSTEM_MESSAGE\",\"created_at\":\"{ts}\",\"content\":\"<SYSTEM_MESSAGE>hidden_system_token</SYSTEM_MESSAGE>\"}}\n",
            "{{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"{ts}\",\"tool_calls\":[{{\"name\":\"read_file\"}}],\"thinking\":\"hidden_thinking_token\"}}\n",
            "{{\"step_index\":3,\"source\":\"MODEL\",\"type\":\"GENERIC\",\"created_at\":\"{ts}\",\"content\":\"Command output: hidden_generic_token\"}}\n",
            "{{\"step_index\":4,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"{ts}\",\"content\":\"可以通过 SessionStart hook 自动加载\"}}\n",
            "{{\"step_index\":5,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"created_at\":\"{ts}\",\"content\":\"<USER_REQUEST>\\n配置 exact phrase alpha beta 规则\\n</USER_REQUEST>\"}}\n",
            "{{\"step_index\":6,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"{ts}\",\"content\":\"回复里包含 assistant_only_token 和 anchor_middle_hit\"}}\n",
        ),
        ts = recent_rfc
    );
    fixture.write_transcript(id1, "transcript_full.jsonl", &transcript1);
    fixture.write_transcript(id1, "transcript.jsonl", &transcript1);

    // 2. Old session in another project (tests --since and -p)
    let id2 = "4f3d08ab-3813-4e74-883f-22c3524a3d8b";
    conn.execute(
        "INSERT INTO conversation_summaries \
         (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, project_id) \
         VALUES (?1, '', 'legacy alpha separate beta task', 2, ?2, '[\"file:///Volumes/Data/Projects/CLIProxyAPI\"]', 'default-cli-project')",
        rusqlite::params![id2, old_sqlite],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversations_fts \
         (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
         VALUES (?1, -1, 0, 0, 1577836800000000000, 2, 1577836800000000000, 0, '', '', '', 'CLIProxyAPI', 'CLI', '', '')",
        rusqlite::params![id2],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversations_fts \
         (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
         VALUES (?1, 0, 0, 1, 0, 0, 0, 0, '', '', '', '', '', 'legacy alpha separate beta task', 'legacy done')",
        rusqlite::params![id2],
    )
    .unwrap();
    let transcript2 = format!(
        concat!(
            "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"created_at\":\"{ts}\",\"content\":\"<USER_REQUEST>\\nlegacy alpha separate beta task\\n</USER_REQUEST>\"}}\n",
            "{{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"{ts}\",\"content\":\"legacy done\"}}\n",
        ),
        ts = old_rfc
    );
    fixture.write_transcript(id2, "transcript.jsonl", &transcript2);

    // 3. Unindexed live session (in DB + brain, NOT yet in conversations_fts)
    let id3 = "e9fba5d7-4523-4609-b494-967fccbaf8b8";
    conn.execute(
        "INSERT INTO conversation_summaries \
         (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, project_id) \
         VALUES (?1, 'Live Unindexed Session', 'live_unindexed_marker', 2, ?2, '[\"file:///Volumes/Data/Projects/claude-code-sync\"]', 'default-cli-project')",
        rusqlite::params![id3, recent_sqlite],
    )
    .unwrap();
    let transcript3 = format!(
        "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"created_at\":\"{recent_rfc}\",\"content\":\"<USER_REQUEST>\\nlive_unindexed_marker\\n</USER_REQUEST>\"}}\n"
    );
    fixture.write_transcript(id3, "transcript.jsonl", &transcript3);

    // 4. DB-only session (no brain/ transcript directory)
    let id4 = "c630d845-cd00-4e4f-bc8f-48d38c50c33e";
    conn.execute(
        "INSERT INTO conversation_summaries \
         (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, project_id) \
         VALUES (?1, '自我介绍', 'db_only_preview_needle', 2, ?2, '', 'outside-of-project')",
        rusqlite::params![id4, recent_sqlite],
    )
    .unwrap();

    // 5. Empty subagent child row (should be filtered out automatically)
    conn.execute(
        "INSERT INTO conversation_summaries \
         (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, parent_conversation_id) \
         VALUES ('8d3d88cc-849e-400d-853b-1b39e2e506c0', '', '', 2, ?1, '[\"file:///private/tmp\"]', ?2)",
        rusqlite::params![recent_sqlite, id1],
    )
    .unwrap();
}

#[test]
fn agy_search_show_list_and_filters_match_existing_sources() {
    let fixture = AgyFixture::new();
    populate_standard_agy_data(&fixture);
    fixture.write_claude_session(
        "cc-shared-1",
        "-Volumes-Data-Projects-claude-code-sync",
        "/Volumes/Data/Projects/claude-code-sync",
        "shared cross source SessionStart check",
    );

    // 1. -s agy search with FTS5 + JSON contract
    let out = fixture.run_ok(
        "search-json",
        &["session", "search", "SessionStart", "-s", "agy", "--json"],
    );
    let payload: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["schema_version"], 1);
    assert!(payload["diagnostics"]["agy_scan_ms"].is_u64());
    let results = payload["session_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["source"], "agy");
    assert_eq!(results[0]["project"], "安装环境");
    assert_eq!(
        results[0]["session_id"],
        "fda33c60-94ee-424f-8b59-bda78f88e945"
    );

    // 2. -s all includes both Claude and Agy
    let all_out = fixture.run_ok(
        "search-all",
        &["session", "search", "SessionStart", "-s", "all", "--json"],
    );
    let all_payload: Value = serde_json::from_str(&all_out).unwrap();
    let sources: Vec<&str> = all_payload["session_results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["source"].as_str())
        .collect();
    assert!(sources.contains(&"claude"));
    assert!(sources.contains(&"agy"));

    // 3. --user-only excludes assistant-only tokens, keeps user tokens
    let user_only_miss = fixture.run_ok(
        "user-only-miss",
        &[
            "session",
            "search",
            "assistant_only_token",
            "-s",
            "agy",
            "--user-only",
            "--json",
        ],
    );
    let miss_val: Value = serde_json::from_str(&user_only_miss).unwrap();
    assert_eq!(miss_val["total_matches"], 0);

    // 4. CJK search works accurately
    let cjk_out = fixture.run_ok(
        "search-cjk",
        &[
            "session",
            "search",
            "检索库",
            "-s",
            "agy",
            "--user-only",
            "--json",
        ],
    );
    let cjk_val: Value = serde_json::from_str(&cjk_out).unwrap();
    assert_eq!(cjk_val["session_results"].as_array().unwrap().len(), 1);

    // 5. --phrase matches adjacent "alpha beta" only, not "alpha separate beta"
    let phrase_out = fixture.run_ok(
        "search-phrase",
        &[
            "session",
            "search",
            "alpha beta",
            "-s",
            "agy",
            "--phrase",
            "--json",
        ],
    );
    let phrase_val: Value = serde_json::from_str(&phrase_out).unwrap();
    let phrase_ids: Vec<&str> = phrase_val["session_results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    assert_eq!(phrase_ids, vec!["fda33c60-94ee-424f-8b59-bda78f88e945"]);

    // 6. -p project filter and --since duration filter
    let proj_out = fixture.run_ok(
        "search-proj",
        &[
            "session",
            "search",
            "legacy",
            "-s",
            "agy",
            "-p",
            "CLIProxyAPI",
            "--json",
        ],
    );
    let proj_val: Value = serde_json::from_str(&proj_out).unwrap();
    assert_eq!(proj_val["session_results"].as_array().unwrap().len(), 1);

    let since_out = fixture.run_ok(
        "search-since",
        &[
            "session", "search", "legacy", "-s", "agy", "--since", "1d", "--json",
        ],
    );
    let since_val: Value = serde_json::from_str(&since_out).unwrap();
    assert_eq!(since_val["total_matches"], 0);

    // 7. System injections, tool outputs, and thinking are excluded
    for noise in [
        "hidden_system_token",
        "hidden_thinking_token",
        "hidden_generic_token",
    ] {
        let out = fixture.run_ok(
            "search-noise",
            &["session", "search", noise, "-s", "agy", "--json"],
        );
        let val: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(val["total_matches"], 0, "noise leaked into search: {noise}");
    }

    // 8. Unindexed live session and DB-only session are both searchable
    let live_out = fixture.run_ok(
        "search-live",
        &[
            "session",
            "search",
            "live_unindexed_marker",
            "-s",
            "agy",
            "--json",
        ],
    );
    let live_val: Value = serde_json::from_str(&live_out).unwrap();
    assert_eq!(live_val["session_results"].as_array().unwrap().len(), 1);

    let db_only_out = fixture.run_ok(
        "search-db-only",
        &[
            "session",
            "search",
            "db_only_preview_needle",
            "-s",
            "agy",
            "--json",
        ],
    );
    let db_only_val: Value = serde_json::from_str(&db_only_out).unwrap();
    assert_eq!(
        db_only_val["session_results"][0]["session_id"],
        "c630d845-cd00-4e4f-bc8f-48d38c50c33e"
    );

    // 9. 8-char prefix matching in `ccs session show` + `--around` + `-n` + `--json`
    let show_out = fixture.run_ok(
        "show-around",
        &[
            "session",
            "show",
            "fda33c60",
            "--around",
            "anchor_middle_hit",
            "-n",
            "1",
            "--json",
        ],
    );
    let show_val: Value = serde_json::from_str(&show_out).unwrap();
    assert_eq!(show_val["source"], "agy");
    assert_eq!(show_val["project"], "安装环境");
    let msgs = show_val["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs
        .iter()
        .any(|m| m["content"].as_str().unwrap().contains("anchor_middle_hit")));
    // Verify <USER_REQUEST> wrapper was stripped
    assert!(!show_out.contains("<USER_REQUEST>"));
    assert!(!show_out.contains("<ADDITIONAL_METADATA>"));

    // 10. Non-JSON `ccs session show <8位ID>` renders [AG] without blocking
    let show_text = fixture.run_ok("show-text", &["session", "show", "fda33c60"]);
    assert!(show_text.contains("[AG]"));
    assert!(show_text.contains("安装环境"));

    // 11. `ccs session list -s agy --show-ids` and `overview -s agy --json`
    let list_text = fixture.run_ok("list-agy", &["session", "list", "-s", "agy", "--show-ids"]);
    assert!(list_text.contains("[AG]"));
    assert!(list_text.contains("fda33c60-94ee-424f-8b59-bda78f88e945"));
    assert!(!list_text.contains("8d3d88cc-849e-400d-853b-1b39e2e506c0"));

    let overview_out = fixture.run_ok(
        "overview-agy",
        &["session", "overview", "-s", "agy", "--json"],
    );
    let overview_val: Value = serde_json::from_str(&overview_out).unwrap();
    assert!(overview_val["total_projects"].as_u64().unwrap() >= 3);

    // 12. -n limit and -c context chars work in text output
    let limit_text = fixture.run_ok(
        "search-limit",
        &[
            "session", "search", "alpha", "-s", "agy", "-n", "1", "-c", "30",
        ],
    );
    assert!(limit_text.contains("[AG]"));
    assert!(limit_text.contains("showing 1 of"));

    // 13. Read-only guards: rename and delete fail cleanly
    let rename_out = fixture.run(
        "rename-agy",
        &["session", "rename", "fda33c60", "new title", "-s", "agy"],
    );
    assert!(!rename_out.status.success());
    assert!(String::from_utf8_lossy(&rename_out.stderr).contains("read-only"));

    let delete_out = fixture.run(
        "delete-agy",
        &["session", "delete", "fda33c60", "--force", "-s", "agy"],
    );
    assert!(!delete_out.status.success());
    assert!(String::from_utf8_lossy(&delete_out.stderr).contains("read-only"));
}

#[test]
fn missing_agy_dir_and_brain_only_fallback_behave_gracefully() {
    // Missing ~/.gemini/antigravity-cli: zero errors, zero warnings
    let empty = AgyFixture::new();
    let out = empty.run_ok(
        "missing-agy",
        &["session", "search", "anything", "-s", "agy", "--json"],
    );
    let val: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(val["total_matches"], 0);
    assert_eq!(val["diagnostics"]["degraded"], false);
    assert_eq!(val["diagnostics"]["io_errors"], 0);

    // Brain-only transcript with no SQLite database
    let brain_only = AgyFixture::new();
    brain_only.write_transcript(
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "transcript.jsonl",
        "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-26T10:00:00Z\",\"content\":\"<USER_REQUEST>\\nbrain_only_fallback_token\\n</USER_REQUEST>\"}\n",
    );
    assert!(!Path::new(&brain_only.agy_root().join("conversation_summaries.db")).exists());
    let fb_out = brain_only.run_ok(
        "brain-only",
        &[
            "session",
            "search",
            "brain_only_fallback_token",
            "-s",
            "agy",
            "--json",
        ],
    );
    let fb_val: Value = serde_json::from_str(&fb_out).unwrap();
    assert_eq!(fb_val["session_results"].as_array().unwrap().len(), 1);
    assert_eq!(
        fb_val["session_results"][0]["session_id"],
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    );
    assert_eq!(fb_val["diagnostics"]["degraded"], false);
}

#[test]
fn agy_only_queries_skip_loading_session_index_cache() {
    let fixture = AgyFixture::new();
    populate_standard_agy_data(&fixture);
    // Corrupt session_index.json so any attempt to read it would emit a cache error.
    fs::write(
        fixture.config.path().join("session_index.json"),
        b"{not valid json",
    )
    .expect("write corrupt session_index.json");

    let out = fixture.run_ok(
        "agy-skips-cache",
        &["session", "search", "SessionStart", "-s", "agy", "--json"],
    );
    let val: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(val["session_results"].as_array().unwrap().len(), 1);
    assert_eq!(val["diagnostics"]["degraded"], false);
    assert_eq!(val["diagnostics"]["cache_errors"], 0);
    assert_eq!(val["diagnostics"]["cache_load_ms"], 0);
}
