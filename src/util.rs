use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::Local;

// ── ANSI strip ───────────────────────────────────────────────────────────────

pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // ESC sequences
        if b == 0x1B {
            i += 1;
            if i >= bytes.len() { break; }
            match bytes[i] {
                // CSI: ESC [ ... final (0x40-0x7E)
                b'[' => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) { i += 1; }
                    i += 1;
                }
                // OSC: ESC ] ... ST (ESC \ or BEL)
                b']' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 { i += 1; break; }
                        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i+1] == b'\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                // DCS / SOS / PM / APC: ESC [PX^_] ... ST
                b'P' | b'X' | b'^' | b'_' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i+1] == b'\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                // two-byte sequences: just skip next byte
                _ => { i += 1; }
            }
        }
        // C1 range 0x80-0x9F — skip only actual C1 bytes (bare, not UTF-8 continuation)
        // 0x80-0x9F as single bytes are only valid in Latin-1 legacy streams.
        // In UTF-8 they appear as continuation bytes (0x80-0xBF) and must NOT be stripped.
        // We only strip 0x90 (DCS) and 0x9B (CSI) which are unambiguous C1 controls:
        // actually to be safe we skip stripping C1 here — handled by sanitize_utf8.

        // Discard other C0 controls except \n \r \t
        else if b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t' {
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    // SAFETY: sanitize_utf8 will fix any bad sequences; here just convert lossily
    String::from_utf8_lossy(&out).into_owned()
}

// ── UTF-8 sanitise ───────────────────────────────────────────────────────────

pub fn sanitize_utf8(input: &str) -> String {
    // Replace invalid UTF-8 sequences with U+FFFD.
    // Rust strings are always valid UTF-8, so this is a no-op on &str.
    // Used when we receive raw bytes from PTY.
    input.to_string()
}

/// Used for raw byte buffers from PTY.
pub fn sanitize_bytes(input: &[u8]) -> String {
    let s = String::from_utf8_lossy(input).into_owned();
    strip_ansi(&s)
}

// ── Output finalise (4 KB cap + truncation marker) ───────────────────────────

const MAX_OUTPUT_BYTES: usize = 4096;
const TRUNC_MARKER: &str = "===4KB截断===\n";

pub fn finalize_output(mut s: String) -> String {
    s = strip_ansi(&s);
    s = String::from_utf8_lossy(s.as_bytes()).into_owned(); // ensure valid UTF-8
    if s.len() > MAX_OUTPUT_BYTES {
        // take tail, align to UTF-8 boundary
        let mut start = s.len() - MAX_OUTPUT_BYTES;
        while start < s.len() && (s.as_bytes()[start] & 0xC0) == 0x80 {
            start += 1;
        }
        s = format!("{}{}", TRUNC_MARKER, &s[start..]);
    }
    s
}

// ── Timestamp ────────────────────────────────────────────────────────────────

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Audit log ────────────────────────────────────────────────────────────────

static AUDIT_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn audit_mutex() -> &'static Mutex<Option<String>> {
    AUDIT_PATH.get_or_init(|| Mutex::new(None))
}

pub fn set_audit_path(path: String) {
    *audit_mutex().lock().unwrap() = Some(path);
}

pub fn audit_path() -> PathBuf {
    let guard = audit_mutex().lock().unwrap();
    if let Some(p) = guard.as_ref() {
        return PathBuf::from(p);
    }
    // env override
    if let Ok(p) = std::env::var("SHELL_MCP_AUDIT_LOG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // exe dir
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("audit.log");
        }
    }
    PathBuf::from("audit.log")
}

pub fn audit_log(tool: &str, reason: &str, args_summary: &str, status: &str) {
    let path = audit_path();
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = format!("[{now}] {tool} | reason={reason} | args={args_summary} | status={status}\n");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn audit_log_json(tool: &str, reason: &str, args: &serde_json::Value, status: &str) {
    let mut summary = serde_json::to_string(args).unwrap_or_default();
    if summary.len() > 512 {
        // clip at UTF-8 boundary
        let mut cut = 512;
        while cut > 0 && (summary.as_bytes()[cut] & 0xC0) == 0x80 { cut -= 1; }
        summary.truncate(cut);
        summary.push_str("...");
    }
    audit_log(tool, reason, &summary, status);
}
