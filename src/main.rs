//! gray-orchestrate — one sidecar for every multi-step driver plugin.
//!
//! Merges gray-loop, gray-gsd, gray-workflows, gray-plan and
//! gray-plannotator. Each source lives verbatim in `mods/` (entry points
//! renamed `*_<mod>`) with its own state files and slash commands — only the
//! wire plumbing is shared.
//!
//! Hook fan-out:
//! - `agent/before_start` → loop + plan: `{text}` results joined with blank
//!   lines (loop's continue nudge, plan's read-only banner).
//! - `tool/before`        → plan (read-only gate).
//! - `prompt/context`     → workflows + plannotator: `{text}` joined.
//! - `tool/call`          → `loop_done` → loop, `gsd_task` → gsd.
//! - `command/run`        → routed by `params.name` to the owning module.
//!
//! argv: `manifest` prints the merged manifest; `orchestrate <ledger>` is the
//! detached workflows supervisor (gray-workflows re-execs its own binary);
//! bare / `sidecar` runs the NDJSON wire loop.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "mods/gsd.rs"]
mod gsd;
#[allow(dead_code)]
#[path = "mods/loop.rs"]
mod r#loop;
#[allow(dead_code)]
#[path = "mods/plan.rs"]
mod plan;
#[allow(dead_code)]
#[path = "mods/plannotator.rs"]
mod plannotator;
#[allow(dead_code)]
#[path = "mods/workflows.rs"]
mod workflows;

const META: &str = "/orchestrate";

/// Every constituent manifest, in module order (first wins on collisions).
fn module_manifests() -> [(&'static str, Value); 5] {
    [
        ("loop", r#loop::manifest_loop()),
        ("gsd", gsd::manifest_gsd()),
        ("workflows", workflows::manifest_workflows()),
        ("plan", plan::manifest_plan()),
        ("plannotator", plannotator::manifest_plannotator()),
    ]
}

/// Append `items` from `m[key]` into `acc`, skipping duplicates.
fn union_strs(acc: &mut Vec<Value>, m: &Value, key: &str) {
    for v in m.get(key).and_then(Value::as_array).into_iter().flatten() {
        if !acc.contains(v) {
            acc.push(v.clone());
        }
    }
}

fn manifest() -> Value {
    let mut tools: Vec<Value> = Vec::new();
    let mut commands: Vec<Value> = vec![json!(META)];
    let mut hooks: Vec<Value> = Vec::new();
    let mut subcommands: Vec<Value> = Vec::new();
    let mut completion: Vec<Value> = Vec::new();
    for (_, m) in module_manifests() {
        for t in m.get("tools").and_then(Value::as_array).into_iter().flatten() {
            let name = t.get("name");
            if !tools.iter().any(|x| x.get("name") == name) {
                tools.push(t.clone());
            }
        }
        union_strs(&mut commands, &m, "commands");
        union_strs(&mut hooks, &m, "hooks");
        union_strs(&mut subcommands, &m, "subcommands");
        union_strs(&mut completion, &m, "completion");
    }
    json!({
        "name": "orchestrate",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": tools,
        "commands": commands,
        "subcommands": subcommands,
        "completion": completion,
        "hooks": hooks,
    })
}

/// Adapt the Option-returning lib-style handlers (workflows, plannotator)
/// to the `(reply, exit)` shape.
fn handle_workflows(req: &Value) -> (Option<Value>, bool) {
    (workflows::handle_workflows(req), false)
}
fn handle_plannotator(req: &Value) -> (Option<Value>, bool) {
    (plannotator::handle_plannotator(req), false)
}

type Handler = fn(&Value) -> (Option<Value>, bool);

/// Commands each module owns, routed by `command/run`'s `params.name`.
fn command_owner(name: &str) -> Option<Handler> {
    match name {
        "/loop" => Some(r#loop::handle_loop),
        "/gsd" => Some(gsd::handle_gsd),
        "/workflow" | "/workflows" | "workflows" | "/deep-research" => Some(handle_workflows),
        "/plan" => Some(plan::handle_plan),
        "/plan-review" | "/plan-accept" => Some(handle_plannotator),
        _ => None,
    }
}

/// Tools each module owns, routed by `tool/call`'s `params.name`.
fn tool_owner(name: &str) -> Option<Handler> {
    match name {
        "loop_done" => Some(r#loop::handle_loop),
        "gsd_task" => Some(gsd::handle_gsd),
        _ => None,
    }
}

/// Fan a hook out to several owners; concatenate their text-ish fields.
fn fan_out(req: &Value, owners: &[Handler]) -> Value {
    let mut parts: Vec<String> = Vec::new();
    for h in owners {
        if let (Some(reply), _) = h(req) {
            let Some(r) = reply.get("result") else { continue };
            for key in ["text", "message", "append"] {
                if let Some(t) = r.get(key).and_then(Value::as_str) {
                    let t = t.trim_end();
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        json!({})
    } else {
        json!({ "text": parts.join("\n\n") })
    }
}

/// `/orchestrate` — merged-plugin overview.
fn meta_command() -> String {
    format!(
        "gray-orchestrate {} — one sidecar, five multi-step drivers:\n\
         \x20 /loop <goal>           re-prompt the agent each turn until loop_done (loop)\n\
         \x20 /gsd <goal>            spec → plan → tasks in .gsd/, gsd_task tool (gsd)\n\
         \x20 /workflow <spec.yaml>  fan a yaml/json spec across subagents (workflows)\n\
         \x20 /workflows, /deep-research  status/resume/stop/report · research preset\n\
         \x20 /plan [on|off|status]  read-only planning mode (plan)\n\
         \x20 /plan-review, /plan-accept  annotated plan review loop (plannotator)",
        env!("CARGO_PKG_VERSION")
    )
}

fn handle(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "agent/before_start" => fan_out(req, &[r#loop::handle_loop, plan::handle_plan]),
        "tool/before" => {
            let (reply, _) = plan::handle_plan(req);
            return (reply, false);
        }
        "prompt/context" => fan_out(req, &[handle_workflows, handle_plannotator]),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match tool_owner(name) {
                Some(owner) => {
                    let (reply, _) = owner(req);
                    return (reply, false);
                }
                None => json!({ "content": format!("unknown tool: {name}"), "is_error": true }),
            }
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == META {
                json!({ "text": meta_command() })
            } else if let Some(owner) = command_owner(name) {
                let (reply, _) = owner(req);
                return (reply, false);
            } else {
                json!({ "text": format!("unknown command {name} — try {META}") })
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

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("manifest") => {
            println!("{}", manifest());
            return Ok(());
        }
        // gray-workflows' detached supervisor: `spawn_orchestrator` re-execs
        // current_exe (now this binary) as `orchestrate <ledger>`.
        Some("orchestrate") => {
            return workflows::main_workflows().map_err(std::io::Error::other);
        }
        _ => {}
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle(&req);
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
