//! gray-repl — persistent per-session CPython namespace as a wire tool.
//!
//! Port of prime-agent's "rlm" kernel mechanics (MIT — mechanics, not code):
//! one long-lived `python3 -u -c <driver>` child per session executes cells in
//! a single globals dict, so tool results, file contents and intermediate
//! values live as variables *outside* the message context and later cells
//! compose over them (the PTC / "prompt-as-a-variable" enabler).
//!
//! Differences from prime's kernel (documented in README): sync exec only (no
//! top-level await, no background tasks), no display/MIME side channel, no
//! per-cell stream attribution, pickle (not dill) snapshots, and a cell timeout
//! kills the whole kernel — prime raises KeyboardInterrupt instead.
//!
//! Wire tools: repl_exec{code,timeout_ms?} repl_reset repl_vars
//!             repl_state{action:save|load,path?}
//! Command:    /repl status|vars|reset|save|load
//! State:      ~/.gray/repl/<session>.pkl snapshots.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

const DRIVER: &str = r#"
import sys, os, io, ast, json, pickle, builtins, linecache, traceback

G = {"__name__": "__main__"}
_wire = sys.stdout
_log = sys.stderr
_skip = {"In", "Out", "get_ipython", "exit", "quit", "open"}
_cell = [0]
_CAP = 1048576
_mark = "\n...[repr truncated at 1MiB]"


def _say(o):
    _wire.write(json.dumps(o) + "\n")
    _wire.flush()


def _cap(s):
    return s[:_CAP] + _mark if len(s) > _CAP else s


def _names():
    return sorted(k for k in G if not (k.startswith("_") or k in _skip))


class _Drop:
    # Writes outside a cell (threads, strays) must never corrupt the wire:
    # they go to the process log instead, like prime's id:null channel.
    def write(self, t):
        _log.write(t)
        _log.flush()
        return len(t)

    def flush(self):
        pass


def _exec(code):
    _cell[0] += 1
    name = "<cell-%d>" % _cell[0]
    out, err = io.StringIO(), io.StringIO()
    linecache.cache[name] = (len(code), None, code.splitlines(True), name)
    oout, oerr = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = out, err
    try:
        # Multi-statement cells: exec the body; when the last node is an
        # expression, eval it and ship the repr (IPython semantics — the
        # value also lands on builtins._).
        tree = ast.parse(code, filename=name, mode="exec")
        last = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last = ast.Expression(tree.body.pop().value)
            ast.fix_missing_locations(last)
        exec(compile(tree, name, "exec"), G)
        value = None
        have = False
        if last is not None:
            v = eval(compile(last, name, "eval"), G)
            if v is not None:
                builtins._ = v
                value = _cap(repr(v))
                have = True
        r = {"stdout": out.getvalue(), "stderr": err.getvalue()}
        if have:
            r["result"] = value
        return r
    except BaseException:
        ei = sys.exc_info()
        tb = [t for t in traceback.format_exception(*ei)
              if 'File "<string>"' not in t]
        return {"stdout": out.getvalue(), "stderr": err.getvalue(),
                "error": ei[0].__name__, "evalue": _cap(str(ei[1])),
                "traceback": [_cap(t) for t in tb]}
    finally:
        sys.stdout, sys.stderr = oout, oerr


def _handle(req):
    if "code" in req:
        return _exec(req["code"])
    if req.get("vars"):
        return {"names": [{"name": k, "type": type(G[k]).__name__}
                          for k in _names()]}
    if req.get("reset"):
        G.clear()
        G["__name__"] = "__main__"
        return {"reset": True}
    if "save" in req:
        path = req["save"]
        payload, skipped = {}, []
        for k in _names():
            try:
                pickle.dumps(G[k])
                payload[k] = G[k]
            except Exception:
                skipped.append(k)
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "wb") as f:
            pickle.dump(payload, f)
        os.replace(tmp, path)
        return {"saved": sorted(payload), "skipped": skipped, "path": path}
    if "load" in req:
        path = req["load"]
        if not os.path.exists(path):
            return {"restored": [], "failed": [], "reason": "snapshot not found"}
        try:
            with open(path, "rb") as f:
                data = pickle.load(f)
        except Exception as e:
            return {"restored": [], "failed": [],
                    "reason": "corrupt snapshot: %s" % e}
        restored, failed = [], []
        for k, v in data.items():
            if k in ("In", "Out", "get_ipython"):
                continue
            try:
                G[k] = v
                restored.append(k)
            except Exception:
                failed.append(k)
        return {"restored": sorted(restored), "failed": failed}
    return {"error": "ProtocolError", "evalue": "unknown request shape"}


def main():
    sys.stdout = _Drop()
    sys.stderr = _Drop()
    _say({"event": "ready", "protocol": 1})
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception:
            _say({"id": None, "error": "ProtocolError",
                  "evalue": "malformed line"})
            continue
        try:
            r = _handle(req)
        except BaseException:
            ei = sys.exc_info()
            r = {"error": ei[0].__name__, "evalue": _cap(str(ei[1]))}
        r["id"] = req.get("id")
        _say(r)


main()
"#;

fn manifest() -> Value {
    json!({
        "name": "repl",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [
            {"name": "repl_exec",
             "description": "Run a Python cell in this session's persistent namespace. Variables, imports and values survive across calls — assign large tool results or file contents to names and slice them programmatically in later cells instead of re-reading them into context. Trailing expressions return their repr (also bound to `_`).",
             "parameters": {"type": "object", "properties": {
                "code": {"type": "string", "description": "Python source for one cell"},
                "timeout_ms": {"type": "integer", "description": "cell timeout (default 30000); on expiry the kernel is killed and the namespace is lost"}},
             "required": ["code"]}},
            {"name": "repl_reset",
             "description": "Kill this session's Python kernel. The next repl_exec starts a fresh empty namespace.",
             "parameters": {"type": "object", "properties": {}}},
            {"name": "repl_vars",
             "description": "List the user-defined names alive in this session's persistent namespace, with types.",
             "parameters": {"type": "object", "properties": {}}},
            {"name": "repl_state",
             "description": "Snapshot or restore this session's namespace (pickle). action=save|load, optional path (default ~/.gray/repl/<session>.pkl).",
             "parameters": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["save", "load"]},
                "path": {"type": "string", "description": "snapshot file (default per-session path under ~/.gray/repl/)"}},
             "required": ["action"]}},
        ],
        "commands": ["/repl"],
        "subcommands": ["repl"],
    })
}

// ---------- paths ----------

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn state_dir() -> PathBuf {
    gray_home().join("repl")
}

fn sanitize(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    if s.is_empty() { "default".into() } else { s }
}

fn default_snapshot(sid: &str) -> PathBuf {
    state_dir().join(format!("{}.pkl", sanitize(sid)))
}

fn session_id(params: &Value) -> String {
    params
        .pointer("/session/id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string()
}

// ---------- kernel ----------

struct Kernel {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    seq: u64,
}

impl Kernel {
    fn spawn() -> Result<Kernel, String> {
        let mut child = Command::new("python3")
            .args(["-u", "-c", DRIVER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!("cannot start python3 ({e}) — gray-repl needs CPython on PATH")
            })?;
        let stdout = child.stdout.take().ok_or("no kernel stdout")?;
        let stdin = child.stdin.take().ok_or("no kernel stdin")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        // drain the ready event so it can't be mistaken for a cell reply
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(v) if v.get("event").and_then(Value::as_str) == Some("ready") => {}
            _ => {
                let _ = child.kill();
                return Err("kernel did not announce ready — is python3 a real CPython?".into());
            }
        }
        Ok(Kernel { child, stdin, rx, seq: 0 })
    }

    /// One request round-trip. On timeout or a dead channel the child is
    /// killed (prime raises KeyboardInterrupt; killing is the sidecar-safe
    /// approximation) and the kernel reports dead so the next call respawns.
    fn call(&mut self, mut req: Value, timeout: Duration) -> Result<Value, String> {
        self.seq += 1;
        req["id"] = json!(self.seq);
        let want = self.seq;
        writeln!(self.stdin, "{req}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("kernel stdin write failed ({e}) — kernel dead"))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                let _ = self.child.kill();
                return Err(format!(
                    "cell timed out after {}ms — kernel killed; namespace lost (next call starts fresh)",
                    timeout.as_millis()
                ));
            }
            match self.rx.recv_timeout(left.min(Duration::from_secs(60))) {
                Ok(v) if v.get("id").and_then(Value::as_u64) == Some(want) => return Ok(v),
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("kernel stdout closed — kernel dead".into())
                }
            }
        }
    }
}

fn kernel<'m>(
    map: &'m mut HashMap<String, Kernel>,
    sid: &str,
) -> Result<&'m mut Kernel, String> {
    if !map.contains_key(sid) {
        map.insert(sid.to_string(), Kernel::spawn()?);
    }
    Ok(map.get_mut(sid).unwrap())
}

fn render_exec(v: &Value) -> (String, bool) {
    let mut out = String::new();
    if let Some(s) = v["stdout"].as_str().filter(|s| !s.is_empty()) {
        out.push_str(s);
    }
    if let Some(s) = v["stderr"].as_str().filter(|s| !s.is_empty()) {
        if !out.is_empty() { out.push('\n'); }
        out.push_str("[stderr]\n");
        out.push_str(s);
    }
    if let Some(e) = v["error"].as_str() {
        if !out.is_empty() { out.push('\n'); }
        out.push_str(&format!(
            "{e}: {}\n{}",
            v["evalue"].as_str().unwrap_or(""),
            v["traceback"]
                .as_array()
                .map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(""))
                .unwrap_or_default()
        ));
        return (out.trim_end().to_string(), true);
    }
    if let Some(r) = v["result"].as_str() {
        if !out.is_empty() { out.push('\n'); }
        out.push_str(r);
    }
    if out.is_empty() {
        out.push_str("(cell completed, no output)");
    }
    (out.trim_end().to_string(), false)
}

// ---------- tools ----------

fn tool(map: &mut HashMap<String, Kernel>, name: &str, args: &Value, params: &Value) -> Value {
    let sid = session_id(params);
    let res: Result<String, String> = (|| {
        match name {
            "repl_exec" => {
                let code = args["code"].as_str().ok_or("repl_exec needs code")?;
                let ms = args["timeout_ms"].as_u64().unwrap_or(30_000).clamp(100, 600_000);
                let v = kernel(map, &sid)?.call(json!({"code": code}), Duration::from_millis(ms))?;
                let (text, is_err) = render_exec(&v);
                if is_err {
                    return Err(text); // is_error result below
                }
                return Ok(text);
            }
            "repl_reset" => {
                if let Some(mut k) = map.remove(&sid) {
                    let _ = k.child.kill();
                }
                Ok("kernel reset — next repl_exec starts a fresh namespace".into())
            }
            "repl_vars" => {
                let v = kernel(map, &sid)?.call(json!({"vars": true}), Duration::from_secs(10))?;
                let names = v["names"].as_array().cloned().unwrap_or_default();
                if names.is_empty() {
                    return Ok("namespace is empty".into());
                }
                Ok(names
                    .iter()
                    .map(|n| format!("{} : {}", n["name"].as_str().unwrap_or("?"),
                                     n["type"].as_str().unwrap_or("?")))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "repl_state" => {
                let action = args["action"].as_str().unwrap_or("");
                let path = args["path"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| default_snapshot(&sid).to_string_lossy().to_string());
                let req = match action {
                    "save" => json!({"save": path}),
                    "load" => json!({"load": path}),
                    _ => return Err("action must be save|load".into()),
                };
                let v = kernel(map, &sid)?.call(req, Duration::from_secs(60))?;
                if let Some(r) = v["reason"].as_str() {
                    return Ok(format!("{action}: {r}"));
                }
                Ok(serde_json::to_string_pretty(&v).unwrap_or_default())
            }
            t => return Err(format!("unknown tool {t}")),
        }
    })();
    match res {
        Ok(c) => json!({"result": {"content": c}}),
        Err(e) => json!({"result": {"content": e, "is_error": true}}),
    }
}

// ---------- command ----------

fn cmd(map: &mut HashMap<String, Kernel>, argv: &[String], params: &Value) -> String {
    let sid = session_id(params);
    match argv.first().map(String::as_str) {
        Some("reset") => {
            if let Some(mut k) = map.remove(&sid) {
                let _ = k.child.kill();
            }
            "kernel reset".into()
        }
        Some("vars") => tool(map, "repl_vars", &json!({}), params)["result"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        Some("save") | Some("load") => tool(
            map,
            "repl_state",
            &json!({"action": argv[0], "path": argv.get(1).cloned().unwrap_or_default()}),
            params,
        )["result"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        Some("status") | None => {
            let live = map.contains_key(&sid);
            format!(
                "repl session {sid}\n  kernel: {}\n  snapshot: {}",
                if live { "running" } else { "not started" },
                default_snapshot(&sid).display()
            )
        }
        Some(o) => format!("unknown verb {o} — /repl status|vars|reset|save [path]|load [path]"),
    }
}

// ---------- wire ----------

fn sidecar() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut kernels: HashMap<String, Kernel> = HashMap::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let method = v["method"].as_str().unwrap_or("");
        match v.get("id").cloned() {
            None if method == "plugin/shutdown" => break,
            None => {}
            Some(id) => {
                let reply = match method {
                    "plugin/manifest" => json!({"id": id, "result": manifest()}),
                    "tool/call" => {
                        let mut r = tool(
                            &mut kernels,
                            v["params"]["name"].as_str().unwrap_or(""),
                            &v["params"]["args"],
                            &v["params"],
                        );
                        r["id"] = id;
                        r
                    }
                    "command/run" => {
                        let argv: Vec<String> = v["params"]["argv"]
                            .as_array()
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        let text = cmd(&mut kernels, &argv, &v["params"]);
                        json!({"id": id, "result": {"text": text}})
                    }
                    other => json!({"id": id, "error": {"code": -32601,
                        "message": format!("method not found: {other}")}}),
                };
                writeln!(out, "{reply}")?;
                out.flush()?;
            }
        }
    }
    // drop kernels: kills the python children
    kernels.clear();
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("manifest") => println!("{}", manifest()),
        Some("sidecar") => {
            let _ = sidecar();
        }
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                println!("repl — persistent Python namespace sidecar; install as a gray plugin");
                return;
            }
            let _ = sidecar();
        }
        _ => println!("repl — the live kernel runs inside the gray sidecar; use /repl status|vars|reset|save|load in-session"),
    }
}
