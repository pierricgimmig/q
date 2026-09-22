use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_q"))
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("q-cli-{label}-{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "q-test@example.com"]);
    git(dir, &["config", "user.name", "q-test"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-m", "init"]);
    git(
        dir,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/profiler-core.git",
        ],
    );
}

fn run(cmd: &mut Command) -> std::process::Output {
    let output = cmd.output().expect("run q");
    if !output.status.success() {
        panic!(
            "command failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

#[test]
fn capture_ready_and_claim_json_stay_on_protocol_streams() {
    let root = temp_root("repo");
    init_repo(&root);
    let nested = root.join("crates").join("trace");
    fs::create_dir_all(&nested).unwrap();
    let db = temp_root("db").join("queue.db");

    let captured = run(bin().current_dir(&nested).env("RUST_LOG", "info").args([
        "--db",
        db.to_str().unwrap(),
        "--json",
        "Benchmark delta coding versus varint timestamps",
    ]));
    let created: Value = serde_json::from_slice(&captured.stdout).expect("capture json");
    assert_eq!(created["status"], "inbox");
    assert_eq!(created["repo"], "github.com/acme/profiler-core");
    assert_eq!(created["project"], "profiler-core");
    assert_eq!(created["repo_relative_path"], "crates/trace");
    assert!(created["git_head"].as_str().unwrap().len() >= 7);
    let id = created["id"].as_i64().unwrap();

    let shown = run(bin().args([
        "--db",
        db.to_str().unwrap(),
        "show",
        &id.to_string(),
        "--json",
    ]));
    let detail: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(detail["status"], "inbox");
    assert_eq!(
        detail["original_capture"],
        "Benchmark delta coding versus varint timestamps"
    );

    let empty = run(bin().env("RUST_LOG", "info").args([
        "--db",
        db.to_str().unwrap(),
        "claim",
        "--agent",
        "codex-local-01",
        "--json",
    ]));
    let stderr = String::from_utf8(empty.stderr.clone()).unwrap();
    assert!(
        stderr.contains("claim_next") || stderr.contains("opening queue"),
        "expected logs on stderr, got {stderr}"
    );
    let claim: Value = serde_json::from_slice(&empty.stdout).expect("stdout must be json only");
    assert_eq!(claim["found"], false);
    assert_eq!(claim["reason"], "no_eligible_ready_tasks");
    assert!(!String::from_utf8_lossy(&empty.stdout).contains("INFO"));

    let ready = run(bin().args(["--db", db.to_str().unwrap(), "ready", &id.to_string()]));
    let ready_out = String::from_utf8_lossy(&ready.stdout);
    assert!(ready_out.contains("ready"), "{ready_out}");
    let ready_err = String::from_utf8_lossy(&ready.stderr);
    assert!(ready_err.contains("Goal"), "{ready_err}");
    let shown = run(bin().args([
        "--db",
        db.to_str().unwrap(),
        "show",
        &id.to_string(),
        "--json",
    ]));
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["status"], "ready");
    let claimed = run(bin().env("RUST_LOG", "info").args([
        "--db",
        db.to_str().unwrap(),
        "claim",
        "--agent",
        "codex-local-01",
        "--json",
    ]));
    let claimed: Value = serde_json::from_slice(&claimed.stdout).expect("claim json");
    assert_eq!(claimed["found"], true);
    assert_eq!(claimed["task"]["id"], id);
    assert_eq!(claimed["task"]["status"], "claimed");
    assert!(claimed["claim"]["token"].as_str().unwrap().len() > 8);

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn mcp_stdio_is_protocol_clean() {
    let db_dir = temp_root("mcp");
    let db = db_dir.join("queue.db");
    let mut child = bin()
        .args(["mcp", "--db", db.to_str().unwrap()])
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    use std::io::{BufRead, BufReader, Read, Write};
    let mut reader = BufReader::new(stdout);
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-03-26","capabilities":{{}},"clientInfo":{{"name":"smoke","version":"0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(line.trim()).expect("initialize response");
    assert_eq!(init["result"]["serverInfo"]["name"], "q");
    assert!(!line.contains("INFO"));

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"queue_claim_next","arguments":{{"agent_id":"codex-local-01"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"queue_capture","arguments":{{"status":"ready"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    line.clear();
    reader.read_line(&mut line).unwrap();
    let claim: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(claim["result"]["isError"], false);
    let body: Value =
        serde_json::from_str(claim["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["found"], false);
    assert_eq!(body["reason"], "no_eligible_ready_tasks");

    line.clear();
    reader.read_line(&mut line).unwrap();
    let invalid: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(invalid["error"]["code"], -32602);

    let status = child.wait().unwrap();
    assert!(status.success());
    let mut err_reader = BufReader::new(stderr);
    let mut err = String::new();
    err_reader.read_to_string(&mut err).unwrap();
    assert!(
        err.contains("mcp") || err.contains("opening queue"),
        "{err}"
    );
    let _ = fs::remove_dir_all(db_dir);
}

#[test]
fn delete_removes_the_task_unless_an_active_claim_blocks_it() {
    let db = temp_root("delete").join("queue.db");
    let db_arg = db.to_str().unwrap();
    let captured = run(bin().args(["--db", db_arg, "--json", "Throwaway inbox task"]));
    let created: Value = serde_json::from_slice(&captured.stdout).unwrap();
    let id = created["id"].as_i64().unwrap().to_string();

    let deleted = run(bin().args(["--db", db_arg, "delete", &id]));
    let text = String::from_utf8(deleted.stdout).unwrap();
    assert!(text.contains(&format!("deleted #{id}")), "{text}");
    assert!(text.contains("[inbox]"), "{text}");
    assert!(text.contains("claims="), "{text}");
    assert!(!text.contains("reason:"), "{text}");

    let missing = bin().args(["--db", db_arg, "show", &id]).output().unwrap();
    assert!(!missing.status.success());
    let stderr = String::from_utf8(missing.stderr).unwrap();
    assert!(stderr.contains("not found"), "{stderr}");

    let listed = run(bin().args(["--db", db_arg, "ls"]));
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains("no tasks"), "{listed}");

    let captured = run(bin().args(["--db", db_arg, "--json", "Claimed work"]));
    let created: Value = serde_json::from_slice(&captured.stdout).unwrap();
    let id = created["id"].as_i64().unwrap().to_string();
    run(bin().args(["--db", db_arg, "ready", &id]));
    run(bin().args(["--db", db_arg, "claim", "--agent", "codex-local-01"]));
    let rejected = bin()
        .args(["--db", db_arg, "delete", &id])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let stderr = String::from_utf8(rejected.stderr).unwrap();
    assert!(stderr.contains("active claim"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");

    let forced = run(bin().args(["--db", db_arg, "--json", "delete", &id, "--force"]));
    let body: Value = serde_json::from_slice(&forced.stdout).unwrap();
    assert!(body.get("reason").is_none());
    assert_eq!(body["active_claim_cleared"], true);
    assert!(body["claims_removed"].as_i64().unwrap() >= 1);
    assert_eq!(body["status"], "claimed");
    let missing = bin().args(["--db", db_arg, "show", &id]).output().unwrap();
    assert!(!missing.status.success());
    let _ = fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn skill_prints_frontmatter_and_install_help() {
    let home = temp_root("skill-home");
    let db = temp_root("skill-db").join("queue.db");
    let printed = run(bin().env("HOME", &home).env("RUST_LOG", "info").args([
        "--db",
        db.to_str().unwrap(),
        "skill",
    ]));
    let stdout = String::from_utf8(printed.stdout).unwrap();
    let stderr = String::from_utf8(printed.stderr).unwrap();
    assert!(stdout.contains("name: q"));
    assert!(stdout.contains("q skill install"));
    assert!(stdout.contains("## How to install"));
    assert!(!stderr.contains("name: q"), "{stderr}");
    assert!(!db.exists(), "q skill must not open the queue database");

    let json_out = run(bin().env("HOME", &home).args(["--json", "skill"]));
    let document: Value = serde_json::from_slice(&json_out.stdout).expect("skill json");
    assert!(document["skill"].as_str().unwrap().contains("name: q"));
    assert!(document["install_help"]
        .as_str()
        .unwrap()
        .contains("q skill install"));
    assert_eq!(document["install_targets"].as_array().unwrap().len(), 4);
    assert!(document["install_targets"][0]["path"]
        .as_str()
        .unwrap()
        .contains(".agents/skills/q/SKILL.md"));
}

#[test]
fn skill_install_target_agents_writes_skill_md() {
    let home = temp_root("skill-install");
    let created = run(bin()
        .env("HOME", &home)
        .args(["skill", "install", "--target", "agents"]));
    let stdout = String::from_utf8(created.stdout).unwrap();
    let path = home.join(".agents/skills/q/SKILL.md");
    assert!(stdout.contains("created"), "{stdout}");
    assert!(stdout.contains(&path.display().to_string()), "{stdout}");
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: q"));
    assert!(!home.join(".claude").exists());
    assert!(!home.join(".cursor").exists());
    assert!(!home.join(".codex").exists());

    let updated = run(bin()
        .env("HOME", &home)
        .args(["skill", "install", "--target", "agents", "--force"]));
    let stdout = String::from_utf8(updated.stdout).unwrap();
    assert!(stdout.contains("updated"), "{stdout}");
}

#[test]
fn ls_hides_terminal_tasks_unless_all_or_status_and_prints_a_table() {
    let root = temp_root("ls");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();

    let capture = |title: &str, project: Option<&str>| -> i64 {
        let mut cmd = bin();
        cmd.current_dir(&work)
            .args(["--db", db_arg, "--json", "add", title]);
        if let Some(project) = project {
            cmd.args(["--project", project]);
        }
        let output = run(&mut cmd);
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        if let Some(project) = project {
            assert_eq!(body["project"], project);
        } else {
            assert!(body["project"].is_null(), "{body}");
        }
        body["id"].as_i64().unwrap()
    };

    let visible = capture("Keep visible work", Some("proj-alpha"));
    let long_title = format!("Long {}", "x".repeat(80));
    let _long = capture(&long_title, Some("proj-alpha"));
    let floating = capture("Floating capture", None);
    let cancelled = capture("Drop superseded work", Some("proj-alpha"));
    run(bin()
        .current_dir(&work)
        .args(["--db", db_arg, "cancel", &cancelled.to_string()]));
    let done = capture("Ship finished report", Some("proj-beta"));
    run(bin()
        .current_dir(&work)
        .args(["--db", db_arg, "ready", &done.to_string()]));
    let claimed = run(bin().current_dir(&work).args([
        "--db",
        db_arg,
        "--json",
        "claim",
        "--agent",
        "codex-local-01",
    ]));
    let claimed: Value = serde_json::from_slice(&claimed.stdout).unwrap();
    assert_eq!(claimed["task"]["id"], done);
    let token = claimed["claim"]["token"].as_str().unwrap();
    run(bin().current_dir(&work).args([
        "--db",
        db_arg,
        "start",
        &done.to_string(),
        "--claim-token",
        token,
    ]));
    run(bin().current_dir(&work).args([
        "--db",
        db_arg,
        "complete",
        &done.to_string(),
        "--claim-token",
        token,
        "--summary",
        "shipped",
    ]));

    let listed = run(bin().current_dir(&work).args(["--db", db_arg, "ls"]));
    let table = String::from_utf8(listed.stdout).unwrap();
    assert!(table.contains("Keep visible work"), "{table}");
    assert!(table.contains("Floating capture"), "{table}");
    assert!(table.contains("(none)"), "{table}");
    assert!(table.contains("proj-alpha"), "{table}");
    assert!(!table.contains("Drop superseded work"), "{table}");
    assert!(!table.contains("Ship finished report"), "{table}");
    assert!(!table.contains(&long_title), "{table}");
    assert!(table.contains('…'), "{table}");
    assert_aligned_table(&table);

    let aliased = run(bin().current_dir(&work).args(["--db", db_arg, "list"]));
    let aliased = String::from_utf8(aliased.stdout).unwrap();
    assert_eq!(aliased, table);

    let all = run(bin()
        .current_dir(&work)
        .args(["--db", db_arg, "ls", "--all"]));
    let all = String::from_utf8(all.stdout).unwrap();
    assert!(all.contains("Drop superseded work"), "{all}");
    assert!(all.contains("Ship finished report"), "{all}");
    assert!(all.contains("Keep visible work"), "{all}");
    let alpha_at = all.find("proj-alpha").unwrap();
    let beta_at = all.find("proj-beta").unwrap();
    let floating_at = all.find("Floating capture").unwrap();
    assert!(alpha_at < beta_at && beta_at < floating_at, "{all}");
    assert_aligned_table(&all);

    let only_cancelled =
        run(bin()
            .current_dir(&work)
            .args(["--db", db_arg, "ls", "--status", "cancelled"]));
    let only_cancelled = String::from_utf8(only_cancelled.stdout).unwrap();
    assert!(
        only_cancelled.contains("Drop superseded work"),
        "{only_cancelled}"
    );
    assert!(
        !only_cancelled.contains("Keep visible work"),
        "{only_cancelled}"
    );
    assert!(
        !only_cancelled.contains("Ship finished report"),
        "{only_cancelled}"
    );
    assert!(only_cancelled.contains("cancelled"), "{only_cancelled}");

    let json = run(bin()
        .current_dir(&work)
        .args(["--db", db_arg, "--json", "ls"]));
    let body: Value = serde_json::from_slice(&json.stdout).unwrap();
    let tasks = body["tasks"].as_array().unwrap();
    let ids: Vec<i64> = tasks
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&visible) && ids.contains(&floating), "{body}");
    assert!(!ids.contains(&cancelled) && !ids.contains(&done), "{body}");
    assert!(tasks.iter().any(|task| task["title"] == long_title));
    assert!(tasks
        .iter()
        .all(|task| task["status"] != "done" && task["status"] != "cancelled"));

    let json_all = run(bin()
        .current_dir(&work)
        .args(["--db", db_arg, "--json", "ls", "--all"]));
    let body: Value = serde_json::from_slice(&json_all.stdout).unwrap();
    let ids: Vec<i64> = body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&done) && ids.contains(&cancelled), "{body}");

    let json_cancelled = run(bin().current_dir(&work).args([
        "--db",
        db_arg,
        "--json",
        "list",
        "--status",
        "cancelled",
    ]));
    let body: Value = serde_json::from_slice(&json_cancelled.stdout).unwrap();
    let tasks = body["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], cancelled);
    assert_eq!(tasks[0]["status"], "cancelled");

    let _ = fs::remove_dir_all(&root);
}

fn assert_aligned_table(table: &str) {
    let lines: Vec<&str> = table.lines().filter(|line| !line.is_empty()).collect();
    assert!(lines.len() >= 2, "{table}");
    let header = lines[0];
    for label in [
        "ID", "STATUS", "FEATURE", "PROJECT", "PRI", "UPDATED", "TITLE",
    ] {
        assert!(header.contains(label), "{header}");
    }
    assert!(header.find("ID").unwrap() < header.find("STATUS").unwrap());
    assert!(header.find("STATUS").unwrap() < header.find("FEATURE").unwrap());
    assert!(header.find("FEATURE").unwrap() < header.find("PROJECT").unwrap());
    assert!(header.find("PROJECT").unwrap() < header.find("PRI").unwrap());
    assert!(header.find("PRI").unwrap() < header.find("UPDATED").unwrap());
    assert!(header.find("UPDATED").unwrap() < header.find("TITLE").unwrap());
    let width = header.chars().count();
    assert!(
        lines.iter().all(|line| line.chars().count() == width),
        "{table}"
    );
    let project_at = char_index(header, "PROJECT");
    let updated_at = char_index(header, "UPDATED");
    for line in &lines[1..] {
        assert_eq!(char_index(line, "2026-"), updated_at, "{line}");
        let project = line
            .chars()
            .skip(project_at)
            .take("PROJECT".chars().count())
            .collect::<String>();
        assert!(
            project.starts_with("proj-") || project.starts_with("(none)"),
            "{line}"
        );
    }
}

#[test]
fn features_group_tasks_across_repos_and_resolve_titles() {
    let root = temp_root("feature");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();

    let created = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "feature",
        "create",
        "Cross-repo rollout",
        "--body",
        "Ship the queue across services",
    ]));
    let feature: Value = serde_json::from_slice(&created.stdout).unwrap();
    let feature_id = feature["id"].as_i64().unwrap();
    assert_eq!(feature["title"], "Cross-repo rollout");

    let capture = |title: &str, repo: &str, project: &str, feature: &str| -> i64 {
        let output = run(bin().args([
            "--db",
            db_arg,
            "--json",
            "--repo",
            repo,
            "--project",
            project,
            "add",
            "--feature",
            feature,
            title,
        ]));
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["repo"], repo);
        assert_eq!(body["project"], project);
        assert_eq!(body["feature"], "Cross-repo rollout");
        body["id"].as_i64().unwrap()
    };
    let queue_task = capture(
        "Add the migration",
        "github.com/acme/queue",
        "queue",
        "cross-repo rollout",
    );
    let client_task = capture(
        "Wire the client",
        "github.com/acme/client",
        "client",
        &feature_id.to_string(),
    );
    let loose = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "--repo",
        "github.com/acme/notes",
        "--project",
        "notes",
        "add",
        "Loose note",
    ]));
    let loose: Value = serde_json::from_slice(&loose.stdout).unwrap();
    let loose_id = loose["id"].as_i64().unwrap();
    assert!(loose.get("feature").is_none());

    let listed = run(bin().args(["--db", db_arg, "ls", "--feature", "Cross-repo rollout"]));
    let table = String::from_utf8(listed.stdout).unwrap();
    assert!(table.contains("FEATURE"), "{table}");
    assert!(table.contains("Cross-repo rollout"), "{table}");
    assert!(table.contains("Add the migration"), "{table}");
    assert!(table.contains("Wire the client"), "{table}");
    assert!(!table.contains("Loose note"), "{table}");
    let lines: Vec<&str> = table.lines().filter(|line| !line.is_empty()).collect();
    let width = lines[0].chars().count();
    assert!(
        lines.iter().all(|line| line.chars().count() == width),
        "{table}"
    );

    let json = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "ls",
        "--feature",
        &feature_id.to_string(),
    ]));
    let body: Value = serde_json::from_slice(&json.stdout).unwrap();
    let ids: Vec<i64> = body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![client_task, queue_task]);

    run(bin().args(["--db", db_arg, "cancel", &queue_task.to_string()]));
    let hidden = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "ls",
        "--feature",
        "Cross-repo rollout",
    ]));
    let hidden: Value = serde_json::from_slice(&hidden.stdout).unwrap();
    let ids: Vec<i64> = hidden["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![client_task]);
    let all = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "ls",
        "--all",
        "--feature",
        "Cross-repo rollout",
    ]));
    let all: Value = serde_json::from_slice(&all.stdout).unwrap();
    let ids: Vec<i64> = all["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&queue_task) && ids.contains(&client_task));
    assert!(!ids.contains(&loose_id));

    let duplicate = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "feature",
        "create",
        "Cross-repo rollout",
    ]));
    let duplicate: Value = serde_json::from_slice(&duplicate.stdout).unwrap();
    let duplicate_id = duplicate["id"].as_i64().unwrap();
    let ambiguous = bin()
        .args([
            "--db",
            db_arg,
            "add",
            "--feature",
            "Cross-repo rollout",
            "Should fail",
        ])
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    let stderr = String::from_utf8(ambiguous.stderr).unwrap();
    assert!(stderr.contains("more than one feature"), "{stderr}");

    let edited = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "edit",
        &client_task.to_string(),
        "--feature",
        &duplicate_id.to_string(),
    ]));
    let edited: Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(edited["feature_id"], duplicate_id);

    let cleared = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "edit",
        &client_task.to_string(),
        "--clear-feature",
    ]));
    let cleared: Value = serde_json::from_slice(&cleared.stdout).unwrap();
    assert!(cleared.get("feature").is_none());
    assert!(cleared.get("feature_id").is_none());

    let removed = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "feature",
        "delete",
        &feature_id.to_string(),
    ]));
    let removed: Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert!(removed["tasks_detached"].as_i64().unwrap() >= 1);
    let shown = run(bin().args(["--db", db_arg, "--json", "show", &queue_task.to_string()]));
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert!(shown.get("feature_id").is_none());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tree_prints_dependencies_for_a_task_and_a_feature() {
    let dir = temp_root("tree");
    let db = dir.join("queue.db");
    let db_arg = db.to_str().unwrap();

    run(bin().args(["--db", db_arg, "--json", "feature", "create", "Rollout"]));
    run(bin().args(["--db", db_arg, "--json", "feature", "create", "Other"]));
    run(bin().args(["--db", db_arg, "--json", "feature", "create", "Empty"]));

    let external = add_task(db_arg, "Shared schema", "db", Some("Other"), None);
    let base = add_task(db_arg, "Add the types", "api", Some("Rollout"), None);
    let left = add_task(
        db_arg,
        "Write the schema",
        "api",
        Some("Rollout"),
        Some(&base.to_string()),
    );
    let right = add_task(
        db_arg,
        "Write the client",
        "api",
        Some("Rollout"),
        Some(&base.to_string()),
    );
    let deps = format!("{external},{left},{right}");
    let top = add_task(
        db_arg,
        "Ship the rollout",
        "api",
        Some("Rollout"),
        Some(&deps),
    );
    let _notes = add_task(db_arg, "Write the notes", "web", Some("Rollout"), None);

    let human = run(bin().args(["--db", db_arg, "tree", &top.to_string()]));
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("Ship the rollout"), "{text}");
    assert!(text.contains("├──") && text.contains("└──"), "{text}");
    assert!(text.contains("already shown"), "{text}");
    assert!(text.contains("[api]"), "{text}");
    assert!(!text.contains("(external)"), "{text}");

    let json = run(bin().args(["--db", db_arg, "--json", "tree", &top.to_string()]));
    let body: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(body.get("feature").is_none());
    assert_eq!(body["roots"][0]["id"], top);
    assert_eq!(body["roots"][0]["title"], "Ship the rollout");
    let children = body["roots"][0]["depends_on"].as_array().unwrap();
    assert_eq!(children.len(), 3);
    let left_node = children.iter().find(|node| node["id"] == left).unwrap();
    let right_node = children.iter().find(|node| node["id"] == right).unwrap();
    assert_eq!(left_node["depends_on"][0]["id"], base);
    assert!(left_node["depends_on"][0].get("already_shown").is_none());
    assert_eq!(right_node["depends_on"][0]["id"], base);
    assert_eq!(right_node["depends_on"][0]["already_shown"], true);

    let forest = run(bin().args(["--db", db_arg, "tree", "--feature", "Rollout"]));
    let forest_text = String::from_utf8(forest.stdout).unwrap();
    assert!(forest_text.contains("Ship the rollout"), "{forest_text}");
    assert!(forest_text.contains("Write the notes"), "{forest_text}");
    assert!(forest_text.contains("(external)"), "{forest_text}");
    assert!(forest_text.contains("{Other}"), "{forest_text}");
    assert!(forest_text.contains("already shown"), "{forest_text}");

    let forest_json = run(bin().args(["--db", db_arg, "--json", "tree", "--feature", "rollout"]));
    let forest_body: Value = serde_json::from_slice(&forest_json.stdout).unwrap();
    assert_eq!(forest_body["feature"]["title"], "Rollout");
    let roots = forest_body["roots"].as_array().unwrap();
    assert!(roots.iter().any(|node| node["id"] == top));
    assert!(roots.iter().any(|node| node["title"] == "Write the notes"));
    assert!(!roots.iter().any(|node| node["title"] == "Add the types"));

    let empty = run(bin().args(["--db", db_arg, "tree", "--feature", "Empty"]));
    let empty_text = String::from_utf8(empty.stdout).unwrap();
    assert_eq!(empty_text.trim(), "no tasks in Empty");

    let _ = fs::remove_dir_all(dir);
}

fn add_task(
    db: &str,
    title: &str,
    project: &str,
    feature: Option<&str>,
    depends_on: Option<&str>,
) -> i64 {
    let mut cmd = bin();
    cmd.args(["--db", db, "--json", "--project", project, "add", title]);
    if let Some(feature) = feature {
        cmd.args(["--feature", feature]);
    }
    if let Some(depends_on) = depends_on {
        cmd.args(["--depends-on", depends_on]);
    }
    let output = run(&mut cmd);
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    body["id"].as_i64().unwrap()
}

fn char_index(line: &str, needle: &str) -> usize {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle} in {line}"));
    line[..byte].chars().count()
}
