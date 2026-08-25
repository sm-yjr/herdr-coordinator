use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::state::{
    effective_status, latest_claim, latest_claim_mut, now, obj_mut, open_decisions,
    optional_string, registry_as_value, short_uuid, sorted_registry, string, utc_ms, State,
    STATUSES,
};

pub const USAGE: &str = r#"Herdr Coordinator 的注册表、可信状态协议与注意力队列。

用法:
  fleet init
  fleet register <项目> --tab <tab_id> --commander <机长名> --cwd <路径>
  fleet set-status <项目> <状态> [备注]
  fleet unregister <项目>
  fleet list
  fleet route <项目> <给机长的指令>
  fleet sync
  fleet report <项目> <状态> <一行摘要> [--confidence <0..1>] [--evidence <类型:内容>]...
  fleet claims [项目] [--all] [--json]
  fleet verify <项目> [--claim <claim_id>] [--label <标签>] -- <命令> [参数...]
  fleet accept <项目> [--claim <claim_id>] [--note <说明>] [--force]
  fleet ask <项目> <问题> [--option <选项>]...
  fleet decisions [--all]
  fleet resolve <决策编号> <答案>
  fleet attention [--json]
  fleet inbox [--all]
  fleet plugin-reconcile | plugin-event
  fleet watch | watch-start | watch-status | watch-stop
  fleet tower | open

状态: working|idle|done|blocked|need_decision
"#;

pub fn run(state: &State, args: &[String]) -> Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!(USAGE)
    };
    if matches!(command, "--help" | "-h" | "help") {
        print!("{USAGE}");
        return Ok(0);
    }
    state.migrate_legacy()?;
    let rest = &args[1..];
    match command {
        "init" => cmd_init(state, rest),
        "register" => cmd_register(state, rest),
        "set-status" => cmd_set_status(state, rest),
        "unregister" => cmd_unregister(state, rest),
        "list" => cmd_list(state, rest),
        "route" => cmd_route(state, rest),
        "sync" => cmd_sync(state, rest),
        "report" => cmd_report(state, rest),
        "claims" => cmd_claims(state, rest),
        "verify" => cmd_verify(state, rest),
        "accept" => cmd_accept(state, rest),
        "ask" => cmd_ask(state, rest),
        "decisions" => cmd_decisions(state, rest),
        "resolve" => cmd_resolve(state, rest),
        "attention" => cmd_attention(state, rest),
        "inbox" => cmd_inbox(state, rest),
        "plugin-reconcile" => cmd_plugin_reconcile(state, rest),
        "plugin-event" => cmd_plugin_event(state, rest),
        "watch" | "watch-start" | "watch-status" | "watch-stop" => {
            crate::watch::run(state, command, rest)
        }
        "tower" => crate::ui::run(state).map(|_| 0),
        "open" => cmd_open(rest),
        _ => bail!(USAGE),
    }
}

fn no_args(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!(USAGE)
    }
    Ok(())
}

fn cmd_init(state: &State, args: &[String]) -> Result<i32> {
    no_args(args)?;
    state.init()?;
    println!("ok: {}", state.dir.display());
    Ok(0)
}

fn option_pairs(args: &[String]) -> Result<BTreeMap<String, String>> {
    if !args.len().is_multiple_of(2) {
        bail!(USAGE)
    }
    Ok(args
        .chunks(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect())
}

fn absolute_path(value: &str) -> Result<PathBuf> {
    let expanded = if value == "~" {
        crate::state::home_dir()
    } else if let Some(rest) = value.strip_prefix("~/") {
        crate::state::home_dir().join(rest)
    } else {
        PathBuf::from(value)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn cmd_register(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 7 {
        bail!(USAGE)
    }
    let project = &args[0];
    let options = option_pairs(&args[1..])?;
    for required in ["--tab", "--commander", "--cwd"] {
        if options.get(required).is_none_or(String::is_empty) {
            bail!(USAGE)
        }
    }
    let commander = options["--commander"].clone();
    state.with_lock(|| {
        let mut registry = state.registry();
        let mut entry = registry.remove(project).unwrap_or_else(|| json!({}));
        let object = obj_mut(&mut entry)?;
        object.insert("tab_id".into(), json!(options["--tab"]));
        object.insert("commander".into(), json!(commander));
        object.insert("cwd".into(), json!(absolute_path(&options["--cwd"])?));
        object.entry("status").or_insert(json!("idle"));
        object.entry("reported_status").or_insert(json!("idle"));
        object.entry("note").or_insert(json!(""));
        object.entry("claim_id").or_insert(Value::Null);
        object.entry("claim_state").or_insert(Value::Null);
        object.insert("updated_at".into(), json!(now()));
        registry.insert(project.clone(), entry);
        state.write_json("fleets.json", &registry_as_value(registry))
    })?;
    crate::watch::notify(state);
    state.refresh_attention()?;
    println!("registered: {project}（机长 {commander}）");
    Ok(0)
}

fn cmd_set_status(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 2 {
        bail!(USAGE)
    }
    let (project, status) = (&args[0], args[1].as_str());
    let note = args[2..].join(" ").trim().to_string();
    if !STATUSES.contains(&status) {
        bail!("状态必须是: {}", STATUSES.join("/"))
    }
    if status == "need_decision" {
        if note.is_empty() {
            bail!("need_decision 必须提供需要拍板的问题")
        }
        return cmd_ask(state, &[vec![project.clone(), note]].concat());
    }
    state.with_lock(|| {
        let mut registry = state.registry();
        let decisions = state.decisions();
        let entry = registry
            .get_mut(project)
            .ok_or_else(|| unregistered(project))?;
        let object = obj_mut(entry)?;
        object.insert("reported_status".into(), json!(status));
        object.insert(
            "status".into(),
            json!(if open_decisions(&decisions, Some(project)).is_empty() {
                status
            } else {
                "need_decision"
            }),
        );
        if !note.is_empty() {
            object.insert("note".into(), json!(note));
        }
        object.insert("updated_at".into(), json!(now()));
        state.write_json("fleets.json", &registry_as_value(registry))
    })?;
    state.refresh_attention()?;
    println!("ok: {project} -> {status}");
    Ok(0)
}

fn cmd_unregister(state: &State, args: &[String]) -> Result<i32> {
    if args.len() != 1 {
        bail!(USAGE)
    }
    state.with_lock(|| {
        let mut registry = state.registry();
        if registry.remove(&args[0]).is_none() {
            return Err(unregistered(&args[0]));
        }
        state.write_json("fleets.json", &registry_as_value(registry))
    })?;
    crate::watch::notify(state);
    state.refresh_attention()?;
    println!("removed: {}", args[0]);
    Ok(0)
}

fn unregistered(project: &str) -> anyhow::Error {
    anyhow!("未注册的项目: {project}（先 fleet register）")
}

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

fn snapshot_timeout() -> Result<Duration> {
    let seconds = env::var("HERDR_SNAPSHOT_TIMEOUT")
        .unwrap_or_else(|_| "15".into())
        .parse::<f64>()
        .context("HERDR_SNAPSHOT_TIMEOUT 必须是秒数")?;
    if seconds <= 0.0 {
        bail!("HERDR_SNAPSHOT_TIMEOUT 必须大于 0")
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn output_with_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<std::process::Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("无法执行 {label}"))?;
    if child.wait_timeout(timeout)?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{label} 超时（{}s）", timeout.as_secs_f64());
    }
    child
        .wait_with_output()
        .with_context(|| format!("无法读取 {label} 输出"))
}

pub fn snapshot_from_cli() -> Result<Value> {
    let mut command = Command::new(herdr_bin());
    command.args(["api", "snapshot"]);
    let output = output_with_timeout(command, snapshot_timeout()?, "herdr api snapshot")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let payload: Value = serde_json::from_slice(&output.stdout).context("无法解析 Herdr 快照")?;
    let result = payload.get("result").unwrap_or(&payload);
    let snapshot = result.get("snapshot").unwrap_or(result);
    if !snapshot.is_object() {
        bail!("Herdr 未返回有效 snapshot")
    }
    Ok(snapshot.clone())
}

fn cmd_sync(state: &State, args: &[String]) -> Result<i32> {
    no_args(args)?;
    let runtime = state.write_runtime(&snapshot_from_cli()?, true, None, "manual_sync", None)?;
    state.refresh_attention()?;
    println!(
        "已从 Herdr 快照恢复 {} 个 Agent 的实时基线",
        runtime["agents"].as_array().map(Vec::len).unwrap_or(0)
    );
    Ok(0)
}

fn cmd_plugin_reconcile(state: &State, args: &[String]) -> Result<i32> {
    no_args(args)?;
    state.import_legacy_once()?;
    state.init()?;
    let runtime = state.write_runtime(&snapshot_from_cli()?, true, None, "plugin_startup", None)?;
    state.refresh_attention()?;
    println!(
        "plugin reconciled: {} agents",
        runtime["agents"].as_array().map(Vec::len).unwrap_or(0)
    );
    Ok(0)
}

fn plugin_event() -> Result<(Option<String>, Option<Value>)> {
    let envelope = match env::var("HERDR_PLUGIN_EVENT_JSON") {
        Ok(raw) if !raw.trim().is_empty() => {
            Some(serde_json::from_str::<Value>(&raw).context("HERDR_PLUGIN_EVENT_JSON 无效")?)
        }
        _ => None,
    };
    let name = env::var("HERDR_PLUGIN_EVENT").ok().or_else(|| {
        envelope
            .as_ref()
            .and_then(|value| value.get("event"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    Ok((name, envelope))
}

fn cmd_plugin_event(state: &State, args: &[String]) -> Result<i32> {
    no_args(args)?;
    state.import_legacy_once()?;
    state.init()?;
    let (event_name, envelope) = plugin_event()?;
    let last_event = json!({"name": event_name, "envelope": envelope});
    let runtime = state.with_lock(|| match snapshot_from_cli() {
        Ok(snapshot) => state.write_runtime(
            &snapshot,
            true,
            None,
            "plugin_event",
            Some(last_event.clone()),
        ),
        Err(error) => {
            let previous = state.load("runtime.json", json!({}));
            let snapshot = json!({
                "agents": previous.get("agents").cloned().unwrap_or_else(|| json!([])),
                "version": previous.get("herdr_version").cloned().unwrap_or(Value::Null),
                "protocol": previous.get("protocol").cloned().unwrap_or(Value::Null)
            });
            state.write_runtime(
                &snapshot,
                false,
                Some(error.to_string()),
                "plugin_event",
                Some(last_event.clone()),
            )
        }
    })?;
    state.refresh_attention()?;
    println!(
        "plugin event reconciled: {} ({} agents)",
        event_name.unwrap_or_else(|| "unknown".into()),
        runtime["agents"].as_array().map(Vec::len).unwrap_or(0)
    );
    Ok(0)
}

fn parse_evidence(value: &str) -> Result<Value> {
    let (kind, detail) = value.split_once(':').unwrap_or(("note", value));
    let kind = if kind.trim().is_empty() {
        "note"
    } else {
        kind.trim()
    };
    let detail = detail.trim();
    if detail.is_empty() {
        bail!("--evidence 不能为空")
    }
    Ok(json!({
        "id": format!("evidence-{}", short_uuid(10)), "kind": kind, "value": detail,
        "source": "commander", "verified": false, "created_at": now()
    }))
}

fn cmd_report(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 3 {
        bail!(USAGE)
    }
    let project = &args[0];
    let status = args[1].as_str();
    if !STATUSES.contains(&status) {
        bail!("状态必须是: {}", STATUSES.join("/"))
    }
    let mut summary = Vec::new();
    let mut confidence = Value::Null;
    let mut evidence = Vec::new();
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--confidence" if index + 1 < args.len() => {
                let value: f64 = args[index + 1]
                    .parse()
                    .context("--confidence 必须是 0 到 1 的数字")?;
                if !(0.0..=1.0).contains(&value) {
                    bail!("--confidence 必须在 0 到 1 之间")
                }
                confidence = json!(value);
                index += 2;
            }
            "--evidence" if index + 1 < args.len() => {
                evidence.push(parse_evidence(&args[index + 1])?);
                index += 2;
            }
            flag if flag == "--confidence" || flag == "--evidence" => bail!("{flag} 后缺少参数"),
            _ => {
                summary.push(args[index].clone());
                index += 1;
            }
        }
    }
    let summary = summary.join(" ").trim().to_string();
    if summary.is_empty() {
        bail!("必须提供一行摘要")
    }
    if status == "need_decision" {
        return cmd_ask(state, &[vec![project.clone(), summary]].concat());
    }
    let claim_id = format!(
        "{project}-claim-{}-{}",
        Local::now().format("%Y%m%d-%H%M%S"),
        short_uuid(6)
    );
    let created = now();
    state.with_lock(|| {
        let mut registry = state.registry();
        let decisions = state.decisions();
        let mut claims = state.claims();
        let entry = registry
            .get_mut(project)
            .ok_or_else(|| unregistered(project))?;
        let claim = json!({
            "id": claim_id, "project": project, "status": status, "summary": summary,
            "confidence": confidence, "state": "reported", "evidence": evidence,
            "created_at": created, "verified_at": null, "accepted_at": null,
            "accepted_by": null, "acceptance_note": null
        });
        claims.push(claim);
        let object = obj_mut(entry)?;
        object.insert("reported_status".into(), json!(status));
        object.insert(
            "status".into(),
            json!(if open_decisions(&decisions, Some(project)).is_empty() {
                status
            } else {
                "need_decision"
            }),
        );
        object.insert("note".into(), json!(summary));
        object.insert("claim_id".into(), json!(claim_id));
        object.insert("claim_state".into(), json!("reported"));
        object.insert("updated_at".into(), json!(created));
        state.write_json("fleets.json", &registry_as_value(registry))?;
        state.write_json("claims.json", &Value::Array(claims))?;
        state.append_inbox(&json!({
            "ts": created, "type": "status_claimed", "project": project, "status": status,
            "summary": summary, "claim_id": claim_id, "claim_state": "reported",
            "confidence": confidence, "evidence_count": evidence.len()
        }))
    })?;
    state.refresh_attention()?;
    println!("claimed: {claim_id}");
    if status == "done" {
        println!("next: fleet verify {project} --claim {claim_id} --label <标签> -- <验证命令>");
    }
    Ok(0)
}

fn cmd_claims(state: &State, args: &[String]) -> Result<i32> {
    let mut project: Option<&str> = None;
    let mut all = false;
    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            "--json" => json_mode = true,
            value if !value.starts_with("--") && project.is_none() => project = Some(value),
            _ => bail!(USAGE),
        }
    }
    let values: Vec<Value> = state
        .claims()
        .into_iter()
        .filter(|claim| {
            project
                .map(|name| string(claim, "project") == name)
                .unwrap_or(true)
        })
        .collect();
    let values = if all {
        values
    } else {
        let mut latest = BTreeMap::new();
        for claim in values {
            latest.insert(string(&claim, "project"), claim);
        }
        latest.into_values().collect()
    };
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(0);
    }
    if values.is_empty() {
        println!("(没有状态声明)");
        return Ok(0);
    }
    for claim in values {
        let confidence = claim
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|value| format!("  confidence={value:.2}"))
            .unwrap_or_default();
        println!(
            "{}  {}  {}  {}  {}{}  evidence={}",
            string(&claim, "id"),
            string(&claim, "project"),
            string(&claim, "status"),
            string(&claim, "state"),
            string(&claim, "summary"),
            confidence,
            claim
                .get("evidence")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        );
    }
    Ok(0)
}

fn cmd_verify(state: &State, args: &[String]) -> Result<i32> {
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| anyhow!(USAGE))?;
    if separator == 0 || separator + 1 >= args.len() {
        bail!(USAGE)
    }
    let before = &args[..separator];
    let command = &args[separator + 1..];
    let project = &before[0];
    let mut claim_id: Option<&str> = None;
    let mut label = "verification";
    let mut index = 1;
    while index < before.len() {
        match before[index].as_str() {
            "--claim" if index + 1 < before.len() => {
                claim_id = Some(&before[index + 1]);
                index += 2;
            }
            "--label" if index + 1 < before.len() => {
                label = &before[index + 1];
                index += 2;
            }
            _ => bail!(USAGE),
        }
    }
    let (selected, cwd) = state.with_lock(|| {
        let registry = state.registry();
        let claims = state.claims();
        let entry = registry.get(project).ok_or_else(|| unregistered(project))?;
        let claim = latest_claim(&claims, project, claim_id)
            .ok_or_else(|| anyhow!("项目 {project} 没有可验证的 claim"))?;
        Ok((
            string(claim, "id"),
            optional_string(entry, "cwd").unwrap_or_else(|| ".".into()),
        ))
    })?;
    let started_at = now();
    let started_ms = utc_ms();
    let timeout = env::var("HERDR_VERIFY_TIMEOUT")
        .unwrap_or_else(|_| "900".into())
        .parse::<u64>()
        .context("HERDR_VERIFY_TIMEOUT 必须是秒数")?;
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (exit_code, stdout, stderr, error) = match process.spawn() {
        Ok(mut child) => {
            let timed_out = child.wait_timeout(Duration::from_secs(timeout))?.is_none();
            if timed_out {
                let _ = child.kill();
            }
            let output = child.wait_with_output()?;
            let code = if timed_out {
                124
            } else {
                output.status.code().unwrap_or(1)
            };
            (
                code,
                String::from_utf8_lossy(&output.stdout).to_string(),
                String::from_utf8_lossy(&output.stderr).to_string(),
                timed_out.then(|| format!("timeout after {timeout}s")),
            )
        }
        Err(error) => (127, String::new(), String::new(), Some(error.to_string())),
    };
    let finished_at = now();
    let evidence_id = format!("evidence-{}", short_uuid(10));
    let evidence = json!({
        "id": evidence_id, "kind": "command", "label": label, "command": command,
        "cwd": cwd, "source": "coordinator", "verified": exit_code == 0,
        "exit_code": exit_code, "stdout_tail": tail(&stdout, 4000), "stderr_tail": tail(&stderr, 4000),
        "error": error, "started_at": started_at, "finished_at": finished_at,
        "duration_ms": (utc_ms() - started_ms).max(0)
    });
    state.with_lock(|| {
        let mut registry = state.registry();
        let mut claims = state.claims();
        let entry = registry.get_mut(project).ok_or_else(|| unregistered(project))?;
        let claim = latest_claim_mut(&mut claims, project, Some(&selected)).ok_or_else(|| anyhow!("claim 在验证期间被删除: {selected}"))?;
        let object = obj_mut(claim)?;
        object.entry("evidence").or_insert_with(|| json!([])).as_array_mut().ok_or_else(|| anyhow!("claim evidence 格式无效"))?.push(evidence);
        if object.get("state").and_then(Value::as_str) != Some("accepted") {
            object.insert("state".into(), json!(if exit_code == 0 { "verified" } else { "reported" }));
        }
        object.insert(if exit_code == 0 { "verified_at".into() } else { "verification_failed_at".into() }, json!(finished_at));
        let status = object.get("status").cloned().unwrap_or_else(|| json!("unknown"));
        let claim_state = object.get("state").cloned().unwrap_or_else(|| json!("reported"));
        if entry.get("claim_id").and_then(Value::as_str) == Some(&selected) {
            let entry = obj_mut(entry)?;
            entry.insert("claim_state".into(), claim_state);
            entry.insert("updated_at".into(), json!(finished_at));
        }
        state.write_json("fleets.json", &registry_as_value(registry))?;
        state.write_json("claims.json", &Value::Array(claims))?;
        state.append_inbox(&json!({
            "ts": finished_at, "type": if exit_code == 0 { "verification_passed" } else { "verification_failed" },
            "project": project, "status": status, "summary": format!("{label}: {}", if exit_code == 0 { "通过" } else { "失败" }),
            "claim_id": selected, "evidence_id": evidence_id, "exit_code": exit_code
        }))
    })?;
    state.refresh_attention()?;
    println!(
        "{}: {selected} ({label}, exit={exit_code})",
        if exit_code == 0 {
            "verified"
        } else {
            "verification failed"
        }
    );
    Ok(exit_code)
}

fn tail(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value
        .chars()
        .rev()
        .take(limit)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn cmd_accept(state: &State, args: &[String]) -> Result<i32> {
    if args.is_empty() {
        bail!(USAGE)
    }
    let project = &args[0];
    let mut claim_id = None;
    let mut note = String::new();
    let mut force = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--claim" if index + 1 < args.len() => {
                claim_id = Some(args[index + 1].as_str());
                index += 2;
            }
            "--note" if index + 1 < args.len() => {
                note = args[index + 1].clone();
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            _ => bail!(USAGE),
        }
    }
    if force && note.is_empty() {
        bail!("--force 必须同时提供 --note，记录接受未验证风险的原因")
    }
    let mut accepted_id = String::new();
    state.with_lock(|| {
        let mut registry = state.registry();
        let mut claims = state.claims();
        let entry = registry.get_mut(project).ok_or_else(|| unregistered(project))?;
        let claim = latest_claim_mut(&mut claims, project, claim_id).ok_or_else(|| anyhow!("项目 {project} 没有可验收的 claim"))?;
        if string(claim, "status") != "done" { bail!("只有 done claim 可以验收") }
        if string(claim, "state") != "verified" && !force { bail!("claim 尚未通过机器验证；先运行 fleet verify，或由用户明确授权 --force") }
        accepted_id = string(claim, "id");
        let accepted_at = now();
        let object = obj_mut(claim)?;
        object.insert("state".into(), json!("accepted"));
        object.insert("accepted_at".into(), json!(accepted_at));
        object.insert("accepted_by".into(), json!(env::var("USER").or_else(|_| env::var("USERNAME")).unwrap_or_else(|_| "operator".into())));
        object.insert("acceptance_note".into(), if note.is_empty() { Value::Null } else { json!(note) });
        if entry.get("claim_id").and_then(Value::as_str) == Some(&accepted_id) {
            let entry = obj_mut(entry)?;
            entry.insert("claim_state".into(), json!("accepted"));
            entry.insert("updated_at".into(), json!(accepted_at));
        }
        state.write_json("fleets.json", &registry_as_value(registry))?;
        state.write_json("claims.json", &Value::Array(claims))?;
        state.append_inbox(&json!({"ts": accepted_at, "type": "claim_accepted", "project": project, "status": "done", "summary": if note.is_empty() { "用户已验收" } else { &note }, "claim_id": accepted_id}))
    })?;
    state.refresh_attention()?;
    println!("accepted: {accepted_id}");
    Ok(0)
}

fn cmd_ask(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 2 {
        bail!(USAGE)
    }
    let project = &args[0];
    let mut question = Vec::new();
    let mut options = Vec::new();
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--option" {
            if index + 1 >= args.len() {
                bail!("--option 后必须提供内容")
            }
            options.push(args[index + 1].clone());
            index += 2;
        } else {
            question.push(args[index].clone());
            index += 1;
        }
    }
    let question = question.join(" ").trim().to_string();
    if question.is_empty() {
        bail!("必须提供需要拍板的问题")
    }
    let decision_id = format!(
        "{project}-{}-{}",
        Local::now().format("%Y%m%d-%H%M%S"),
        short_uuid(6)
    );
    let created = now();
    state.with_lock(|| {
        let mut registry = state.registry();
        let mut decisions = state.decisions();
        let entry = registry.get_mut(project).ok_or_else(|| unregistered(project))?;
        decisions.push(json!({
            "id": decision_id, "project": project, "state": "open", "question": question,
            "options": options, "source": "commander_report", "created_at": created,
            "resolved_at": null, "resolution": null
        }));
        let entry = obj_mut(entry)?;
        entry.insert("status".into(), json!("need_decision"));
        entry.insert("note".into(), json!(question));
        entry.insert("updated_at".into(), json!(created));
        state.write_json("fleets.json", &registry_as_value(registry))?;
        state.write_json("decisions.json", &Value::Array(decisions))?;
        state.append_inbox(&json!({
            "ts": created, "type": "decision_requested", "project": project,
            "status": "need_decision", "summary": question, "decision_id": decision_id, "options": options
        }))
    })?;
    state.refresh_attention()?;
    println!("待拍板: {decision_id}");
    Ok(0)
}

fn cmd_decisions(state: &State, args: &[String]) -> Result<i32> {
    if !(args.is_empty() || args.len() == 1 && args[0] == "--all") {
        bail!(USAGE)
    }
    let all = !args.is_empty();
    let decisions = state.decisions();
    let values: Vec<&Value> = if all {
        decisions.iter().collect()
    } else {
        open_decisions(&decisions, None)
    };
    if values.is_empty() {
        println!(
            "{}",
            if all {
                "(没有决策记录)"
            } else {
                "(没有待拍板事项)"
            }
        );
        return Ok(0);
    }
    for item in values {
        let state_label = if string(item, "state") == "open" {
            "待拍板"
        } else {
            "已解决"
        };
        let options = item
            .get("options")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default();
        let mut suffix = if options.is_empty() {
            String::new()
        } else {
            format!("  选项: {options}")
        };
        if let Some(resolution) = item.get("resolution").and_then(Value::as_str) {
            suffix.push_str(&format!("  结论: {resolution}"));
        }
        println!(
            "{}  {}  {}  {}{}",
            string(item, "id"),
            string(item, "project"),
            state_label,
            string(item, "question"),
            suffix
        );
    }
    Ok(0)
}

fn cmd_resolve(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 2 {
        bail!(USAGE)
    }
    let decision_id = &args[0];
    let answer = args[1..].join(" ").trim().to_string();
    if answer.is_empty() {
        bail!("必须提供用户的拍板答案")
    }
    let mut project = String::new();
    state.with_lock(|| {
        let mut registry = state.registry();
        let mut decisions = state.decisions();
        let index = decisions.iter().position(|value| string(value, "id") == *decision_id).ok_or_else(|| anyhow!("找不到决策: {decision_id}"))?;
        if string(&decisions[index], "state") != "open" { bail!("决策已经解决: {decision_id}") }
        project = string(&decisions[index], "project");
        let target = obj_mut(&mut decisions[index])?;
        target.insert("state".into(), json!("resolved"));
        target.insert("resolution".into(), json!(answer));
        target.insert("resolved_at".into(), json!(now()));
        let remaining: Vec<String> = decisions.iter().enumerate().filter(|(other, item)| *other != index && string(item, "project") == project && string(item, "state") == "open").map(|(_, item)| string(item, "question")).collect();
        let entry = registry.get_mut(&project).ok_or_else(|| unregistered(&project))?;
        let entry = obj_mut(entry)?;
        let reported = entry.get("reported_status").and_then(Value::as_str).unwrap_or("idle").to_string();
        entry.insert("status".into(), json!(if remaining.is_empty() { reported } else { "need_decision".into() }));
        entry.insert("note".into(), json!(remaining.first().cloned().unwrap_or_else(|| format!("已拍板：{answer}"))));
        entry.insert("updated_at".into(), json!(now()));
        let resolved_status = entry.get("status").cloned().unwrap_or_else(|| json!("idle"));
        state.write_json("fleets.json", &registry_as_value(registry))?;
        state.write_json("decisions.json", &Value::Array(decisions))?;
        state.append_inbox(&json!({"ts": now(), "type": "decision_resolved", "project": project, "status": resolved_status, "summary": format!("{decision_id} → {answer}"), "decision_id": decision_id}))
    })?;
    state.refresh_attention()?;
    println!("已拍板: {decision_id} -> {answer}");
    Ok(0)
}

fn cmd_attention(state: &State, args: &[String]) -> Result<i32> {
    if !(args.is_empty() || args.len() == 1 && args[0] == "--json") {
        bail!(USAGE)
    }
    let values = state.refresh_attention()?;
    if args.first().is_some_and(|value| value == "--json") {
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(0);
    }
    if values.is_empty() {
        println!("(当前无需人工介入)");
        return Ok(0);
    }
    for (index, item) in values.iter().enumerate() {
        println!(
            "{:>2}. P{}  {}  {}  {}  → {}",
            index + 1,
            item["priority"],
            string(item, "project"),
            string(item, "kind"),
            string(item, "reason"),
            string(item, "required_actor")
        );
        println!("    {}", string(item, "action"));
    }
    Ok(0)
}

fn cmd_list(state: &State, args: &[String]) -> Result<i32> {
    no_args(args)?;
    let registry = state.registry();
    if registry.is_empty() {
        println!("(注册表为空)");
        return Ok(0);
    }
    let decisions = state.decisions();
    let runtime = state.load("runtime.json", json!({}));
    let projects = runtime.get("projects").and_then(Value::as_object);
    let watching = crate::watch::pid(state).is_some();
    for (project, entry) in sorted_registry(&registry) {
        let live = projects.and_then(|values| values.get(&project));
        let live_text = if runtime
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            live.map(|value| string(value, "live_status"))
                .unwrap_or_else(|| "offline".into())
        } else if state.plugin_mode() {
            "未对账".into()
        } else if !watching {
            "未监听".into()
        } else {
            "连接中断".into()
        };
        let status = effective_status(&entry, &decisions, &project);
        let decision = open_decisions(&decisions, Some(&project))
            .first()
            .map(|item| format!("  待拍板={}", string(item, "question")))
            .unwrap_or_default();
        let claim = optional_string(&entry, "claim_state")
            .map(|value| format!("  claim={value}"))
            .unwrap_or_default();
        println!("{project:<16} 机长={:<14} 项目={status:<14} 实时={live_text:<10} 更新={}  {}{decision}{claim}", string(&entry, "commander"), string(&entry, "updated_at"), string(&entry, "note"));
    }
    let registered: HashSet<String> = registry
        .values()
        .map(|entry| string(entry, "commander"))
        .collect();
    let ghosts: Vec<String> = runtime
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| agent.get("name").and_then(Value::as_str))
        .filter(|name| name.ends_with("-cmd") && !registered.contains(*name))
        .map(str::to_string)
        .collect();
    if !ghosts.is_empty() {
        println!("⚠ 存活但未注册的机长: {}", ghosts.join(", "));
    }
    let attention = state.refresh_attention()?;
    if let Some(top) = attention.first() {
        println!(
            "⚠ attention={}  top={}/{}: {}",
            attention.len(),
            string(top, "project"),
            string(top, "kind"),
            string(top, "reason")
        );
    }
    if !state.plugin_mode() && !watching {
        println!("⚠ 事件监听器未运行；执行 ./fleet watch-start，或安装为 Herdr plugin");
    }
    Ok(0)
}

fn cmd_route(state: &State, args: &[String]) -> Result<i32> {
    if args.len() < 2 {
        bail!(USAGE)
    }
    let project = &args[0];
    let text = args[1..].join(" ").trim().to_string();
    if text.is_empty() {
        bail!("必须提供要转发给机长的指令")
    }
    let registry = state.registry();
    let entry = registry.get(project).ok_or_else(|| unregistered(project))?;
    let commander = string(entry, "commander");
    if commander.is_empty() {
        bail!("项目 {project} 没有登记机长")
    }
    let output = Command::new(herdr_bin())
        .args(["agent", "prompt", &commander, &text])
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
    println!("已转发给 {project} 的机长");
    Ok(0)
}

fn cmd_inbox(state: &State, args: &[String]) -> Result<i32> {
    if !(args.is_empty() || args.len() == 1 && args[0] == "--all") {
        bail!(USAGE)
    }
    let all = !args.is_empty();
    let entries = state.inbox_entries();
    let cursor = if all {
        0
    } else {
        fs::read_to_string(state.path("inbox.cursor"))
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    };
    let values = if all {
        &entries[..]
    } else {
        entries.get(cursor..).unwrap_or(&[])
    };
    if values.is_empty() {
        println!(
            "{}",
            if entries.is_empty() && all {
                "(收件箱为空)"
            } else {
                "(没有新消息)"
            }
        );
    }
    for entry in values {
        let mut refs = Vec::new();
        if let Some(id) = entry.get("decision_id").and_then(Value::as_str) {
            refs.push(format!("决策 {id}"));
        }
        if let Some(id) = entry.get("claim_id").and_then(Value::as_str) {
            refs.push(format!("claim {id}"));
        }
        let reference = if refs.is_empty() {
            String::new()
        } else {
            format!(" · {}", refs.join(" · "))
        };
        println!(
            "[{}] {} · {} · {}{}",
            string(entry, "ts"),
            string(entry, "project"),
            string(entry, "status"),
            string(entry, "summary"),
            reference
        );
    }
    if !all {
        fs::write(state.path("inbox.cursor"), entries.len().to_string())?;
    }
    Ok(0)
}

fn cmd_open(args: &[String]) -> Result<i32> {
    no_args(args)?;
    let plugin_id = env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| crate::state::PLUGIN_ID.into());
    let output = Command::new(herdr_bin()).args(["pane", "list"]).output()?;
    if output.status.success() {
        if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
            let panes = value
                .get("result")
                .unwrap_or(&value)
                .get("panes")
                .and_then(Value::as_array);
            if let Some(pane) = panes.into_iter().flatten().find(|pane| {
                pane.get("label")
                    .or_else(|| pane.get("title"))
                    .and_then(Value::as_str)
                    == Some("Fleet Control Tower")
            }) {
                if let Some(id) = pane.get("pane_id").and_then(Value::as_str) {
                    let status = if pane
                        .get("focused")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Command::new(herdr_bin())
                            .args(["pane", "close", id])
                            .status()?
                    } else {
                        Command::new(herdr_bin())
                            .args(["plugin", "pane", "focus", id])
                            .status()?
                    };
                    return Ok(status.code().unwrap_or(1));
                }
            }
        }
    }
    let status = Command::new(herdr_bin())
        .args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            &plugin_id,
            "--entrypoint",
            "tower",
            "--placement",
            "overlay",
            "--focus",
        ])
        .status()?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn fixture() -> (TempDir, State) {
        let temporary = tempfile::tempdir().unwrap();
        let state = State {
            dir: temporary.path().join("state"),
        };
        run(&state, &strings(&["init"])).unwrap();
        run(
            &state,
            &strings(&[
                "register",
                "demo",
                "--tab",
                "w1:t1",
                "--commander",
                "demo-cmd",
                "--cwd",
                temporary.path().to_str().unwrap(),
            ]),
        )
        .unwrap();
        (temporary, state)
    }

    #[test]
    fn decision_is_first_class_and_preserves_reported_status() {
        let (_temporary, state) = fixture();
        run(&state, &strings(&["report", "demo", "working", "开始实现"])).unwrap();
        run(
            &state,
            &strings(&[
                "ask",
                "demo",
                "采用哪种兼容策略？",
                "--option",
                "保持旧格式",
                "--option",
                "升级格式",
            ]),
        )
        .unwrap();
        run(&state, &strings(&["report", "demo", "done", "实现已完成"])).unwrap();
        let registry = state.registry();
        assert_eq!(registry["demo"]["reported_status"], "done");
        assert_eq!(registry["demo"]["status"], "need_decision");
        let decision = state.decisions().remove(0);
        assert_eq!(decision["options"], json!(["保持旧格式", "升级格式"]));
        run(
            &state,
            &strings(&["resolve", &string(&decision, "id"), "保持旧格式"]),
        )
        .unwrap();
        assert_eq!(state.registry()["demo"]["status"], "done");
    }

    #[test]
    fn blocked_does_not_create_a_decision() {
        let (_temporary, state) = fixture();
        run(
            &state,
            &strings(&["set-status", "demo", "blocked", "等待外部服务"]),
        )
        .unwrap();
        assert!(state.decisions().is_empty());
        assert_eq!(state.registry()["demo"]["status"], "blocked");
    }

    #[test]
    fn report_verify_accept_has_three_distinct_stages() {
        let (_temporary, state) = fixture();
        run(
            &state,
            &strings(&[
                "report",
                "demo",
                "done",
                "实现完成",
                "--confidence",
                "0.91",
                "--evidence",
                "commit:abc123",
            ]),
        )
        .unwrap();
        let claim_id = string(&state.claims()[0], "id");
        assert_eq!(state.claims()[0]["state"], "reported");
        assert!(state
            .refresh_attention()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "reported_done_unverified"));
        assert_eq!(
            run(
                &state,
                &strings(&[
                    "verify",
                    "demo",
                    "--claim",
                    &claim_id,
                    "--label",
                    "unit-tests",
                    "--",
                    "sh",
                    "-c",
                    "printf ok"
                ])
            )
            .unwrap(),
            0
        );
        assert_eq!(state.claims()[0]["state"], "verified");
        run(
            &state,
            &strings(&["accept", "demo", "--claim", &claim_id, "--note", "验收通过"]),
        )
        .unwrap();
        assert_eq!(state.claims()[0]["state"], "accepted");
        assert_eq!(state.registry()["demo"]["claim_state"], "accepted");
    }

    #[test]
    fn failed_verification_returns_the_command_exit_code() {
        let (_temporary, state) = fixture();
        run(&state, &strings(&["report", "demo", "done", "声称完成"])).unwrap();
        let code = run(
            &state,
            &strings(&["verify", "demo", "--", "sh", "-c", "exit 3"]),
        )
        .unwrap();
        assert_eq!(code, 3);
        assert_eq!(state.claims()[0]["state"], "reported");
        assert_eq!(state.claims()[0]["evidence"][0]["exit_code"], 3);
    }

    #[test]
    fn force_acceptance_requires_done_and_a_note() {
        let (_temporary, state) = fixture();
        run(&state, &strings(&["report", "demo", "working", "仍在开发"])).unwrap();
        let claim_id = string(&state.claims()[0], "id");
        assert!(run(
            &state,
            &strings(&[
                "accept",
                "demo",
                "--claim",
                &claim_id,
                "--force",
                "--note",
                "不应允许"
            ])
        )
        .is_err());
        assert_eq!(state.claims()[0]["state"], "reported");
    }

    #[test]
    fn decision_precedes_a_stale_block_in_attention() {
        let (_temporary, state) = fixture();
        run(
            &state,
            &strings(&[
                "register",
                "blocked",
                "--tab",
                "w1:t2",
                "--commander",
                "blocked-cmd",
                "--cwd",
                ".",
            ]),
        )
        .unwrap();
        run(
            &state,
            &strings(&["set-status", "blocked", "working", "正在执行"]),
        )
        .unwrap();
        state.write_json("runtime.json", &json!({
            "connected": true,
            "projects": {
                "demo": {"online": true, "live_status": "idle", "live_status_since": "2020-01-01 00:00:00"},
                "blocked": {"online": true, "live_status": "blocked", "live_status_since": "2020-01-01 00:00:00"}
            }
        })).unwrap();
        run(
            &state,
            &strings(&[
                "ask",
                "demo",
                "是否发布？",
                "--option",
                "发布",
                "--option",
                "暂缓",
            ]),
        )
        .unwrap();
        let attention = state.refresh_attention().unwrap();
        assert_eq!(attention[0]["kind"], "decision_required");
        assert!(attention.iter().any(|item| item["kind"] == "stale_blocked"));
    }
}
