use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{Local, NaiveDateTime};
use fs2::FileExt;
use serde_json::{json, Map, Value};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub const PLUGIN_ID: &str = "sm-yjr.herdr-coordinator";
pub const STATUSES: &[&str] = &["working", "idle", "done", "blocked", "need_decision"];
pub const REPORTED_STATUSES: &[&str] = &["working", "idle", "done", "blocked"];

pub fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn utc_ms() -> i64 {
    Local::now().timestamp_millis()
}

pub fn short_uuid(len: usize) -> String {
    Uuid::new_v4().simple().to_string()[..len].to_string()
}

pub fn age_seconds(value: Option<&str>) -> i64 {
    let Some(value) = value else { return 0 };
    let parsed = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value).map(|v| v.naive_local()));
    parsed
        .map(|value| (Local::now().naive_local() - value).num_seconds().max(0))
        .unwrap_or(0)
}

pub fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

pub fn obj_mut(value: &mut Value) -> Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("状态文件不是 JSON object"))
}

#[derive(Clone, Debug)]
pub struct State {
    pub dir: PathBuf,
}

impl State {
    pub fn discover() -> Self {
        if let Some(path) = env::var_os("HERDR_PLUGIN_STATE_DIR") {
            return Self {
                dir: PathBuf::from(path),
            };
        }
        if let Some(path) = env::var_os("HERDR_COORDINATOR_HOME") {
            return Self {
                dir: PathBuf::from(path),
            };
        }
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local/state"));
        for app in ["herdr", "herdr-dev"] {
            let candidate = base.join(app).join("plugins").join(PLUGIN_ID);
            if candidate.is_dir() {
                return Self { dir: candidate };
            }
        }
        Self {
            dir: home_dir().join(".herdr-coordinator"),
        }
    }

    pub fn plugin_mode(&self) -> bool {
        env::var_os("HERDR_PLUGIN_STATE_DIR").is_some()
            || env::var_os("HERDR_PLUGIN_ID").is_some()
            || self
                .dir
                .components()
                .any(|part| part.as_os_str() == "plugins")
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("无法创建状态目录 {}", self.dir.display()))
    }

    pub fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.ensure_dir()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.path("state.lock"))?;
        file.lock_exclusive()?;
        let result = operation();
        let _ = FileExt::unlock(&file);
        result
    }

    pub fn load(&self, name: &str, default: Value) -> Value {
        fs::read_to_string(self.path(name))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(default)
    }

    pub fn write_json(&self, name: &str, value: &Value) -> Result<()> {
        self.ensure_dir()?;
        let mut temporary = NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(&mut temporary, value)?;
        temporary.write_all(b"\n")?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(self.path(name))
            .map_err(|error| anyhow!(error.error))?;
        Ok(())
    }

    pub fn append_inbox(&self, value: &Value) -> Result<()> {
        self.ensure_dir()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path("inbox.jsonl"))?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    pub fn init(&self) -> Result<()> {
        self.ensure_dir()?;
        self.with_lock(|| {
            for (name, default) in [
                ("fleets.json", json!({})),
                ("decisions.json", json!([])),
                ("claims.json", json!([])),
                ("attention.json", json!([])),
                ("controllers.json", json!([])),
                ("deliveries.json", json!([])),
            ] {
                if !self.path(name).exists() {
                    self.write_json(name, &default)?;
                }
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path("inbox.jsonl"))?;
            if !self.path("inbox.cursor").exists() {
                fs::write(self.path("inbox.cursor"), "0")?;
            }
            if !self.path("delivery-state.json").exists() {
                let attention = self
                    .load("attention.json", json!([]))
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.write_json(
                    "delivery-state.json",
                    &json!({
                        "inbox_offset": self.inbox_entries().len(),
                        "attention_active": attention
                            .iter()
                            .filter_map(crate::dispatch::attention_source_key)
                            .collect::<Vec<_>>()
                    }),
                )?;
            }
            Ok(())
        })
    }

    pub fn registry(&self) -> Map<String, Value> {
        let mut registry = self.load("fleets.json", json!({}));
        let Some(values) = registry.as_object_mut() else {
            return Map::new();
        };
        for entry in values.values_mut().filter_map(Value::as_object_mut) {
            let status = entry
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("idle");
            if !entry.contains_key("reported_status") {
                let reported = if REPORTED_STATUSES.contains(&status) {
                    status
                } else {
                    "idle"
                };
                entry.insert("reported_status".into(), json!(reported));
            }
            let claim_state = entry.get("claim_state").and_then(Value::as_str);
            if !matches!(claim_state, Some("reported" | "verified" | "accepted")) {
                entry.insert("claim_state".into(), Value::Null);
            }
        }
        values.clone()
    }

    pub fn decisions(&self) -> Vec<Value> {
        self.load("decisions.json", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn claims(&self) -> Vec<Value> {
        self.load("claims.json", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn controllers(&self) -> Vec<Value> {
        self.load("controllers.json", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn deliveries(&self) -> Vec<Value> {
        self.load("deliveries.json", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn migrate_legacy(&self) -> Result<()> {
        self.ensure_dir()?;
        self.with_lock(|| {
            let mut registry = self.registry();
            let mut decisions = self.decisions();
            let claims = self.claims();
            let mut changed = false;
            for (project, entry) in &mut registry {
                let Some(entry) = entry.as_object_mut() else {
                    continue;
                };
                let status = entry
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("idle")
                    .to_string();
                if !entry.contains_key("reported_status") {
                    let reported = if REPORTED_STATUSES.contains(&status.as_str()) {
                        status.as_str()
                    } else {
                        "idle"
                    };
                    entry.insert("reported_status".into(), json!(reported));
                }
                entry.entry("claim_id").or_insert(Value::Null);
                entry.entry("claim_state").or_insert(Value::Null);
                if status != "need_decision"
                    || decisions.iter().any(|item| {
                        string(item, "project") == *project && string(item, "state") == "open"
                    })
                {
                    continue;
                }
                let created = entry
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(now);
                decisions.push(json!({
                    "id": format!("{project}-legacy-{}", short_uuid(8)),
                    "project": project,
                    "state": "open",
                    "question": entry.get("note").and_then(Value::as_str).unwrap_or("需要用户拍板"),
                    "options": [],
                    "source": "legacy_registry_migration",
                    "created_at": created,
                    "resolved_at": null,
                    "resolution": null
                }));
                changed = true;
            }
            if changed || !self.path("decisions.json").exists() {
                self.write_json("decisions.json", &Value::Array(decisions))?;
            }
            if !self.path("claims.json").exists() {
                self.write_json("claims.json", &Value::Array(claims))?;
            }
            if changed {
                self.write_json("fleets.json", &Value::Object(registry))?;
            }
            Ok(())
        })
    }

    pub fn import_legacy_once(&self) -> Result<Vec<String>> {
        if env::var_os("HERDR_PLUGIN_STATE_DIR").is_none() {
            return Ok(Vec::new());
        }
        let legacy = env::var_os("HERDR_COORDINATOR_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".herdr-coordinator"));
        if !legacy.is_dir()
            || same_path(&legacy, &self.dir)
            || self.path("legacy-import.json").exists()
        {
            return Ok(Vec::new());
        }
        self.ensure_dir()?;
        self.with_lock(|| {
            if self.path("legacy-import.json").exists() {
                return Ok(Vec::new());
            }
            let mut imported = Vec::new();
            for name in [
                "fleets.json",
                "claims.json",
                "decisions.json",
                "attention.json",
                "inbox.jsonl",
                "inbox.cursor",
                "controllers.json",
                "deliveries.json",
                "delivery-state.json",
                "dispatch.json",
                "summaries.json",
            ] {
                let source = legacy.join(name);
                let target = self.path(name);
                if source.is_file() && !target.exists() {
                    fs::copy(&source, &target)?;
                    imported.push(name.to_string());
                }
            }
            self.write_json(
                "legacy-import.json",
                &json!({
                    "source": fs::canonicalize(&legacy).unwrap_or(legacy.clone()),
                    "imported": imported,
                    "imported_at_unix_ms": utc_ms()
                }),
            )?;
            Ok(imported)
        })
    }

    pub fn write_runtime(
        &self,
        snapshot: &Value,
        connected: bool,
        error: Option<String>,
        source: &str,
        last_event: Option<Value>,
    ) -> Result<Value> {
        let registry = self.registry();
        let previous = self.load("runtime.json", json!({}));
        let previous_projects = previous
            .get("projects")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let agents = snapshot
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let by_name: HashMap<String, &Value> = agents
            .iter()
            .filter_map(|agent| {
                agent
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| (name.to_string(), agent))
            })
            .collect();
        let observed = now();
        let mut projects = Map::new();
        for (project, entry) in registry {
            let commander = string(&entry, "commander");
            let agent = by_name.get(&commander).copied();
            let live_status = agent
                .map(|a| string(a, "agent_status"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "offline".into());
            let pane_id = agent
                .and_then(|a| a.get("pane_id"))
                .cloned()
                .unwrap_or(Value::Null);
            let old = previous_projects
                .get(&project)
                .cloned()
                .unwrap_or_else(|| json!({}));
            let unchanged = string(&old, "live_status") == live_status
                && old.get("pane_id").unwrap_or(&Value::Null) == &pane_id
                && old.get("online").and_then(Value::as_bool).unwrap_or(false) == agent.is_some();
            let since = if unchanged {
                old.get("live_status_since")
                    .and_then(Value::as_str)
                    .unwrap_or(&observed)
                    .to_string()
            } else {
                observed.clone()
            };
            projects.insert(project, json!({
                "commander": commander,
                "online": agent.is_some(),
                "live_status": live_status,
                "live_status_since": since,
                "pane_id": pane_id,
                "workspace_id": agent.and_then(|a| a.get("workspace_id")).cloned().unwrap_or(Value::Null),
                "state_labels": agent.and_then(|a| a.get("state_labels")).cloned().unwrap_or_else(|| json!({})),
                "observed_at": observed
            }));
        }
        let runtime = json!({
            "connected": connected,
            "error": error,
            "source": source,
            "updated_at": observed,
            "herdr_version": snapshot.get("version").cloned().unwrap_or(Value::Null),
            "protocol": snapshot.get("protocol").cloned().unwrap_or(Value::Null),
            "last_event": last_event,
            "agents": agents,
            "projects": projects
        });
        self.write_json("runtime.json", &runtime)?;
        Ok(runtime)
    }

    pub fn refresh_attention(&self) -> Result<Vec<Value>> {
        self.with_lock(|| {
            let values = compute_attention(
                &self.registry(),
                &self.decisions(),
                &self.claims(),
                &self.load("runtime.json", json!({})),
            );
            self.write_json("attention.json", &Value::Array(values.clone()))?;
            Ok(values)
        })
    }

    pub fn inbox_entries(&self) -> Vec<Value> {
        let Ok(file) = File::open(self.path("inbox.jsonl")) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn same_path(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left).ok() == fs::canonicalize(right).ok()
}

pub fn open_decisions<'a>(decisions: &'a [Value], project: Option<&str>) -> Vec<&'a Value> {
    decisions
        .iter()
        .filter(|item| {
            string(item, "state") == "open"
                && project
                    .map(|name| string(item, "project") == name)
                    .unwrap_or(true)
        })
        .collect()
}

pub fn effective_status(entry: &Value, decisions: &[Value], project: &str) -> String {
    if !open_decisions(decisions, Some(project)).is_empty() {
        return "need_decision".into();
    }
    let status = entry
        .get("reported_status")
        .or_else(|| entry.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("idle");
    if REPORTED_STATUSES.contains(&status) {
        status.into()
    } else {
        "idle".into()
    }
}

pub fn latest_claim<'a>(
    claims: &'a [Value],
    project: &str,
    claim_id: Option<&str>,
) -> Option<&'a Value> {
    if let Some(id) = claim_id {
        return claims
            .iter()
            .find(|claim| string(claim, "project") == project && string(claim, "id") == id);
    }
    claims
        .iter()
        .rev()
        .find(|claim| string(claim, "project") == project)
}

pub fn latest_claim_mut<'a>(
    claims: &'a mut [Value],
    project: &str,
    claim_id: Option<&str>,
) -> Option<&'a mut Value> {
    if let Some(id) = claim_id {
        return claims
            .iter_mut()
            .find(|claim| string(claim, "project") == project && string(claim, "id") == id);
    }
    claims
        .iter_mut()
        .rev()
        .find(|claim| string(claim, "project") == project)
}

#[allow(clippy::too_many_arguments)]
fn attention(
    project: &str,
    kind: &str,
    priority: i64,
    reason: String,
    actor: &str,
    created_at: Option<&str>,
    action: String,
    reference: Option<String>,
) -> Value {
    let created = created_at.map(str::to_string).unwrap_or_else(now);
    json!({
        "id": format!("{kind}:{project}:{}", reference.as_deref().unwrap_or(project)),
        "project": project,
        "kind": kind,
        "priority": priority,
        "reason": reason,
        "required_actor": actor,
        "created_at": created,
        "age_seconds": age_seconds(Some(&created)),
        "action": action,
        "reference_id": reference
    })
}

pub fn compute_attention(
    registry: &Map<String, Value>,
    decisions: &[Value],
    claims: &[Value],
    runtime: &Value,
) -> Vec<Value> {
    let mut items = Vec::new();
    let mut decisions_by_project: HashMap<String, usize> = HashMap::new();
    for decision in open_decisions(decisions, None) {
        let project = string(decision, "project");
        *decisions_by_project.entry(project.clone()).or_default() += 1;
        let id = string(decision, "id");
        items.push(attention(
            &project,
            "decision_required",
            100,
            optional_string(decision, "question").unwrap_or_else(|| "需要用户拍板".into()),
            "user",
            decision.get("created_at").and_then(Value::as_str),
            format!("fleet resolve {id} <答案>"),
            Some(id),
        ));
    }
    if !runtime.as_object().map(Map::is_empty).unwrap_or(true)
        && !runtime
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && !registry.is_empty()
    {
        items.push(attention(
            "*",
            "runtime_disconnected",
            95,
            optional_string(runtime, "error").unwrap_or_else(|| "Herdr 实时状态连接中断".into()),
            "coordinator",
            runtime.get("updated_at").and_then(Value::as_str),
            "fleet plugin-reconcile".into(),
            None,
        ));
    }
    let runtime_projects = runtime.get("projects").and_then(Value::as_object);
    for (project, entry) in registry {
        let status = effective_status(entry, decisions, project);
        let live = runtime_projects.and_then(|values| values.get(project));
        let has_decision = decisions_by_project.contains_key(project);
        if runtime
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && matches!(status.as_str(), "working" | "blocked")
        {
            if let Some(live) = live {
                let live_since = live
                    .get("live_status_since")
                    .or_else(|| live.get("observed_at"))
                    .and_then(Value::as_str);
                if !live.get("online").and_then(Value::as_bool).unwrap_or(false) {
                    items.push(attention(
                        project,
                        "commander_offline",
                        88,
                        format!(
                            "项目仍标记为 {status}，但机长 {} 已离线",
                            string(entry, "commander")
                        ),
                        "coordinator",
                        live_since,
                        format!("fleet route {project} <恢复或重新指派指令>"),
                        None,
                    ));
                } else if string(live, "live_status") == "blocked" && !has_decision {
                    let stale = age_seconds(live_since) >= 600;
                    items.push(attention(
                        project,
                        if stale {
                            "stale_blocked"
                        } else {
                            "observed_blocked"
                        },
                        if stale { 90 } else { 82 },
                        if stale {
                            "机长持续阻塞超过 10 分钟，且尚未创建正式决策".into()
                        } else {
                            "机长实时状态为 blocked，但尚未创建正式决策".into()
                        },
                        "coordinator",
                        live_since,
                        format!(
                            "fleet route {project} 请说明阻塞原因；需要用户拍板时运行 fleet ask"
                        ),
                        None,
                    ));
                } else if status == "working"
                    && string(live, "live_status") == "idle"
                    && age_seconds(live_since) >= 300
                {
                    items.push(attention(
                        project,
                        "project_idle_mismatch",
                        52,
                        "项目仍标记 working，但机长已空闲超过 5 分钟".into(),
                        "coordinator",
                        live_since,
                        format!("fleet route {project} 请汇报当前项目状态"),
                        None,
                    ));
                }
            }
        }
        if status == "done" {
            let claim_id = entry.get("claim_id").and_then(Value::as_str);
            let claim = latest_claim(claims, project, claim_id);
            match claim
                .and_then(|claim| claim.get("state"))
                .and_then(Value::as_str)
            {
                None | Some("reported") => {
                    let id = claim
                        .map(|value| string(value, "id"))
                        .filter(|value| !value.is_empty());
                    let created = claim
                        .and_then(|value| value.get("created_at"))
                        .or_else(|| entry.get("updated_at"))
                        .and_then(Value::as_str);
                    let action = id
                        .as_ref()
                        .map(|id| {
                            format!(
                                "fleet verify {project} --claim {id} --label <标签> -- <验证命令>"
                            )
                        })
                        .unwrap_or_else(|| {
                            format!("fleet report {project} done <摘要> 后运行 fleet verify")
                        });
                    items.push(attention(
                        project,
                        "reported_done_unverified",
                        72,
                        "机长声称完成，但尚无通过的机器验证证据".into(),
                        "coordinator",
                        created,
                        action,
                        id,
                    ));
                }
                Some("verified") => {
                    let claim = claim.expect("verified claim");
                    let id = string(claim, "id");
                    let created = claim
                        .get("verified_at")
                        .or_else(|| claim.get("created_at"))
                        .and_then(Value::as_str);
                    items.push(attention(
                        project,
                        "ready_for_acceptance",
                        62,
                        "完成声明已通过机器验证，等待用户验收".into(),
                        "user",
                        created,
                        format!("fleet accept {project} --claim {id}"),
                        Some(id),
                    ));
                }
                _ => {}
            }
        }
    }
    items.sort_by(|left, right| {
        let key = |value: &Value| {
            (
                value.get("priority").and_then(Value::as_i64).unwrap_or(0),
                value
                    .get("age_seconds")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            )
        };
        let left_key = key(left);
        let right_key = key(right);
        right_key
            .cmp(&left_key)
            .then_with(|| string(left, "project").cmp(&string(right, "project")))
            .then_with(|| string(left, "kind").cmp(&string(right, "kind")))
    });
    items
}

pub fn registry_as_value(registry: Map<String, Value>) -> Value {
    Value::Object(registry)
}

pub fn sorted_registry(registry: &Map<String, Value>) -> BTreeMap<String, Value> {
    registry
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_projection_never_overwrites_business_state() {
        let temporary = tempfile::tempdir().unwrap();
        let state = State {
            dir: temporary.path().join("state"),
        };
        state.init().unwrap();
        state
            .write_json(
                "fleets.json",
                &json!({"demo": {
                    "commander": "demo-cmd", "status": "done", "reported_status": "done",
                    "updated_at": now()
                }}),
            )
            .unwrap();
        state
            .write_runtime(
                &json!({"version": "test", "protocol": 20, "agents": [{
                    "name": "demo-cmd", "pane_id": "w1:p1", "agent_status": "working"
                }]}),
                true,
                None,
                "test",
                None,
            )
            .unwrap();
        assert_eq!(state.registry()["demo"]["reported_status"], "done");
        assert_eq!(
            state.load("runtime.json", json!({}))["projects"]["demo"]["live_status"],
            "working"
        );
    }

    #[test]
    fn atomic_state_initialization_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let state = State {
            dir: temporary.path().join("state"),
        };
        state.init().unwrap();
        state.init().unwrap();
        assert!(state.path("fleets.json").is_file());
        assert!(state.path("claims.json").is_file());
        assert!(state.path("attention.json").is_file());
    }
}
