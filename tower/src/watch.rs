use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::flag;

use crate::state::{home_dir, State};

const TOPOLOGY_EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.closed",
    "tab.created",
    "tab.closed",
    "pane.created",
    "pane.closed",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
];

pub fn run(state: &State, command: &str, args: &[String]) -> Result<i32> {
    if !args.is_empty() {
        bail!(crate::commands::USAGE)
    }
    match command {
        "watch" => foreground(state),
        "watch-start" => start(state),
        "watch-status" => status(state),
        "watch-stop" => stop(state),
        _ => unreachable!(),
    }
}

pub fn pid(state: &State) -> Option<i32> {
    let pid = fs::read_to_string(state.path("watch.pid"))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    if unsafe { libc::kill(pid, 0) } != 0 {
        return None;
    }
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&output.stdout);
    (command.contains("herdr-coordinator") && command.contains("watch")).then_some(pid)
}

pub fn notify(state: &State) {
    if let Some(pid) = pid(state) {
        unsafe {
            libc::kill(pid, SIGHUP);
        }
    }
}

fn foreground(state: &State) -> Result<i32> {
    if state.plugin_mode() {
        bail!("Plugin 模式由 Herdr startup/event hooks 维护实时状态，无需 fleet watch")
    }
    state.ensure_dir()?;
    if let Some(existing) = pid(state) {
        if existing != std::process::id() as i32 {
            bail!("事件监听器已在运行（pid {existing}）")
        }
    }
    fs::write(state.path("watch.pid"), std::process::id().to_string())?;
    let previous = state.load("runtime.json", json!({}));
    let baseline = json!({
        "agents": previous.get("agents").cloned().unwrap_or_else(|| json!([])),
        "version": previous.get("herdr_version").cloned().unwrap_or(Value::Null),
        "protocol": previous.get("protocol").cloned().unwrap_or(Value::Null)
    });
    state.write_runtime(
        &baseline,
        false,
        Some("正在建立 Herdr 事件订阅".into()),
        "watch",
        None,
    )?;
    let reload = Arc::new(AtomicBool::new(false));
    let terminate = Arc::new(AtomicBool::new(false));
    flag::register(SIGHUP, Arc::clone(&reload))?;
    flag::register(SIGTERM, Arc::clone(&terminate))?;
    let mut delay = Duration::from_millis(250);
    while !terminate.load(Ordering::Relaxed) {
        match watch_connection(state, &reload, &terminate) {
            Ok(()) => delay = Duration::from_millis(250),
            Err(error) => {
                if terminate.load(Ordering::Relaxed) {
                    break;
                }
                let current = state.load("runtime.json", json!({}));
                let snapshot = json!({
                    "agents": current.get("agents").cloned().unwrap_or_else(|| json!([])),
                    "version": current.get("herdr_version").cloned().unwrap_or(Value::Null),
                    "protocol": current.get("protocol").cloned().unwrap_or(Value::Null)
                });
                state.write_runtime(&snapshot, false, Some(error.to_string()), "watch", None)?;
                state.refresh_attention()?;
                let slices = (delay.as_millis() / 50).max(1);
                for _ in 0..slices {
                    if terminate.load(Ordering::Relaxed) || reload.swap(false, Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
    if fs::read_to_string(state.path("watch.pid"))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        == Some(std::process::id())
    {
        let _ = fs::remove_file(state.path("watch.pid"));
    }
    Ok(0)
}

fn start(state: &State) -> Result<i32> {
    if state.plugin_mode() {
        println!("Plugin 模式由 Herdr startup/event hooks 维护实时状态，无独立 watcher");
        return Ok(0);
    }
    if let Some(pid) = pid(state) {
        println!("事件监听器已在运行（pid {pid}）");
        return Ok(0);
    }
    state.ensure_dir()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.path("watch.log"))?;
    let error_log = log.try_clone()?;
    Command::new(env::current_exe()?)
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()?;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        if let Some(pid) = pid(state) {
            println!("事件监听器已启动（pid {pid}）");
            return Ok(0);
        }
    }
    bail!(
        "事件监听器启动失败；查看 {}",
        state.path("watch.log").display()
    )
}

fn status(state: &State) -> Result<i32> {
    if state.plugin_mode() {
        let runtime = state.load("runtime.json", json!({}));
        let connection = if runtime
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "已连接 Herdr"
        } else {
            "等待 snapshot 对账"
        };
        println!("Plugin 事件模式（{connection}，无独立 watcher）");
        return Ok(0);
    }
    let Some(pid) = pid(state) else {
        println!("事件监听器未运行");
        return Ok(0);
    };
    let runtime = state.load("runtime.json", json!({}));
    let connection = if runtime
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "已连接 Herdr"
    } else {
        "正在重连 Herdr"
    };
    println!("事件监听器运行中（pid {pid}，{connection}）");
    Ok(0)
}

fn stop(state: &State) -> Result<i32> {
    let Some(pid) = pid(state) else {
        println!("事件监听器未运行");
        return Ok(0);
    };
    unsafe {
        libc::kill(pid, SIGTERM);
    }
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        if self::pid(state).is_none() {
            println!("事件监听器已停止");
            return Ok(0);
        }
    }
    bail!("事件监听器没有及时停止")
}

fn socket_path() -> PathBuf {
    if let Some(path) = env::var_os("HERDR_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    let config = env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config/herdr/config.toml"));
    let base = config.parent().unwrap_or_else(|| std::path::Path::new("."));
    match env::var("HERDR_SESSION") {
        Ok(session) if session != "default" => {
            base.join("sessions").join(session).join("herdr.sock")
        }
        _ => base.join("herdr.sock"),
    }
}

fn send(writer: &mut BufWriter<UnixStream>, id: &str, method: &str, params: Value) -> Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &json!({"id": id, "method": method, "params": params}),
    )?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_response(reader: &mut BufReader<UnixStream>, id: &str) -> Result<(Value, Vec<Value>)> {
    let mut pending = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("Herdr socket 已关闭")
        }
        let value: Value = serde_json::from_str(&line)?;
        if value.get("id").and_then(Value::as_str) == Some(id) {
            if let Some(error) = value.get("error") {
                bail!("{error}")
            }
            return Ok((value, pending));
        }
        pending.push(value);
    }
}

fn socket_snapshot() -> Result<Value> {
    let stream = UnixStream::connect(socket_path()).context("无法连接 Herdr socket")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    send(&mut writer, "tower_snapshot", "session.snapshot", json!({}))?;
    let (response, _) = read_response(&mut reader, "tower_snapshot")?;
    response
        .pointer("/result/snapshot")
        .cloned()
        .ok_or_else(|| anyhow!("Herdr 未返回 session snapshot"))
}

fn subscriptions(snapshot: &Value) -> Value {
    let mut values: Vec<Value> = TOPOLOGY_EVENTS
        .iter()
        .map(|kind| json!({"type": kind}))
        .collect();
    let mut panes: Vec<&str> = snapshot
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| agent.get("pane_id").and_then(Value::as_str))
        .collect();
    panes.sort_unstable();
    panes.dedup();
    values.extend(
        panes
            .into_iter()
            .map(|pane| json!({"type": "pane.agent_status_changed", "pane_id": pane})),
    );
    Value::Array(values)
}

fn event_name(message: &Value) -> String {
    let raw = message
        .get("event")
        .or_else(|| message.pointer("/data/type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if raw.contains('.') {
        return raw.to_string();
    }
    TOPOLOGY_EVENTS
        .iter()
        .copied()
        .chain(std::iter::once("pane.agent_status_changed"))
        .find(|canonical| canonical.replace('.', "_") == raw)
        .unwrap_or(raw)
        .to_string()
}

fn apply_status(snapshot: &mut Value, message: &Value) -> bool {
    if event_name(message) != "pane.agent_status_changed" {
        return false;
    }
    let Some(pane_id) = message.pointer("/data/pane_id").and_then(Value::as_str) else {
        return false;
    };
    let data = message.get("data").cloned().unwrap_or_else(|| json!({}));
    for collection in ["agents", "panes"] {
        for item in snapshot
            .get_mut(collection)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if item.get("pane_id").and_then(Value::as_str) != Some(pane_id) {
                continue;
            }
            if let Some(object) = item.as_object_mut() {
                for field in [
                    "agent_status",
                    "agent",
                    "display_agent",
                    "title",
                    "state_labels",
                ] {
                    if let Some(value) = data.get(field) {
                        object.insert(field.into(), value.clone());
                    }
                }
            }
        }
    }
    true
}

fn watch_connection(state: &State, reload: &AtomicBool, terminate: &AtomicBool) -> Result<()> {
    let mut snapshot = socket_snapshot()?;
    let stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    send(
        &mut writer,
        "tower_subscribe",
        "events.subscribe",
        json!({"subscriptions": subscriptions(&snapshot)}),
    )?;
    let (_, early) = read_response(&mut reader, "tower_subscribe")?;
    state.write_runtime(&snapshot, true, None, "watch", None)?;
    state.refresh_attention()?;
    for message in early {
        if apply_status(&mut snapshot, &message) {
            state.write_runtime(&snapshot, true, None, "watch", Some(message))?;
            state.refresh_attention()?;
        }
    }
    loop {
        if terminate.load(Ordering::Relaxed) {
            return Ok(());
        }
        if reload.swap(false, Ordering::Relaxed) {
            bail!("reload")
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("Herdr socket 已关闭"),
            Ok(_) => {
                let message: Value = serde_json::from_str(&line)?;
                let event = message
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if TOPOLOGY_EVENTS.contains(&event) {
                    bail!("topology changed")
                }
                if apply_status(&mut snapshot, &message) {
                    state.write_runtime(&snapshot, true, None, "watch", Some(message))?;
                    state.refresh_attention()?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_event_aliases_preserve_underscores_inside_segments() {
        assert_eq!(
            event_name(&json!({"event": "pane_agent_status_changed"})),
            "pane.agent_status_changed"
        );
        assert_eq!(
            event_name(&json!({"event": "pane_agent_detected"})),
            "pane.agent_detected"
        );
    }

    #[test]
    fn status_events_only_mutate_the_runtime_snapshot() {
        let mut snapshot = json!({"agents": [{"pane_id": "w1:p1", "agent_status": "idle"}]});
        assert!(apply_status(
            &mut snapshot,
            &json!({"event": "pane.agent_status_changed", "data": {"pane_id": "w1:p1", "agent_status": "working"}})
        ));
        assert_eq!(snapshot["agents"][0]["agent_status"], "working");
    }
}
