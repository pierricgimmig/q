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

    let not_ready = bin()
        .args(["--db", db.to_str().unwrap(), "ready", &id.to_string()])
        .output()
        .unwrap();
    assert!(!not_ready.status.success());
    let ready_err = String::from_utf8_lossy(&not_ready.stderr);
    assert!(ready_err.contains("Goal"), "{ready_err}");
    assert!(ready_err.contains("force"), "{ready_err}");
    let still = run(bin().args([
        "--db",
        db.to_str().unwrap(),
        "show",
        &id.to_string(),
        "--json",
    ]));
    let still: Value = serde_json::from_slice(&still.stdout).unwrap();
    assert_eq!(still["status"], "inbox");

    run(bin().args([
        "--db",
        db.to_str().unwrap(),
        "ready",
        &id.to_string(),
        "--force",
    ]));
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
