//! End-to-end tests of the `ctx` binary, the way a user drives it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn ctx(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .current_dir(cwd)
        .env("CTX_HOME", home)
        .env("CTX_TOKEN_FILE", home.with_extension("token"))
        .output()
        .unwrap()
}

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "ctx failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout.clone()).unwrap()
}

#[test]
fn save_log_search_reindex_outside_a_project() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("ctx");
    let cwd = dir.path();

    let out = ok(&ctx(
        &home,
        cwd,
        &[
            "save",
            "Covenant settlement",
            "-k",
            "rejected",
            "-w",
            "too slow",
            "-t",
            "Settlement",
        ],
    ));
    assert!(out.starts_with("saved [c:"), "{out}");
    let out = ok(&ctx(
        &home,
        cwd,
        &[
            "save",
            "Covenant settlement  ",
            "-k",
            "rejected",
            "-w",
            "too slow",
            "-t",
            "settlement",
        ],
    ));
    assert!(out.starts_with("already recorded as "), "{out}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["save", "-", "-k", "question"])
        .current_dir(cwd)
        .env("CTX_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Is the peg fast enough?\r\n")
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    let log = ok(&ctx(&home, cwd, &["log"]));
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.contains("rejected    Covenant settlement"), "{log}");

    let hits = ok(&ctx(&home, cwd, &["search", "peg", "-v"]));
    assert!(hits.contains("Is the peg fast enough?"));
    assert!(!hits.contains("Covenant"));

    let out = ok(&ctx(&home, cwd, &["reindex"]));
    assert!(out.starts_with("reindexed 2 claims from 1 shards"), "{out}");

    let bad = ctx(&home, cwd, &["save", "  \n "]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("empty"));
}

#[test]
fn relay_initialisation_prints_one_time_credentials_and_node_status_is_safe_unpaired() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("ctx");
    let data = dir.path().join("relay");
    let cwd = dir.path();

    let status = ok(&ctx(&home, cwd, &["node", "status"]));
    assert!(status.contains("node is not paired"), "{status}");

    let data_arg = data.to_str().unwrap();
    let initialised = ok(&ctx(&home, cwd, &["relay", "init", "--data", data_arg]));
    assert!(initialised.contains("Bootstrap code"), "{initialised}");
    assert!(initialised.contains("Connector secret"), "{initialised}");
    assert!(
        initialised.contains("Authorization: Bearer"),
        "{initialised}"
    );
    assert!(data.join("relay.db").exists());

    let token = ok(&ctx(
        &home,
        cwd,
        &["relay", "token", "--data", data_arg, "--label", "test"],
    ));
    assert!(token.contains("rcm_"), "{token}");

    let again = ctx(&home, cwd, &["relay", "init", "--data", data_arg]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already initialised"));
}

#[test]
fn init_wires_a_repo_and_packs_follow_the_binding() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("ctx");
    let repo = dir.path().join("Acme API");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "# Acme\n\nRun make test.\n").unwrap();

    let out = ok(&ctx(&home, &repo, &["init", "--agents", "claude-code"]));
    assert!(out.contains("RecurOS: acme-api/code"), "{out}");
    assert!(repo.join(".ctx/config.yaml").exists());
    assert!(repo.join(".mcp.json").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap(),
        "@AGENTS.md\n"
    );
    // Idempotent.
    ok(&ctx(&home, &repo, &["init", "--agents", "claude-code"]));

    ok(&ctx(
        &home,
        &repo,
        &["save", "Never block the event loop", "-k", "constraint"],
    ));
    ok(&ctx(
        &home,
        &repo,
        &["save", "Webhooks retry for 3 days", "--to", "research"],
    ));
    let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(
        agents.starts_with("# Acme\n\nRun make test.\n"),
        "user content kept"
    );
    assert!(
        agents.contains("Never block the event loop"),
        "AGENTS.md refreshed on save"
    );

    // Kind not held by the branch: rejected with a suggestion.
    let bad = ctx(
        &home,
        &repo,
        &["save", "Webhooks are the future", "-k", "claim"],
    );
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--to acme-api/research"));

    let handoff = ok(&ctx(&home, &repo, &["pack", "--for", "handoff"]));
    assert!(handoff.starts_with("# Handoff: acme-api/code"), "{handoff}");
    let research = ok(&ctx(&home, &repo, &["pack", "research"]));
    assert!(research.contains("Webhooks retry for 3 days"));

    let ls = ok(&ctx(&home, &repo, &["branch", "ls"]));
    assert!(ls.contains("* acme-api/code"), "{ls}");
    // The hook is silent when nothing changed, and never fails a session.
    let hook = ctx(&home, &repo, &["hook", "session-start"]);
    assert!(hook.status.success());
    assert!(
        hook.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&hook.stdout)
    );
}

#[test]
fn spec_save_takes_a_file_literally_but_unwraps_a_pasted_fence() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("ctx");
    let cwd = dir.path();

    // A document that *describes* the ctx-spec protocol contains an example
    // fence. Saving it as a file must store the whole document, not the
    // example inside it (our own docs/protocol.md hit this).
    let doc = "# Protocol\n\nModels answer like this:\n\n````ctx-spec\n# Example Spec\n\nnot the real spec\n````\n\nThe end.\n";
    let path = cwd.join("protocol.md");
    std::fs::write(&path, doc).unwrap();
    let out = ok(&ctx(
        &home,
        cwd,
        &[
            "spec",
            "save",
            path.to_str().unwrap(),
            "--name",
            "protocol",
            "--to",
            "idea/research",
        ],
    ));
    assert!(
        out.starts_with("saved `protocol` on idea/research: \"Protocol\""),
        "{out}"
    );
    let shown = ok(&ctx(
        &home,
        cwd,
        &["spec", "show", "protocol", "--branch", "idea/research"],
    ));
    assert_eq!(shown, doc.trim_end().to_owned() + "\n");

    // Piped text is a chat answer, so the fence around it is unwrapped.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["spec", "save", "-", "--to", "idea/research"])
        .current_dir(cwd)
        .env("CTX_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Sure:\n\n````ctx-spec\n# Real Spec\n\nbody\n````\n")
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    let shown = ok(&ctx(
        &home,
        cwd,
        &["spec", "show", "--branch", "idea/research"],
    ));
    assert_eq!(shown, "# Real Spec\n\nbody\n");
}

#[test]
fn rename_carries_an_idea_over_and_retires_the_old_name() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("ctx");
    let cwd = dir.path();

    ok(&ctx(&home, cwd, &["new", "demo idea"]));
    ok(&ctx(
        &home,
        cwd,
        &[
            "save",
            "Use postgres",
            "-k",
            "decision",
            "-w",
            "transactions",
            "-r",
            "src/db.rs",
            "--to",
            "demo-idea/research",
        ],
    ));
    ok(&ctx(
        &home,
        cwd,
        &[
            "save",
            "Latency stays under 100ms",
            "-k",
            "constraint",
            "--to",
            "demo-idea/research",
        ],
    ));

    let out = ok(&ctx(
        &home,
        cwd,
        &["rename", "demo idea", "Better Idea", "--yes"],
    ));
    assert!(
        out.contains("renamed demo-idea to better-idea: 2 claims"),
        "{out}"
    );

    // The context is on the new name, whys and refs intact.
    let listed = ok(&ctx(
        &home,
        cwd,
        &["log", "--branch", "better-idea/research"],
    ));
    assert!(
        listed.contains("Use postgres") && listed.contains("Latency"),
        "{listed}"
    );
    let packed = ok(&ctx(&home, cwd, &["pack", "better-idea/research"]));
    assert!(packed.contains("src/db.rs"), "{packed}");

    // The old name is retired, not erased: nothing visible, still in history.
    let old = ok(&ctx(&home, cwd, &["log", "--branch", "demo-idea/research"]));
    assert!(!old.contains("Use postgres"), "{old}");
    let history = ok(&ctx(
        &home,
        cwd,
        &["log", "--branch", "demo-idea/research", "--all"],
    ));
    assert!(history.contains("Use postgres"), "{history}");

    // `ctx list` shows the new idea and not the retired one.
    let list = ok(&ctx(&home, cwd, &["list"]));
    assert!(list.contains("better-idea/research"), "{list}");
    assert!(!list.contains("demo-idea"), "{list}");
    assert!(ok(&ctx(&home, cwd, &["list", "--all"])).contains("demo-idea"));

    // Renaming onto a name that already holds context is refused.
    let clash = ctx(
        &home,
        cwd,
        &["rename", "better-idea", "better-idea", "--yes"],
    );
    assert!(!clash.status.success());
}
