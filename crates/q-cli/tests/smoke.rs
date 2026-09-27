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
        "--hold",
        "Benchmark delta coding versus varint timestamps",
    ]));
    let created: Value = serde_json::from_slice(&captured.stdout).expect("capture json");
    assert_eq!(created["status"], "held");
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
    assert_eq!(detail["status"], "held");
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
    assert!(
        !ready_err.contains("missing recommended section"),
        "sparse bodies do not warn on ready: {ready_err}"
    );
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
        .args(["--color", "always", "mcp", "--db", db.to_str().unwrap()])
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
    assert!(!line.contains('\u{1b}'), "{line}");

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
    let captured = run(bin().args(["--db", db_arg, "--json", "-w", "Throwaway held task"]));
    let created: Value = serde_json::from_slice(&captured.stdout).unwrap();
    let id = created["id"].as_i64().unwrap().to_string();

    let deleted = run(bin().args(["--db", db_arg, "delete", &id]));
    let text = String::from_utf8(deleted.stdout).unwrap();
    assert!(text.contains(&format!("deleted #{id}")), "{text}");
    assert!(text.contains("[held]"), "{text}");
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
    assert_eq!(created["status"], "ready");
    let id = created["id"].as_i64().unwrap().to_string();
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
fn ready_cancel_reopen_and_delete_accept_several_ids_and_keep_going_on_failure() {
    let db = temp_root("multi").join("queue.db");
    let db_arg = db.to_str().unwrap();
    let first = add_task(db_arg, "First capture", "alpha", None, None);
    let second = add_task(db_arg, "Second capture", "alpha", None, None);
    let third = add_task(db_arg, "Third capture", "alpha", None, None);
    let (first, second, third) = (first.to_string(), second.to_string(), third.to_string());
    // Captures are ready by default; hold them so `q ready` has work to release.
    for id in [&first, &second, &third] {
        run(bin().args(["--db", db_arg, "hold", id]));
    }

    // One id keeps the single-task human line and the single-task JSON shape.
    let single = run(bin().args(["--db", db_arg, "--json", "ready", &first]));
    let body: Value = serde_json::from_slice(&single.stdout).unwrap();
    assert_eq!(body["task"]["id"].as_i64().unwrap().to_string(), first);
    assert_eq!(body["task"]["status"], "ready");
    assert!(body["warnings"].is_array());
    assert!(body.get("results").is_none());

    // Several ids print one confirmation per task, in order.
    let many = run(bin().args(["--db", db_arg, "ready", &second, &third]));
    let text = String::from_utf8(many.stdout).unwrap();
    let second_line = text.find(&format!("ready #{second}")).expect(&text);
    let third_line = text.find(&format!("ready #{third}")).expect(&text);
    assert!(second_line < third_line, "{text}");
    assert_eq!(text.matches("[ready]").count(), 2, "{text}");

    // A failing id is reported and the others still change; exit is non-zero.
    let mixed = bin()
        .args(["--db", db_arg, "cancel", &first, "999", &second])
        .output()
        .unwrap();
    assert!(!mixed.status.success());
    let stdout = String::from_utf8(mixed.stdout).unwrap();
    let stderr = String::from_utf8(mixed.stderr).unwrap();
    assert!(stdout.contains(&format!("cancelled #{first}")), "{stdout}");
    assert!(stdout.contains(&format!("cancelled #{second}")), "{stdout}");
    assert!(stderr.contains("#999"), "{stderr}");
    assert!(stderr.contains("not found"), "{stderr}");
    assert!(stderr.contains("1 of 3 tasks failed"), "{stderr}");
    for id in [&first, &second] {
        let shown = run(bin().args(["--db", db_arg, "show", id, "--json"]));
        let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
        assert_eq!(shown["status"], "cancelled", "task {id}");
    }

    // With --json and several ids, one document lists results and errors.
    let mixed = bin()
        .args(["--db", db_arg, "--json", "reopen", &first, &third, &second])
        .output()
        .unwrap();
    assert!(!mixed.status.success());
    let body: Value = serde_json::from_slice(&mixed.stdout).expect("stdout must be json only");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"].as_i64().unwrap().to_string(), first);
    assert_eq!(results[0]["status"], "held");
    assert_eq!(results[1]["id"].as_i64().unwrap().to_string(), second);
    assert_eq!(results[1]["status"], "held");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["id"].as_i64().unwrap().to_string(), third);
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap()
            .contains("status is ready"),
        "{body}"
    );
    let stderr = String::from_utf8(mixed.stderr).unwrap();
    assert!(stderr.contains("1 of 3 tasks failed"), "{stderr}");

    // All ids succeeding exits zero and reports no errors.
    let all = run(bin().args(["--db", db_arg, "--json", "delete", &first, &second, &third]));
    let body: Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["results"][2]["task_id"].as_i64().unwrap().to_string(),
        third
    );
    assert_eq!(body["errors"].as_array().unwrap().len(), 0);
    let listed = run(bin().args(["--db", db_arg, "ls", "-a"]));
    assert!(String::from_utf8(listed.stdout)
        .unwrap()
        .contains("no tasks"));

    // A single failing id is still a plain error with nothing on stdout.
    let missing = bin()
        .args(["--db", db_arg, "--json", "ready", &first])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    let stderr = String::from_utf8(missing.stderr).unwrap();
    assert!(stderr.contains("not found"), "{stderr}");
    assert!(!stderr.contains("tasks failed"), "{stderr}");
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
    // Every capture is ready, so priority decides which one the claim takes.
    let done = {
        let output = run(bin().current_dir(&work).args([
            "--db",
            db_arg,
            "--json",
            "--project",
            "proj-beta",
            "add",
            "Ship finished report",
            "--priority",
            "9",
        ]));
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        body["id"].as_i64().unwrap()
    };
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
        let updated = &line[byte_at(line, updated_at)..];
        assert!(
            updated.starts_with("just now")
                || updated.contains(" ago")
                || updated.starts_with("in "),
            "{line}"
        );
        assert!(!updated.contains("T"), "{line}");
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

fn byte_at(line: &str, at: usize) -> usize {
    line.char_indices()
        .nth(at)
        .map(|(index, _)| index)
        .unwrap_or(line.len())
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[test]
fn color_flags_leave_json_and_piped_auto_plain() {
    let root = temp_root("color");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let captured = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "--color",
        "always",
        "Capture a colored task",
    ]));
    assert!(!captured.stdout.contains(&0x1b));
    let created: Value = serde_json::from_slice(&captured.stdout).unwrap();
    let id = created["id"].as_i64().unwrap().to_string();
    assert!(created["updated_at"].as_str().unwrap().contains('T'));
    assert_eq!(created["status"], "ready");
    let cancelled = run(bin().args(["--db", db_arg, "--json", "add", "Cancelled row"]));
    let cancelled: Value = serde_json::from_slice(&cancelled.stdout).unwrap();
    let cancelled_id = cancelled["id"].as_i64().unwrap().to_string();
    run(bin().args(["--db", db_arg, "cancel", &cancelled_id]));

    let plain = run(bin()
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .args(["--db", db_arg, "--color", "never", "ls", "--all"]));
    let plain_text = String::from_utf8(plain.stdout.clone()).unwrap();
    assert!(!plain_text.contains('\u{1b}'));
    assert!(plain_text.contains("just now") || plain_text.contains(" ago"));
    assert!(plain_text.contains("ready"));
    assert!(plain_text.contains("cancelled"));

    let forced = run(bin()
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .args(["--db", db_arg, "--color", "auto", "ls", "--all"]));
    let forced_text = String::from_utf8(forced.stdout).unwrap();
    assert!(forced_text.contains('\u{1b}'), "{forced_text}");
    assert_eq!(strip_ansi(&forced_text), plain_text);
    assert!(forced_text.contains("32"), "ready should be green");
    assert!(
        forced_text.contains('9'),
        "cancelled should be struck through"
    );

    let no_color = run(bin()
        .env("NO_COLOR", "1")
        .env("CLICOLOR_FORCE", "1")
        .args(["--db", db_arg, "--color", "auto", "ls", "--all"]));
    assert!(!String::from_utf8(no_color.stdout)
        .unwrap()
        .contains('\u{1b}'));

    let empty_no_color = run(bin()
        .env("NO_COLOR", "")
        .env("CLICOLOR_FORCE", "1")
        .env_remove("CLICOLOR")
        .args(["--db", db_arg, "--color", "auto", "ls", "--all"]));
    assert!(
        String::from_utf8(empty_no_color.stdout)
            .unwrap()
            .contains('\u{1b}'),
        "an empty NO_COLOR must not disable color"
    );

    let always = run(bin()
        .env("NO_COLOR", "1")
        .args(["--db", db_arg, "--color", "always", "ls", "--all"]));
    let always_text = String::from_utf8(always.stdout).unwrap();
    assert!(always_text.contains('\u{1b}'));
    assert_eq!(strip_ansi(&always_text), plain_text);

    let auto_pipe = run(bin()
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .args(["--db", db_arg, "ls", "--all"]));
    assert!(!auto_pipe.stdout.contains(&0x1b));

    let show_plain = run(bin().args(["--db", db_arg, "--color", "never", "show", &id]));
    let show_plain = String::from_utf8(show_plain.stdout).unwrap();
    assert!(show_plain.contains("created_at: "));
    assert!(
        show_plain.contains('T') && show_plain.contains('Z'),
        "{show_plain}"
    );
    assert!(!show_plain.contains(" ago"), "{show_plain}");
    assert!(show_plain.contains("events:"), "{show_plain}");

    let show_color = run(bin().args(["--db", db_arg, "--color", "always", "show", &id]));
    let show_color = String::from_utf8(show_color.stdout).unwrap();
    assert!(show_color.contains('\u{1b}'));
    assert_eq!(strip_ansi(&show_color), show_plain);

    let tree_plain = run(bin().args(["--db", db_arg, "--color", "never", "tree", &id]));
    let tree_color = run(bin().args(["--db", db_arg, "--color", "always", "tree", &id]));
    let tree_plain = String::from_utf8(tree_plain.stdout).unwrap();
    let tree_color = String::from_utf8(tree_color.stdout).unwrap();
    assert!(tree_color.contains('\u{1b}'));
    assert!(tree_color.contains("├──") || tree_color.contains('#') || tree_plain.contains('#'));
    assert_eq!(strip_ansi(&tree_color), tree_plain);

    let json_always =
        run(bin().args(["--db", db_arg, "--color", "always", "--json", "ls", "--all"]));
    let json_never = run(bin().args(["--db", db_arg, "--color", "never", "--json", "ls", "--all"]));
    assert_eq!(json_always.stdout, json_never.stdout);
    assert!(!json_always.stdout.contains(&0x1b));
    let body: Value = serde_json::from_slice(&json_always.stdout).unwrap();
    assert!(body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["updated_at"].as_str().unwrap().contains('T')));

    let show_json_always =
        run(bin().args(["--db", db_arg, "--color", "always", "--json", "show", &id]));
    let show_json_never =
        run(bin().args(["--db", db_arg, "--color", "never", "--json", "show", &id]));
    assert_eq!(show_json_always.stdout, show_json_never.stdout);
    assert!(!show_json_always.stdout.contains(&0x1b));

    let tree_json_always =
        run(bin().args(["--db", db_arg, "--color", "always", "--json", "tree", &id]));
    let tree_json_never =
        run(bin().args(["--db", db_arg, "--color", "never", "--json", "tree", &id]));
    assert_eq!(tree_json_always.stdout, tree_json_never.stdout);
    assert!(!tree_json_always.stdout.contains(&0x1b));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn add_edit_opens_editor_on_template_or_seed() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root("editor");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let seen = root.join("seen.md");
    let editor = root.join("editor.sh");
    fs::write(
        &editor,
        format!(
            "#!/bin/sh\ncp \"$1\" '{}'\nprintf '\\n## Notes\\n\\n- from editor\\n' >> \"$1\"\n",
            seen.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let editor_arg = editor.to_str().unwrap();

    // Bare title plus -e goes through the add shorthand and seeds the template.
    let added = run(bin().env("VISUAL", editor_arg).env_remove("EDITOR").args([
        "--db",
        db_arg,
        "--json",
        "-e",
        "Write the parser",
    ]));
    let task: Value = serde_json::from_slice(&added.stdout).unwrap();
    let body = task["body"].as_str().unwrap();
    assert!(body.starts_with("## Goal\n"), "template seed, got {body:?}");
    assert!(body.contains("## Acceptance criteria\n"));
    assert!(
        body.ends_with("## Notes\n\n- from editor"),
        "stored bodies are trimmed"
    );
    let opened = fs::read_to_string(&seen).unwrap();
    assert_eq!(opened, q_core::body_template());

    // --body seeds the editor instead of the template.
    let seeded = run(bin().env("EDITOR", editor_arg).args([
        "--db",
        db_arg,
        "--json",
        "add",
        "--edit",
        "--body",
        "## Goal\n\nShip it\n",
        "Seeded capture",
    ]));
    let task: Value = serde_json::from_slice(&seeded.stdout).unwrap();
    assert_eq!(
        task["body"],
        "## Goal\n\nShip it\n\n## Notes\n\n- from editor"
    );
    let id = task["id"].as_i64().unwrap();

    // q edit -e opens the current body even when other flags are set.
    let edited = run(bin().env("EDITOR", editor_arg).args([
        "--db",
        db_arg,
        "--json",
        "edit",
        &id.to_string(),
        "--priority",
        "3",
        "-e",
    ]));
    let task: Value = serde_json::from_slice(&edited.stdout).unwrap();
    assert_eq!(task["priority"], 3);
    assert_eq!(
        task["body"],
        "## Goal\n\nShip it\n\n## Notes\n\n- from editor\n## Notes\n\n- from editor"
    );

    // Without an editor, --edit is an error rather than a silent capture.
    let output = bin()
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .args(["--db", db_arg, "-e", "No editor"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("$VISUAL or $EDITOR"), "{stderr}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn top_once_prints_counts_table_and_changes() {
    let root = temp_root("top");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let _first = add_task(db_arg, "Watch me", "alpha", None, None);

    let frame = run(bin().args(["--db", db_arg, "top", "--once"]));
    let text = String::from_utf8(frame.stdout).unwrap();
    assert!(text.starts_with("q top database: "), "{text}");
    assert!(text.contains("every 2.0s"), "{text}");
    assert!(text.contains("held 0  ready 1  claimed 0"), "{text}");
    assert!(text.contains("claims 0 active, 0 expired"), "{text}");
    assert!(
        text.contains("ID  STATUS  FEATURE  PROJECT  PRI  PROG  UPDATED   PR  TITLE"),
        "{text}"
    );
    assert!(text.contains("Watch me"), "{text}");
    assert!(text.contains("recent changes\n  none yet"), "{text}");
    assert!(
        !text.contains("\x1b["),
        "no escapes without a terminal: {text:?}"
    );

    let json = bin()
        .args(["--db", db_arg, "--json", "top", "--once"])
        .output()
        .unwrap();
    assert!(!json.status.success());
    assert!(String::from_utf8_lossy(&json.stderr).contains("q ls --json"));

    let fast = bin()
        .args(["--db", db_arg, "top", "--once", "-i", "0"])
        .output()
        .unwrap();
    assert!(!fast.status.success());
    assert!(String::from_utf8_lossy(&fast.stderr).contains("--interval"));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn top_loop_reports_additions_completions_and_deletions_until_interrupted() {
    use std::io::Read;
    use std::time::Duration;

    let root = temp_root("top-loop");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let first = add_task(db_arg, "Finish me", "alpha", None, None);
    let second = add_task(db_arg, "Drop me", "alpha", None, None);
    let claimed = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot"]));
    let claim: Value = serde_json::from_slice(&claimed.stdout).unwrap();
    let token = claim["claim"]["token"].as_str().unwrap().to_string();

    let mut top = bin()
        .args(["--db", db_arg, "top", "-i", "0.1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    add_task(db_arg, "New arrival", "alpha", None, None);
    run(bin().args([
        "--db",
        db_arg,
        "log",
        &first.to_string(),
        "--claim-token",
        &token,
        "--progress",
        "60",
    ]));
    std::thread::sleep(Duration::from_millis(300));
    run(bin().args([
        "--db",
        db_arg,
        "complete",
        &first.to_string(),
        "--claim-token",
        &token,
        "--summary",
        "done",
    ]));
    run(bin().args(["--db", db_arg, "delete", &second.to_string()]));
    std::thread::sleep(Duration::from_millis(400));

    let status = Command::new("kill")
        .args(["-INT", &top.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = top.wait().unwrap();
    assert!(exit.success(), "top should exit 0 on Ctrl-C: {exit:?}");
    let mut text = String::new();
    top.stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let last = text.rsplit("q top database: ").next().unwrap();
    let change_lines: Vec<&str> = last
        .split("recent changes\n")
        .nth(1)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(change_lines.len(), 4, "{last}");
    assert!(
        change_lines
            .iter()
            .any(|line| line.contains("#1  claimed  -> claimed 60%  Finish me")),
        "{last}"
    );
    assert!(
        change_lines
            .iter()
            .any(|line| line.contains("#3  new      -> ready        New arrival")),
        "{last}"
    );
    assert!(
        change_lines
            .iter()
            .any(|line| line.contains("#1  claimed  -> done         Finish me")),
        "{last}"
    );
    assert!(
        change_lines
            .iter()
            .any(|line| line.contains("#2  ready    -> deleted      Drop me")),
        "{last}"
    );
    let title_columns: std::collections::HashSet<usize> = change_lines
        .iter()
        .map(|line| {
            let title = ["New arrival", "Finish me", "Drop me"]
                .into_iter()
                .find(|title| line.contains(title))
                .unwrap();
            line.find(title).unwrap()
        })
        .collect();
    assert_eq!(title_columns.len(), 1, "titles share a column: {last}");
    let table = last.split("recent changes").next().unwrap();
    assert!(
        !table.contains("Finish me"),
        "done task leaves the table: {last}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn log_and_artifact_commands_keep_a_per_task_log() {
    let root = temp_root("log");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    // Captures are ready by default, so the task is claimable at once.
    let id = add_task(db_arg, "Write the report", "alpha", None, None);
    let claimed = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot-9"]));
    let claim: Value = serde_json::from_slice(&claimed.stdout).unwrap();
    let token = claim["claim"]["token"].as_str().unwrap().to_string();
    let id_arg = id.to_string();

    let wrong = bin()
        .args([
            "--db",
            db_arg,
            "log",
            &id_arg,
            "hi",
            "--claim-token",
            "nope",
        ])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("claim token"));

    let noted = run(bin().args([
        "--db",
        db_arg,
        "log",
        &id_arg,
        "Reading the encoder",
        "--claim-token",
        &token,
    ]));
    let noted = String::from_utf8(noted.stdout).unwrap();
    assert!(
        noted.starts_with(&format!("logged #{id} task_note Reading the encoder")),
        "{noted}"
    );

    let report = root.join("report.md");
    fs::write(&report, "# Findings\n\nfine\n").unwrap();
    run(bin().args([
        "--db",
        db_arg,
        "log",
        &id_arg,
        "--claim-token",
        &token,
        "--attach",
        report.to_str().unwrap(),
        "--artifact",
        "pr=https://example.com/pr/1",
    ]));

    // Humans annotate without a token.
    run(bin()
        .env("USER", "reviewer")
        .args(["--db", db_arg, "log", &id_arg, "looks right"]));

    // Progress is a percent stored on the task and shown in the tables.
    let progressed = run(bin().args([
        "--db",
        db_arg,
        "log",
        &id_arg,
        "--progress",
        "40",
        "--claim-token",
        &token,
    ]));
    assert!(String::from_utf8(progressed.stdout)
        .unwrap()
        .contains("task_note progress 40%"));
    let rejected = bin()
        .args(["--db", db_arg, "log", &id_arg, "--progress", "101"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let listed = run(bin().args(["--db", db_arg, "ls"]));
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains("PRI  PROG  UPDATED"), "{listed}");
    assert!(listed.contains("  40%  "), "{listed}");
    let shown_progress = run(bin().args(["--db", db_arg, "show", &id_arg]));
    assert!(String::from_utf8(shown_progress.stdout)
        .unwrap()
        .contains("progress: 40%"));

    let log = run(bin().args(["--db", db_arg, "log", &id_arg]));
    let log = String::from_utf8(log.stdout).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("task_created"), "{log}");
    assert!(
        lines.iter().any(|line| line.contains("task_claimed")
            && line.contains("agent:bot-9")
            && line.contains("ready -> claimed")),
        "{log}"
    );
    assert!(
        lines.iter().any(|line| line.contains("task_note")
            && line.contains("agent:bot-9")
            && line.contains("Reading the encoder")),
        "{log}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("artifact_added")
                && line.contains("pr: https://example.com/pr/1")),
        "{log}"
    );
    assert!(
        lines.iter().any(|line| line.contains("task_note")
            && line.contains("human:reviewer")
            && line.contains("looks right")),
        "{log}"
    );
    assert!(
        lines.iter().any(|line| line.contains("task_note")
            && line.contains("agent:bot-9")
            && line.contains("progress 40%")),
        "{log}"
    );
    let detail_columns: std::collections::HashSet<usize> = lines
        .iter()
        .map(|line| {
            line.find("  human:")
                .or_else(|| line.find("  agent:"))
                .unwrap()
        })
        .collect();
    assert_eq!(detail_columns.len(), 1, "actor column is aligned: {log}");

    let shown = run(bin().args(["--db", db_arg, "show", &id_arg]));
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains("report: "), "{shown}");
    assert!(shown.contains("bytes stored, q artifact "), "{shown}");
    let json = run(bin().args(["--db", db_arg, "--json", "show", &id_arg]));
    let detail: Value = serde_json::from_slice(&json.stdout).unwrap();
    let stored = detail["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "report")
        .unwrap();
    assert_eq!(stored["content_bytes"], 17);
    let reference = detail["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "pr")
        .unwrap();
    assert!(reference.get("content_bytes").is_none());

    let artifact_id = stored["id"].as_i64().unwrap().to_string();
    let content = run(bin().args(["--db", db_arg, "artifact", &artifact_id]));
    assert_eq!(
        String::from_utf8(content.stdout).unwrap(),
        "# Findings\n\nfine\n"
    );
    let reference_id = reference["id"].as_i64().unwrap().to_string();
    let plain = run(bin().args(["--db", db_arg, "artifact", &reference_id]));
    assert!(String::from_utf8(plain.stdout)
        .unwrap()
        .contains("reference only"));

    let events = run(bin().args(["--db", db_arg, "--json", "events", &id_arg]));
    let events: Value = serde_json::from_slice(&events.stdout).unwrap();
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["event_type"] == "task_note"));
}

#[test]
fn capture_confirmation_is_a_blank_line_then_one_truncated_line() {
    let root = temp_root("capture-confirm");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let long_title = format!("Line one\nline two {}", "z".repeat(80));
    let captured = run(bin().args(["--db", db_arg, "--color", "never", &long_title]));
    let text = String::from_utf8(captured.stdout).unwrap();
    assert!(text.starts_with('\n'), "{text:?}");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert_eq!(lines[0], "", "{text:?}");
    assert!(lines[1].starts_with("captured #"), "{text:?}");
    assert!(lines[1].contains("[ready] Line one line two z"), "{text:?}");
    assert!(lines[1].ends_with('…'), "{text:?}");
    assert!(!lines[1].contains(&"z".repeat(80)), "{text:?}");

    let short = run(bin().args(["--db", db_arg, "--color", "never", "add", "Short title"]));
    let short = String::from_utf8(short.stdout).unwrap();
    let lines: Vec<&str> = short.lines().collect();
    assert_eq!(lines.len(), 2, "{short:?}");
    assert!(lines[1].ends_with("[ready] Short title"), "{short:?}");

    // --json is unchanged: the full title, no confirmation line.
    let json = run(bin().args(["--db", db_arg, "--json", "add", &long_title]));
    let created: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(created["title"].as_str().unwrap(), long_title);
}

#[test]
fn hold_keeps_work_out_of_the_pool_until_ready() {
    let root = temp_root("hold");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();

    let held = run(bin().args(["--db", db_arg, "--hold", "Risky migration"]));
    let held_text = String::from_utf8(held.stdout).unwrap();
    assert!(
        held_text.contains("captured #1 [held] Risky migration"),
        "{held_text}"
    );

    let open = run(bin().args(["--db", db_arg, "Safe cleanup"]));
    let open_text = String::from_utf8(open.stdout).unwrap();
    assert!(
        open_text.contains("captured #2 [ready] Safe cleanup"),
        "{open_text}"
    );

    // Only the ready task can be claimed.
    let claimed = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot"]));
    let claim: Value = serde_json::from_slice(&claimed.stdout).unwrap();
    assert_eq!(claim["task"]["id"], 2);
    let none = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot-2"]));
    let none: Value = serde_json::from_slice(&none.stdout).unwrap();
    assert_eq!(none["found"], false);

    let listed = run(bin().args(["--db", db_arg, "--json", "ls", "--status", "held"]));
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tasks"][0]["status"], "held");
    let status = run(bin().args(["--db", db_arg, "--json", "status"]));
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["counts"]["held"], 1);

    // ready releases held work; hold takes ready work back.
    let released = run(bin().args(["--db", db_arg, "ready", "1"]));
    let released = String::from_utf8(released.stdout).unwrap();
    assert!(
        released.contains("ready #1 [ready] Risky migration"),
        "{released}"
    );
    let back = run(bin().args(["--db", db_arg, "hold", "1"]));
    let back = String::from_utf8(back.stdout).unwrap();
    assert!(back.contains("held #1 [held] Risky migration"), "{back}");
    let twice = bin().args(["--db", db_arg, "hold", "1"]).output().unwrap();
    assert!(!twice.status.success());

    // Cancelled work reopens to held for another look.
    run(bin().args(["--db", db_arg, "cancel", "1"]));
    let reopened = run(bin().args(["--db", db_arg, "--json", "reopen", "1"]));
    let reopened: Value = serde_json::from_slice(&reopened.stdout).unwrap();
    assert_eq!(reopened["status"], "held");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn pr_artifacts_show_as_a_link_in_the_table() {
    let root = temp_root("pr-link");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let shipped = add_task(db_arg, "Shipped work", "alpha", None, None);
    let pending = add_task(db_arg, "Pending work", "alpha", None, None);
    let claimed = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot"]));
    let claim: Value = serde_json::from_slice(&claimed.stdout).unwrap();
    let claimed_id = claim["task"]["id"].as_i64().unwrap();
    let token = claim["claim"]["token"].as_str().unwrap().to_string();
    let url = "https://github.com/acme/q/pull/9";
    run(bin().args([
        "--db",
        db_arg,
        "complete",
        &claimed_id.to_string(),
        "--claim-token",
        &token,
        "--summary",
        "shipped",
        "--artifact",
        &format!("pr={url}"),
    ]));
    let other = if claimed_id == shipped {
        pending
    } else {
        shipped
    };

    let json = run(bin().args(["--db", db_arg, "--json", "ls", "--all"]));
    let listed: Value = serde_json::from_slice(&json.stdout).unwrap();
    let tasks = listed["tasks"].as_array().unwrap();
    let done = tasks.iter().find(|t| t["id"] == claimed_id).unwrap();
    assert_eq!(done["pr_url"], url);
    let open = tasks.iter().find(|t| t["id"] == other).unwrap();
    assert!(open.get("pr_url").is_none());

    // Piped output keeps the address; a terminal gets a clickable PR label.
    let plain = run(bin().args(["--db", db_arg, "ls", "--all"]));
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("UPDATED   PR"), "{plain}");
    assert!(plain.contains(url), "{plain}");
    let color = run(bin().args(["--db", db_arg, "--color", "always", "ls", "--all"]));
    let color = String::from_utf8(color.stdout).unwrap();
    assert!(color.contains(&format!("\x1b]8;;{url}\x1b\\")), "{color:?}");
    let top = run(bin().args(["--db", db_arg, "--color", "always", "top", "--once", "-a"]));
    let top = String::from_utf8(top.stdout).unwrap();
    assert!(top.contains(&format!("\x1b]8;;{url}")), "{top:?}");
    let shown = run(bin().args([
        "--db",
        db_arg,
        "--color",
        "always",
        "show",
        &claimed_id.to_string(),
    ]));
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains(&format!("\x1b]8;;{url}")), "{shown:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn claim_and_start_print_the_progress_command() {
    let root = temp_root("progress-hint");
    let db = root.join("queue.db");
    let db_arg = db.to_str().unwrap();
    let id = add_task(db_arg, "Nudge me", "alpha", None, None);
    let claimed = run(bin().args(["--db", db_arg, "claim", "--agent", "bot"]));
    let claimed = String::from_utf8(claimed.stdout).unwrap();
    let token = claimed
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect(&claimed)
        .to_string();
    // The hint targets the same queue (--db here) and spells the token out
    // only on claim, where the token line is printed anyway.
    let hint = format!(
        "report progress: q --db {db_arg} log {id} --progress <0-100> --claim-token {token}"
    );
    assert!(claimed.contains(&hint), "{claimed}");
    let started = run(bin().args([
        "--db",
        db_arg,
        "start",
        &id.to_string(),
        "--claim-token",
        &token,
    ]));
    let started = String::from_utf8(started.stdout).unwrap();
    assert!(
        started.contains(&format!(
            "report progress: q --db {db_arg} log {id} --progress <0-100> --claim-token <token from q claim>"
        )),
        "{started}"
    );
    assert!(
        !started.contains(&token),
        "start must not echo the token: {started}"
    );

    // --json claim and start stay single JSON documents with no hint line.
    let second = add_task(db_arg, "Nudge me too", "alpha", None, None);
    let json_claim = run(bin().args(["--db", db_arg, "--json", "claim", "--agent", "bot-2"]));
    let claim: Value = serde_json::from_slice(&json_claim.stdout).expect("claim json only");
    assert_eq!(claim["task"]["id"], second);
    let second_token = claim["claim"]["token"].as_str().unwrap().to_string();
    let json_start = run(bin().args([
        "--db",
        db_arg,
        "--json",
        "start",
        &second.to_string(),
        "--claim-token",
        &second_token,
    ]));
    let started: Value = serde_json::from_slice(&json_start.stdout).expect("start json only");
    assert_eq!(started["status"], "in_progress");
    let _ = fs::remove_dir_all(root);
}
