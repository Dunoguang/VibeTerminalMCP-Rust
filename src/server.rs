use serde_json::{json, Value};
use tracing::warn;

use crate::terminal::TerminalManager;
use crate::util::{audit_log_json, finalize_output, now_ms};

// ── Tool schema helpers ───────────────────────────────────────────────────────

fn required_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, Value> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| json!({"code": -32602, "message": format!("{tool}: missing required parameter '{key}'")}))
}

fn session_or_err(mgr: &TerminalManager, args: &Value, tool: &str)
    -> Result<std::sync::Arc<crate::terminal::TerminalSession>, Value>
{
    let sid = required_str(args, "session_id", tool)?;
    mgr.get(sid).ok_or_else(|| json!({"code": -32602, "message": "Session not found"}))
}

fn missing_reason(args: &Value, tool: &str) -> Option<Value> {
    match args.get("reason").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => None,
        _ => Some(json!({"code": -32602, "message": format!("{tool}: missing required parameter 'reason'")})),
    }
}

fn ok_text(text: impl Into<String>) -> Value {
    let t = text.into();
    json!({"content": [{"type": "text", "text": t}], "isError": false})
}

fn err_text(text: impl Into<String>) -> Value {
    let t = text.into();
    json!({"content": [{"type": "text", "text": t}], "isError": true})
}

// ── AppState ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub manager: TerminalManager,
    tools_json: Value,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            manager: TerminalManager::new(),
            tools_json: build_tools_list(),
        }
    }

    pub fn tools(&self) -> &Value { &self.tools_json }

    /// Handle a JSON-RPC 2.0 request. Returns (Option<response>, is_notification).
    pub fn handle_request(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // notifications (no id) — no response
        if id.is_none() { return None; }
        let id = id.unwrap();

        let result = match method {
            "initialize" => self.handle_initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools_json})),
            "tools/call" => self.handle_call_tool(&params),
            _ => Err(json!({"code": -32601, "message": format!("Method not found: {method}")})),
        };

        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }

    fn handle_initialize(&self, _params: &Value) -> Result<Value, Value> {
        Ok(json!({
            "protocolVersion": "2025-06-18",
            "serverInfo": { "name": "vibeterminalmcp-server", "version": "0.1.0" },
            "capabilities": { "tools": {} }
        }))
    }

    fn handle_call_tool(&self, params: &Value) -> Result<Value, Value> {
        let name = params.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| json!({"code": -32602, "message": "missing tool name"}))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        let result = match name {
            "terminal_new"      => self.tool_terminal_new(&args),
            "terminal_add_cmd"  => self.tool_terminal_add_cmd(&args),
            "terminal_line"     => self.tool_terminal_line(&args),
            "terminal_last"     => self.tool_terminal_last(&args),
            "terminal_wait"     => self.tool_terminal_wait(&args),
            "terminal_timeout"  => self.tool_terminal_timeout(&args),
            "terminal_kill"     => self.tool_terminal_kill(&args),
            "terminal_list"     => self.tool_terminal_list(&args),
            "terminal_resize"   => self.tool_terminal_resize(&args),
            "get_tools"         => self.tool_get_tools(&args),
            _ => Err(json!({"code": -32601, "message": format!("Unknown tool: {name}")})),
        };

        result.map_err(|e| json!({"code": -32602, "message": e.get("message").and_then(|v| v.as_str()).unwrap_or("tool error")}))
    }

    // ── tool implementations ──────────────────────────────────────────────────

    fn tool_terminal_new(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_new") { return Err(e); }
        let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
        let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
        let cwd = args.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());

        // env: object { key: val } or array [ "KEY=VAL" ]
        let mut env: Vec<(String, String)> = vec![];
        if let Some(eobj) = args.get("env").and_then(|v| v.as_object()) {
            for (k, v) in eobj {
                if let Some(vs) = v.as_str() { env.push((k.clone(), vs.to_string())); }
            }
        }

        match self.manager.create(rows, cols, cwd, env) {
            Ok(id) => {
                audit_log_json("terminal_new", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, &id);
                Ok(ok_text(&id))
            }
            Err(e) => {
                warn!("terminal_new failed: {e}");
                Ok(err_text(format!("Failed to create terminal: {e}")))
            }
        }
    }

    fn tool_terminal_add_cmd(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_add_cmd") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_add_cmd")?;
        let input = required_str(args, "input", "terminal_add_cmd")?;
        let auto_enter = args.get("auto_enter").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut data = input.to_string();
        if auto_enter { data.push('\n'); }
        session.write(&data);
        audit_log_json("terminal_add_cmd", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text("ok"))
    }

    fn tool_terminal_line(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_line") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_line")?;
        let start = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let end = args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(-1);
        let all = session.read_all();
        let lines: Vec<&str> = all.split('\n').collect();
        let total = lines.len();
        let lo = start.saturating_sub(1).min(total);
        let hi = if end < 0 { total } else { (end as usize).min(total) };
        let out = if lo < hi { lines[lo..hi].join("\n") } else { String::new() };
        let out = finalize_output(out);
        audit_log_json("terminal_line", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text(out))
    }

    fn tool_terminal_last(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_last") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_last")?;
        let out = finalize_output(session.read_last());
        audit_log_json("terminal_last", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text(out))
    }

    fn tool_terminal_wait(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_wait") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_wait")?;
        let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_i64())
            .unwrap_or(115000).min(115000);
        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("marker");

        let (ok, out, ec) = if mode == "prompt" {
            let (ok, out) = session.wait_prompt(timeout_ms);
            (ok, out, -1i32)
        } else {
            session.wait_marker(timeout_ms)
        };

        let out = finalize_output(out);
        audit_log_json("terminal_wait", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, if ok { "ready" } else { "timeout" });
        Ok(json!({
            "content": [{"type": "text", "text": out}],
            "isError": false,
            "structuredContent": { "ready": ok, "exitCode": ec, "mode": mode }
        }))
    }

    fn tool_terminal_timeout(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_timeout") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_timeout")?;
        let wait_ms = args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(5000).min(115000);
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        let out = finalize_output(session.read_last());
        audit_log_json("terminal_timeout", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text(out))
    }

    fn tool_terminal_kill(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_kill") { return Err(e); }
        let sid = required_str(args, "session_id", "terminal_kill")?;
        self.manager.kill(sid);
        audit_log_json("terminal_kill", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "closed");
        Ok(ok_text("closed"))
    }

    fn tool_terminal_list(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_list") { return Err(e); }
        let sessions = self.manager.list();
        let arr: Vec<Value> = sessions.iter().map(|s| {
            let all = s.read_all();
            let last_line = all.trim_end_matches('\n').rsplit('\n').next().unwrap_or("").to_string();
            let last_line = if last_line.len() > 200 { last_line[last_line.len()-200..].to_string() } else { last_line };
            json!({
                "id": s.id,
                "pid": s.pid,
                "alive": s.alive(),
                "idle_ms": now_ms() - s.last_activity_ms(),
                "last_line": last_line,
            })
        }).collect();
        let out = serde_json::to_string_pretty(&arr).unwrap_or_default();
        audit_log_json("terminal_list", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text(out))
    }

    fn tool_terminal_resize(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "terminal_resize") { return Err(e); }
        let session = session_or_err(&self.manager, args, "terminal_resize")?;
        let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
        let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
        session.resize(rows, cols);
        audit_log_json("terminal_resize", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text("ok"))
    }

    fn tool_get_tools(&self, args: &Value) -> Result<Value, Value> {
        if let Some(e) = missing_reason(args, "get_tools") { return Err(e); }
        let out = serde_json::to_string_pretty(&self.tools_json).unwrap_or_default();
        audit_log_json("get_tools", args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), args, "ok");
        Ok(ok_text(out))
    }
}

// ── Tool schema ───────────────────────────────────────────────────────────────

fn build_tools_list() -> Value {
    json!([
      {
        "name": "terminal_new",
        "description": "创建持久终端会话 (pty + bash 交互)。返回 session_id。可指定尺寸/工作目录/环境变量。",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":  { "type": "string", "description": "操作理由（必填，用于审计）" },
            "rows":    { "type": "integer", "description": "pty 行数 (default: 24)" },
            "cols":    { "type": "integer", "description": "pty 列数 (default: 80)" },
            "cwd":     { "type": "string",  "description": "工作目录 (默认继承)" },
            "env":     { "type": "object",  "description": "环境变量覆盖 (默认 TERM=xterm-256color)" }
          },
          "required": ["reason"]
        }
      },
      {
        "name": "terminal_add_cmd",
        "description": "向终端会话输入字符串 (支持控制字符, 不自动回车; auto_enter=true 时追加 \\n)",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string",  "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string",  "description": "终端会话 ID" },
            "input":      { "type": "string",  "description": "要输入的字符串" },
            "auto_enter": { "type": "boolean", "description": "自动追加回车 (default: false)" }
          },
          "required": ["session_id", "input", "reason"]
        }
      },
      {
        "name": "terminal_line",
        "description": "获取终端输出第 a-b 行内容 (1-based, 最多 4KB)",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string",  "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string",  "description": "终端会话 ID" },
            "start_line": { "type": "integer", "description": "起始行 (1-based) (default: 1)" },
            "end_line":   { "type": "integer", "description": "结束行 (-1=到最后) (default: -1)" }
          },
          "required": ["session_id", "reason"]
        }
      },
      {
        "name": "terminal_last",
        "description": "获取终端自上次查询以来的新输出 (最多 4KB)",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string", "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string", "description": "终端会话 ID" }
          },
          "required": ["session_id", "reason"]
        }
      },
      {
        "name": "terminal_wait",
        "description": "等待终端命令执行完 (marker 模式: 发 echo 标记等退出码; prompt 模式: 检测提示符)。最多 115s",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string",  "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string",  "description": "终端会话 ID" },
            "mode":       { "type": "string",  "description": "完成检测模式 (Allowed: marker, prompt) (default: marker)" },
            "timeout_ms": { "type": "integer", "description": "最大等待 (最多 115000) (default: 115000)" }
          },
          "required": ["session_id", "reason"]
        }
      },
      {
        "name": "terminal_timeout",
        "description": "等待终端 n 秒后返回当前输出 (不判断完成状态, 最多 115s)",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string",  "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string",  "description": "终端会话 ID" },
            "wait_ms":    { "type": "integer", "description": "等待毫秒 (最多 115000) (default: 5000)" }
          },
          "required": ["session_id", "reason"]
        }
      },
      {
        "name": "terminal_kill",
        "description": "关闭终端会话",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string", "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string", "description": "终端会话 ID" }
          },
          "required": ["session_id", "reason"]
        }
      },
      {
        "name": "terminal_list",
        "description": "列出终端会话 (ID/存活时间/最后一行)",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason": { "type": "string", "description": "操作理由（必填，用于审计）" }
          },
          "required": ["reason"]
        }
      },
      {
        "name": "terminal_resize",
        "description": "调整终端会话 pty 尺寸",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason":     { "type": "string",  "description": "操作理由（必填，用于审计）" },
            "session_id": { "type": "string",  "description": "终端会话 ID" },
            "rows":       { "type": "integer", "description": "行数" },
            "cols":       { "type": "integer", "description": "列数" }
          },
          "required": ["session_id", "rows", "cols", "reason"]
        }
      },
      {
        "name": "get_tools",
        "description": "获取服务器支持的所有工具列表（含完整 JSON Schema）",
        "inputSchema": {
          "type": "object",
          "properties": {
            "reason": { "type": "string", "description": "Brief explanation of why you are calling this tool" }
          },
          "required": ["reason"]
        }
      }
    ])
}
