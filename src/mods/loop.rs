//! Ported wholesale from gray-loop. Entry points renamed: manifest/handle/main -> *_loop.
//! gray-loop — self-driving iteration loops on plain wire primitives.
//!
//! `/loop <goal>` seeds
//! `~/.gray/loop/<session>/state.json` and returns a `{prompt}` that puts
//! the agent to work. On every `agent/before_start` while the state reads
//! `running` and `iter < max`, the hook bumps `iter` and appends a
//! "continue" user message — so the loop terminates at `max` even if the
//! model never touches the file.
//!
//! Completion is deterministic when tasks exist: `loop_task pass` flips
//! `passes` flags in state, and the plugin ends the loop itself once every
//! task passes. A `--gate <cmd>` verify command runs detached (tool calls
//! answer in milliseconds; the gate resolves on the next iteration or the
//! next loop_done poll) — a failing gate rejects the "done" and the loop
//! keeps going. With no tasks the `loop_done` tool ends the loop early,
//! still gated.
//!
//! `/loop stop` forces `failed`, `/loop max N` retunes the cap,
//! `/loop status` reports, `/loop tasks` renders the checklist,
//! `/loop steer <msg>` queues steering for the next iteration,
//! `/loop list` shows every session's loop.
//!
//! Fail open everywhere: malformed state, missing files, bad args — all
//! produce `{}` or a text note, never a crash.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

const DEFAULT_MAX: u64 = 10;
const MAX_LEARNINGS: usize = 50;
const GATE_TIMEOUT_SECS: u64 = 180;
const GATE_OUT_TAIL: usize = 3000;

pub fn manifest_loop() -> Value {
    json!({
        "name": "loop",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [
            {
                "name": "loop_done",
                "description": "Stop the active /loop when the goal is verifiably complete (status \"done\") or unachievable (status \"failed\"). If the loop has a gate command, \"done\" arms it: it runs in the background and is re-checked on every iteration and every loop_done call — only exit 0 ends the loop. Only call when a loop is running.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "enum": ["done", "failed"], "description": "Final loop status." },
                        "reason": { "type": "string", "description": "Why the loop is ending." }
                    },
                    "required": ["status"]
                }
            },
            {
                "name": "loop_task",
                "description": "Manage the active /loop's task checklist — the deterministic exit oracle. op \"add\" appends a task (needs text). op \"pass\"/\"fail\" set a task's passes flag (needs task: 1-based id, exact text, unique prefix, or unique substring). op \"list\" shows the checklist. When every task passes the loop ends itself.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["add", "pass", "fail", "list"], "description": "Checklist operation." },
                        "task": { "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "string" }], "description": "For pass/fail: 1-based task id, exact text, unique prefix, or unique substring." },
                        "text": { "type": "string", "description": "For add: the task description (one checkable unit of work)." }
                    },
                    "required": ["op"]
                }
            },
            {
                "name": "loop_learn",
                "description": "Append one durable learning to the active /loop's log — something a future iteration must know (a decision made, a trap hit, a path that worked). Appended, never overwritten; last entries ride every continue prompt.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The learning to append." }
                    },
                    "required": ["text"]
                }
            }
        ],
        "commands": ["/loop"],
        "hooks": ["agent/before_start"],
    })
}

// ── state ─────────────────────────────────────────────────────

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from(".gray"))
}

fn state_path(session_id: &str) -> PathBuf {
    // session ids are uuids, but sanitize anyway — never trust a filename
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    gray_home().join("loop").join(safe).join("state.json")
}

fn read_state(session_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(state_path(session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(session_id: &str, state: &Value) -> std::io::Result<()> {
    let path = state_path(session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, &path).or_else(|_| std::fs::write(&path, serde_json::to_string_pretty(state)?))
}

fn set_status(session_id: &str, status: &str, note: &str) -> bool {
    let Some(mut st) = read_state(session_id) else { return false };
    st["status"] = json!(status);
    if !note.is_empty() {
        st["notes"] = json!(note);
    }
    write_state(session_id, &st).is_ok()
}

fn session_id(params: &Value) -> String {
    params
        .get("session")
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn session_cwd(params: &Value) -> String {
    params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string()
}

fn tasks(st: &Value) -> Vec<Value> {
    st.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn render_tasks(ts: &[Value]) -> String {
    if ts.is_empty() {
        return "no tasks yet — add checkable units of work with loop_task add".into();
    }
    let done = ts.iter().filter(|t| t["passes"].as_bool() == Some(true)).count();
    let mut lines = vec![format!("{done}/{} done:", ts.len())];
    let next_unmarked = ts.iter().position(|t| t["passes"].as_bool() != Some(true));
    for (i, t) in ts.iter().enumerate().take(15) {
        let mark = if t["passes"].as_bool() == Some(true) { "x" } else { " " };
        let arrow = if Some(i) == next_unmarked { "  <- next" } else { "" };
        lines.push(format!("  [{}] {}. {}{}", mark, i + 1, t["text"].as_str().unwrap_or("?"), arrow));
    }
    if ts.len() > 15 {
        lines.push(format!("  … {} more", ts.len() - 15));
    }
    lines.join("\n")
}

// ── gate ──────────────────────────────────────────────────────

fn gate_files(sid: &str) -> (PathBuf, PathBuf) {
    let dir = state_path(sid).parent().map(PathBuf::from).unwrap_or_default();
    (dir.join("gate.out"), dir.join("gate.code"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gate_tail(sid: &str) -> String {
    let (out_p, _) = gate_files(sid);
    let bytes = std::fs::read(&out_p).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let t = text.trim();
    if t.is_empty() {
        return "(no output)".into();
    }
    t.chars()
        .rev()
        .take(GATE_OUT_TAIL)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn gate_kill(pid: u64) {
    #[cfg(unix)]
    {
        let p = pid.to_string();
        let _ = std::process::Command::new("pkill")
            .args(["-KILL", "-P", &p])
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-9", &p])
            .status();
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .status();
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

fn gate_clear(st: &mut Value) {
    st.as_object_mut().map(|o| o.remove("gate_run"));
}

fn gate_arm(sid: &str, st: &mut Value, cmd: &str, cwd: &str) -> Result<(), String> {
    let (out_p, code_p) = gate_files(sid);
    let out = std::fs::File::create(&out_p).map_err(|e| format!("gate: {e}"))?;
    let err = out.try_clone().map_err(|e| format!("gate: {e}"))?;
    let _ = std::fs::remove_file(&code_p);
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg("( eval \"$GATE_CMD\" ); printf %s $? > \"$GATE_CODE\"")
        .env("GATE_CMD", cmd)
        .env("GATE_CODE", &code_p)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("couldn't start gate command (needs `sh` on PATH): {e}"))?;
    st["gate_run"] = json!({ "pid": child.id(), "at": now_secs(), "cmd": cmd });
    write_state(sid, st).map_err(|e| format!("gate: state write failed: {e}"))
}

enum GatePoll {
    Running(u64),
    Finished(i64),
    TimedOut(u64),
    Absent,
}

#[cfg(unix)]
fn gate_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn gate_alive(_pid: u64) -> bool {
    true
}

fn gate_poll(sid: &str, st: &mut Value) -> GatePoll {
    let Some(run) = st.get("gate_run").cloned() else {
        return GatePoll::Absent;
    };
    let pid = run.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let at = run.get("at").and_then(Value::as_u64).unwrap_or(0);
    let (_, code_p) = gate_files(sid);
    if let Ok(code) = std::fs::read_to_string(&code_p) {
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::Finished(code.trim().parse().unwrap_or(-1));
    }
    let elapsed = now_secs().saturating_sub(at);
    if elapsed >= GATE_TIMEOUT_SECS {
        gate_kill(pid);
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::TimedOut(elapsed);
    }
    if !gate_alive(pid) {
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::Finished(-1);
    }
    GatePoll::Running(elapsed)
}

enum DoneCheck {
    Open,
    Verifying(String),
    Rejected(String),
    Complete(String),
}

fn resolve_done(sid: &str, cwd: &str, basis: &str) -> DoneCheck {
    let Some(mut st) = read_state(sid) else {
        return DoneCheck::Rejected("couldn't read loop state".into());
    };
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return DoneCheck::Rejected("loop is not running".into());
    }
    let Some(gate) = st
        .get("gate")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
    else {
        set_status(sid, "done", basis);
        return DoneCheck::Complete(format!("loop complete: {basis}"));
    };
    match gate_poll(sid, &mut st) {
        GatePoll::Absent => match gate_arm(sid, &mut st, &gate, cwd) {
            Ok(()) => DoneCheck::Verifying(format!(
                "verifying — gate `{gate}` running in background; it resolves on the next iteration or your next loop_done call"
            )),
            Err(e) => DoneCheck::Rejected(format!("{e} — \"done\" rejected, loop continues")),
        },
        GatePoll::Running(secs) => DoneCheck::Verifying(format!(
            "gate `{gate}` still verifying ({secs}s of {GATE_TIMEOUT_SECS}s) — resolves on the next iteration or your next loop_done call"
        )),
        GatePoll::TimedOut(secs) => DoneCheck::Rejected(format!(
            "gate `{gate}` timed out after {secs}s — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
        GatePoll::Finished(0) => {
            set_status(sid, "done", &format!("{basis}; gate `{gate}` green"));
            DoneCheck::Complete(format!("loop complete: {basis}, gate green"))
        }
        GatePoll::Finished(code) if code < 0 => DoneCheck::Rejected(format!(
            "gate `{gate}` died without recording an exit code — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
        GatePoll::Finished(code) => DoneCheck::Rejected(format!(
            "gate `{gate}` exited {code} — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
    }
}

fn check_done(sid: &str, cwd: &str) -> DoneCheck {
    let Some(st) = read_state(sid) else {
        return DoneCheck::Open;
    };
    let ts = tasks(&st);
    if ts.is_empty() || ts.iter().any(|t| t["passes"].as_bool() != Some(true)) {
        return DoneCheck::Open;
    }
    resolve_done(sid, cwd, &format!("all {} tasks passed", ts.len()))
}

// ── commands ──────────────────────────────────────────────────

/// `/loop <goal>` — seed state, return the kickoff prompt.
fn start_goal(session_id: &str, goal: &str, gate: Option<&str>) -> Value {
    let path = state_path(session_id);
    let existing_max = read_state(session_id)
        .and_then(|st| st.get("max").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_MAX);
    let mut st = json!({
        "goal": goal,
        "iter": 0,
        "max": existing_max,
        "status": "running",
        "notes": "",
        "tasks": [],
        "learnings": [],
        "steer": [],
    });
    if let Some(g) = gate {
        st["gate"] = json!(g);
    }
    if let Err(e) = write_state(session_id, &st) {
        return json!({ "text": format!("loop: couldn't write {}: {e}", path.display()) });
    }
    let gate_line = gate
        .map(|g| format!("\nA verification gate is set: `{g}` runs in the background (from the session directory) once all tasks pass — it must exit 0 before \"done\" is accepted; it resolves on a later iteration."))
        .unwrap_or_default();
    json!({ "prompt": format!(
        "Work on this goal: {goal}\n\n\
         You are inside a /loop — after this turn you will be re-prompted to continue until the goal is\n\
         done or the iteration cap ({existing_max}) is hit.\n\n\
         How this loop works:\n\
         1. Break the goal into checkable units and register them: call loop_task with op \"add\"\n\
            once per task. The loop ends itself when every task passes — that is the exit oracle,\n\
            not your say-so.\n\
         2. Each iteration: work the next unchecked task, verify it for real, then mark it with\n\
            loop_task op \"pass\". If it breaks later, mark op \"fail\" and fix it.\n\
         3. Anything a future iteration must know (decisions, traps, what worked), append with\n\
            loop_learn — the log rides every continue prompt.\n\
         4. Small task-free goals may skip the checklist and call loop_done directly.\n\
         5. Your durable journal is {} — \"notes\" is a short progress summary\n\
            for your next iteration (what's done, what's next, what not to redo). Update it before\n\
            ending every turn. In a git repo, prefer committing completed units so progress\n\
            survives context loss.{gate_line}\n\n\
         When the goal is verifiably complete call loop_done with status \"done\"; if it turns out\n\
         to be unachievable call it with \"failed\" and a reason.",
        path.display()
    ) })
}

fn run_command(argv: &[&str], params: &Value) -> Value {
    let sid = session_id(params);
    if sid.is_empty() {
        return json!({ "text": "loop: no session id" });
    }
    match argv.first().copied() {
        Some("stop") => {
            if let Some(mut st) = read_state(&sid) {
                if let Some(pid) = st.get("gate_run").and_then(|g| g.get("pid")).and_then(Value::as_u64) {
                    gate_kill(pid);
                }
                gate_clear(&mut st);
                st["status"] = json!("failed");
                st["notes"] = json!("stopped by user via /loop stop");
                if write_state(&sid, &st).is_ok() {
                    return json!({ "text": "loop stopped (status → failed)" });
                }
            }
            json!({ "text": "no active loop for this session" })
        }
        Some("status") => match read_state(&sid) {
            Some(st) => {
                let ts = tasks(&st);
                let task_line = if ts.is_empty() {
                    String::new()
                } else {
                    let done = ts.iter().filter(|t| t["passes"].as_bool() == Some(true)).count();
                    format!("\ntasks: {done}/{} done", ts.len())
                };
                let gate_line = st
                    .get("gate")
                    .and_then(Value::as_str)
                    .map(|g| format!("\ngate: {g}"))
                    .unwrap_or_default();
                let run_line = st
                    .get("gate_run")
                    .and_then(|r| {
                        let at = r.get("at").and_then(Value::as_u64)?;
                        Some(format!(
                            "\nverifying: gate running ({}s of {GATE_TIMEOUT_SECS}s)",
                            now_secs().saturating_sub(at)
                        ))
                    })
                    .unwrap_or_default();
                json!({ "text": format!(
                    "loop {} — iter {}/{} — goal: {}{}{}{}\nnotes: {}",
                    st.get("status").and_then(Value::as_str).unwrap_or("?"),
                    st.get("iter").and_then(Value::as_u64).unwrap_or(0),
                    st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX),
                    st.get("goal").and_then(Value::as_str).unwrap_or("?"),
                    task_line,
                    gate_line,
                    run_line,
                    st.get("notes").and_then(Value::as_str).unwrap_or(""),
                ) })
            }
            None => json!({ "text": "no loop state for this session" }),
        },
        Some("tasks") => match read_state(&sid) {
            Some(st) => json!({ "text": format!("loop tasks — {}", render_tasks(&tasks(&st))) }),
            None => json!({ "text": "no loop state for this session" }),
        },
        Some("steer") => {
            let msg = argv[1..].join(" ");
            if msg.is_empty() {
                return json!({ "text": "usage: /loop steer <message for next iteration>" });
            }
            match read_state(&sid) {
                Some(mut st) if st["status"].as_str() == Some("running") => {
                    let mut q = st["steer"].as_array().cloned().unwrap_or_default();
                    q.push(json!(msg));
                    st["steer"] = json!(q);
                    if write_state(&sid, &st).is_ok() {
                        json!({ "text": "steering queued for next iteration" })
                    } else {
                        json!({ "text": "loop: couldn't write state" })
                    }
                }
                _ => json!({ "text": "no running loop for this session" }),
            }
        }
        Some("max") => {
            let Some(n) = argv.get(1).and_then(|s| s.parse::<u64>().ok()).filter(|n| *n > 0) else {
                return json!({ "text": "usage: /loop max <positive N>" });
            };
            match read_state(&sid) {
                Some(mut st) => {
                    st["max"] = json!(n);
                    if write_state(&sid, &st).is_ok() {
                        json!({ "text": format!("loop max → {n}") })
                    } else {
                        json!({ "text": "loop: couldn't write state" })
                    }
                }
                None => json!({ "text": "no active loop — start one with /loop <goal> first" }),
            }
        }
        Some("list") => {
            let root = gray_home().join("loop");
            let mut lines = vec![];
            if let Ok(rd) = std::fs::read_dir(&root) {
                for ent in rd.flatten() {
                    let Ok(text) = std::fs::read_to_string(ent.path().join("state.json")) else { continue };
                    let Ok(st) = serde_json::from_str::<Value>(&text) else { continue };
                    let sess = ent.file_name().to_string_lossy().to_string();
                    lines.push(format!(
                        "{} — {} — iter {}/{} — {}",
                        &sess[..sess.len().min(8)],
                        st.get("status").and_then(Value::as_str).unwrap_or("?"),
                        st.get("iter").and_then(Value::as_u64).unwrap_or(0),
                        st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX),
                        st.get("goal").and_then(Value::as_str).unwrap_or("?"),
                    ));
                }
            }
            lines.sort();
            if lines.is_empty() {
                json!({ "text": "no loops found" })
            } else {
                json!({ "text": format!("loops ({}):\n{}", lines.len(), lines.join("\n")) })
            }
        }
        Some(goal_word) => {
            let words: Vec<&str> = std::iter::once(goal_word)
                .chain(argv.iter().skip(1).copied())
                .collect();
            let (goal, gate) = match words.iter().position(|w| *w == "--gate") {
                Some(p) => {
                    let g = words[p + 1..].join(" ");
                    (words[..p].join(" "), (!g.is_empty()).then_some(g))
                }
                None => (words.join(" "), None),
            };
            if goal.is_empty() {
                return json!({ "text": "usage: /loop <goal> [--gate \"<verify command>\"]" });
            }
            start_goal(&sid, &goal, gate.as_deref())
        }
        None => json!({ "text": format!(
            "gray-loop {} — /loop <goal> [--gate \"cmd\"] runs the goal in self-driving iterations. \
             /loop status · /loop tasks · /loop steer <msg> · /loop stop · /loop max N · /loop list",
            env!("CARGO_PKG_VERSION")
        ) }),
    }
}

// ── hook + tools ──────────────────────────────────────────────

/// `agent/before_start` — while a loop is running under its cap, bump iter
/// and append the continue message. Anything odd → `{}` (fail open).
fn before_start(params: &Value) -> Value {
    let sid = session_id(params);
    if sid.is_empty() {
        return json!({});
    }
    let Some(mut st) = read_state(&sid) else { return json!({}) };
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return json!({});
    }
    let mut gate_note: Option<String> = None;
    if st.get("gate_run").is_some() {
        match gate_poll(&sid, &mut st) {
            GatePoll::Finished(0) => {
                let ts = tasks(&st);
                let basis = if ts.is_empty() {
                    "goal reported done".to_string()
                } else {
                    format!("all {} tasks passed", ts.len())
                };
                let gate = st.get("gate").and_then(Value::as_str).unwrap_or("?").to_string();
                st["status"] = json!("done");
                st["notes"] = json!(format!("{basis}; gate `{gate}` green"));
                let _ = write_state(&sid, &st);
                return json!({});
            }
            GatePoll::Finished(code) => {
                let why = if code < 0 {
                    "ended without recording an exit code".to_string()
                } else {
                    format!("exit {code}")
                };
                gate_note = Some(format!(
                    "Gate FAILED ({why}) — \"done\" was rejected, keep working. Output tail:\n{}",
                    gate_tail(&sid)
                ));
            }
            GatePoll::TimedOut(secs) => {
                gate_note = Some(format!(
                    "Gate TIMED OUT after {secs}s — \"done\" was rejected, keep working. Output tail:\n{}",
                    gate_tail(&sid)
                ));
            }
            GatePoll::Running(secs) => {
                gate_note = Some(format!("Gate still verifying ({secs}s of {GATE_TIMEOUT_SECS}s)."));
            }
            GatePoll::Absent => {}
        }
    }
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return json!({});
    }
    let iter = st.get("iter").and_then(Value::as_u64).unwrap_or(0);
    let max = st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX);
    if iter >= max {
        if let Some(pid) = st.get("gate_run").and_then(|g| g.get("pid")).and_then(Value::as_u64) {
            gate_kill(pid);
        }
        st["status"] = json!("failed");
        st["notes"] = json!(format!("iteration cap ({max}) reached"));
        let _ = write_state(&sid, &st);
        return json!({});
    }
    st["iter"] = json!(iter + 1);
    let steer: Vec<String> = st
        .get("steer")
        .and_then(Value::as_array)
        .map(|q| q.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !steer.is_empty() {
        st["steer"] = json!([]);
    }
    if write_state(&sid, &st).is_err() {
        return json!({});
    }
    let goal = st.get("goal").and_then(Value::as_str).unwrap_or("?");
    let mut parts = vec![format!("Loop iteration {} of {max} — goal: \"{goal}\"", iter + 1)];
    let ts = tasks(&st);
    if !ts.is_empty() {
        parts.push(format!("Tasks — {}", render_tasks(&ts)));
    }
    if let Some(l) = st.get("learnings").and_then(Value::as_array).filter(|l| !l.is_empty()) {
        let shown: Vec<String> = l.iter().rev().take(3).filter_map(Value::as_str).map(str::to_string).collect();
        parts.push(format!("Learnings (latest first):\n{}", shown.iter().map(|s| format!("  - {s}")).collect::<Vec<_>>().join("\n")));
    }
    if !steer.is_empty() {
        parts.push(format!("Steering: {}", steer.join(" | ")));
    }
    if let Some(note) = gate_note {
        parts.push(note);
    }
    if let Some(gate) = st.get("gate").and_then(Value::as_str).filter(|g| !g.is_empty()) {
        parts.push(format!("Gate: `{gate}` must exit 0 before \"done\" is accepted."));
    }
    parts.push(if ts.is_empty() {
        "Continue. Call loop_done when the goal is verifiably complete.".to_string()
    } else {
        "Continue with the next unchecked task; mark it with loop_task when it truly passes.".to_string()
    });
    json!({ "text": parts.join("\n") })
}

fn call_tool(name: &str, args: &Value, params: &Value) -> Result<String, String> {
    let sid = session_id(params);
    if sid.is_empty() {
        return Err("no session id".into());
    }
    match name {
        "loop_done" => {
            let status = args.get("status").and_then(Value::as_str).unwrap_or("");
            if !matches!(status, "done" | "failed") {
                return Err("status must be \"done\" or \"failed\"".into());
            }
            let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
            match read_state(&sid) {
                Some(mut st) if st.get("status").and_then(Value::as_str) == Some("running") => {
                    if status == "done" {
                        let ts = tasks(&st);
                        let unpassed = ts.iter().filter(|t| t["passes"].as_bool() != Some(true)).count();
                        if unpassed > 0 {
                            return Err(format!(
                                "{}/{} tasks still unchecked — pass them with loop_task first, or call loop_done \"failed\"",
                                ts.len() - unpassed, ts.len()
                            ));
                        }
                        let basis = if ts.is_empty() {
                            "goal reported done".to_string()
                        } else {
                            format!("all {} tasks passed", ts.len())
                        };
                        return match resolve_done(&sid, &session_cwd(params), &basis) {
                            DoneCheck::Complete(msg) | DoneCheck::Verifying(msg) => Ok(msg),
                            DoneCheck::Rejected(msg) => Err(msg),
                            DoneCheck::Open => Err("couldn't finalize loop".into()),
                        };
                    }
                    if let Some(pid) = st.get("gate_run").and_then(|g| g.get("pid")).and_then(Value::as_u64) {
                        gate_kill(pid);
                    }
                    gate_clear(&mut st);
                    let _ = write_state(&sid, &st);
                    if set_status(&sid, status, reason) {
                        Ok(format!("loop ended: {status}{}", if reason.is_empty() { String::new() } else { format!(" — {reason}") }))
                    } else {
                        Err("couldn't write loop state".into())
                    }
                }
                Some(_) => Ok("loop is not running".into()),
                None => Ok("no active loop is running".into()),
            }
        }
        "loop_task" => {
            let op = args.get("op").and_then(Value::as_str).unwrap_or("");
            let Some(mut st) = read_state(&sid) else {
                return Ok("no active loop is running".into());
            };
            if st.get("status").and_then(Value::as_str) != Some("running") {
                return Ok("loop is not running".into());
            }
            let mut ts = tasks(&st);
            match op {
                "add" => {
                    let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    if text.is_empty() {
                        return Err("loop_task add needs \"text\"".into());
                    }
                    ts.push(json!({ "id": ts.len() + 1, "text": text, "passes": false }));
                    st["tasks"] = json!(ts);
                    let _ = write_state(&sid, &st);
                    Ok(format!("task {} added\n{}", tasks(&st).len(), render_tasks(&tasks(&st))))
                }
                "pass" | "fail" => {
                    let want_pass = op == "pass";
                    let idx = match args.get("task") {
                        Some(Value::Number(n)) => n.as_u64().map(|u| u as usize).and_then(|u| u.checked_sub(1)),
                        Some(Value::String(s)) => {
                            let s = s.to_lowercase();
                            let text_of = |t: &Value| t["text"].as_str().unwrap_or("").to_lowercase();
                            if let Some(i) = ts.iter().position(|t| text_of(t) == s) {
                                Some(i)
                            } else {
                                let unique = |pred: &dyn Fn(&Value) -> bool| -> Option<usize> {
                                    let hits: Vec<usize> = ts
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, t)| pred(t))
                                        .map(|(i, _)| i)
                                        .collect();
                                    (hits.len() == 1).then_some(hits[0])
                                };
                                unique(&|t| text_of(t).starts_with(&s))
                                    .or_else(|| unique(&|t| text_of(t).contains(&s)))
                            }
                        }
                        _ => None,
                    };
                    let Some(i) = idx.filter(|i| *i < ts.len()) else {
                        return Err(format!("no such task — use a 1-based id, exact text, unique prefix, or unique substring\n{}", render_tasks(&ts)));
                    };
                    ts[i]["passes"] = json!(want_pass);
                    let label = ts[i]["text"].as_str().unwrap_or("?").to_string();
                    st["tasks"] = json!(ts);
                    let _ = write_state(&sid, &st);
                    let marked = format!("task {} {}: {}", i + 1, if want_pass { "passed" } else { "reopened" }, label);
                    if want_pass {
                        match check_done(&sid, &session_cwd(params)) {
                            DoneCheck::Open => {}
                            DoneCheck::Verifying(msg) | DoneCheck::Complete(msg) | DoneCheck::Rejected(msg) => {
                                return Ok(format!("{marked}\n{msg}"));
                            }
                        }
                    }
                    Ok(format!("{marked}\n{}", render_tasks(&tasks(&st))))
                }
                "list" => Ok(render_tasks(&ts)),
                _ => Err("op must be add|pass|fail|list".into()),
            }
        }
        "loop_learn" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if text.is_empty() {
                return Err("loop_learn needs \"text\"".into());
            }
            let Some(mut st) = read_state(&sid) else {
                return Ok("no active loop is running".into());
            };
            let mut l = st["learnings"].as_array().cloned().unwrap_or_default();
            l.push(json!(text));
            let keep = l.split_off(l.len().saturating_sub(MAX_LEARNINGS));
            st["learnings"] = json!(keep);
            let _ = write_state(&sid, &st);
            Ok(format!("learning {} logged", keep.len()))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ── wire ──────────────────────────────────────────────────────

pub fn handle_loop(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_loop(),
        "agent/before_start" => before_start(&params),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            run_command(&argv, &params)
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args, &params) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

pub fn main_loop() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_loop());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_loop(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_loop(&json!({ "id": 1, "method": method, "params": params })).0.unwrap()
    }

    /// Point state_path at a scratch GRAY_HOME for the duration of `f`.
    /// Env vars are process-global; tests touching them must serialize.
    fn with_home<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("grayloop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        unsafe { std::env::set_var("GRAY_HOME", &home) };
        let out = f(&home);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&home);
        out
    }

    #[test]
    fn manifest_claims_hook_tool_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "loop");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["agent/before_start"]));
        assert_eq!(m["tools"][0]["name"], "loop_done");
    }

    #[test]
    fn loop_lifecycle_start_nudge_done() {
        with_home("life", |home| {
            let sess = json!({ "id": "s-1", "cwd": "/x" });
            // /loop <goal> → prompt + state file
            let r = call("command/run", json!({ "name": "/loop", "argv": ["fix", "tests"], "session": sess }));
            assert!(r["result"]["prompt"].as_str().unwrap().contains("fix tests"));
            let st: Value = serde_json::from_str(
                &std::fs::read_to_string(home.join("loop/s-1/state.json")).unwrap(),
            ).unwrap();
            assert_eq!(st["status"], "running");
            // before_start nudges and counts
            let r = call("agent/before_start", json!({ "session": sess }));
            assert!(r["result"]["text"].as_str().unwrap().contains("Loop iteration 1"));
            // loop_done stops it
            let r = call("tool/call", json!({
                "name": "loop_done",
                "args": { "status": "done", "reason": "all green" },
                "session": sess,
            }));
            assert!(r["result"]["content"].as_str().unwrap().contains("done"));
            // no more nudges
            let r = call("agent/before_start", json!({ "session": sess }));
            assert_eq!(r["result"], json!({}));
        });
    }

    #[test]
    fn iteration_cap_forces_failed() {
        with_home("cap", |_| {
            let sess = json!({ "id": "s-2" });
            call("command/run", json!({ "name": "/loop", "argv": ["g"], "session": sess }));
            call("command/run", json!({ "name": "/loop", "argv": ["max", "2"], "session": sess }));
            for expect_nudge in [true, true, false] {
                let r = call("agent/before_start", json!({ "session": sess }));
                assert_eq!(r["result"].get("text").is_some(), expect_nudge);
            }
            let st = read_state("s-2").unwrap();
            assert_eq!(st["status"], "failed");
        });
    }

    #[test]
    fn stop_and_status_and_fail_open() {
        with_home("stop", |_| {
            let sess = json!({ "id": "s-3" });
            // no loop: everything degrades to text/{}
            let r = call("agent/before_start", json!({ "session": sess }));
            assert_eq!(r["result"], json!({}));
            let r = call("command/run", json!({ "name": "/loop", "argv": ["status"], "session": sess }));
            assert!(r["result"]["text"].as_str().unwrap().contains("no loop"));
            // start then stop
            call("command/run", json!({ "name": "/loop", "argv": ["x"], "session": sess }));
            let r = call("command/run", json!({ "name": "/loop", "argv": ["stop"], "session": sess }));
            assert!(r["result"]["text"].as_str().unwrap().contains("stopped"));
            let st = read_state("s-3").unwrap();
            assert_eq!(st["status"], "failed");
        });
    }

    #[test]
    fn loop_done_rejects_bad_status() {
        let r = call("tool/call", json!({
            "name": "loop_done", "args": { "status": "maybe" }, "session": { "id": "s-9" }
        }));
        assert_eq!(r["result"]["is_error"], true);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_loop(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
