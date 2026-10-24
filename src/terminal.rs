use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::io::RawFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use libc::{c_int, c_ushort, pid_t, termios, winsize};
use rand::Rng;
use tracing::{info, warn};

use crate::util::{now_ms, strip_ansi};

// -- Tunables ----------------------------------------------------------------
// Window of output kept per session. The old 64 KB window silently cut the
// head off ordinary command output (dmesg, build logs) and even left a
// truncated first line, so the window is now large enough for real work.
const BUF_MAX: usize = 4 * 1024 * 1024;
// Compact only after overflowing by this much, so a burst of output costs one
// memmove instead of one per read().
const BUF_SLACK: usize = 512 * 1024;
// Bytes per read(). The reader drains the tty until it is empty, otherwise the
// kernel-side pty buffer (usually 64 KB) stays full and blocks the writer.
// That back-pressure is the real "pty squeeze".
const READ_CHUNK: usize = 128 * 1024;
const POLL_MS: i32 = 50;
const MAX_WAIT_MS: i64 = 115000;
pub const DEFAULT_TAKE: usize = 4096;

// -- UTF-8 helpers -----------------------------------------------------------
fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() { return s.len(); }
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() { return s.len(); }
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}

fn tail_str(s: &str, n: usize) -> &str {
    let start = floor_boundary(s, s.len().saturating_sub(n));
    &s[start..]
}

// Split the longest valid UTF-8 prefix from buf.
// Returns (bytes consumed, decoded text). An incomplete trailing multi-byte
// sequence is left in buf for the next read so characters split across read()
// boundaries are not turned into U+FFFD.
fn split_valid_prefix(buf: &[u8]) -> (usize, String) {
    match std::str::from_utf8(buf) {
        Ok(s) => (buf.len(), s.to_string()),
        Err(e) => {
            let valid = e.valid_up_to();
            let text = String::from_utf8_lossy(&buf[..valid]).into_owned();
            match e.error_len() {
                Some(n) => (valid + n, text),
                None => (valid, text),
            }
        }
    }
}

// -- TerminalSession ---------------------------------------------------------

pub struct TerminalSession {
    pub id: String,
    master_fd: RawFd,
    pub pid: pid_t,
    state: Arc<SessionState>,
}

struct SessionState {
    alive: std::sync::atomic::AtomicBool,
    last_activity: std::sync::atomic::AtomicI64,
    buf: Mutex<BufState>,
    buf_cv: std::sync::Condvar,
}

struct BufState {
    data: String,
    // next byte the incremental reader has not returned yet
    last_read_pos: usize,
    // monotonic counters; compaction never resets them
    written: u64,
    dropped: u64,
    // last dropped value seen by the reader API, for the loss notice
    seen_dropped: u64,
}

impl TerminalSession {
    pub fn new(
        id: String,
        rows: u16,
        cols: u16,
        cwd: Option<String>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Arc<Self>, String> {
        // Explicit interactive termios: no ISTRIP, keep IUTF8 so UTF-8 works.
        let mut tio: termios = unsafe { std::mem::zeroed() };
        tio.c_iflag = (libc::ICRNL | libc::IXON | libc::IUTF8 | libc::BRKINT) as _;
        tio.c_oflag = (libc::OPOST | libc::ONLCR) as _;
        tio.c_cflag = (libc::B38400 | libc::CS8 | libc::CREAD) as _;
        tio.c_lflag = (libc::ISIG | libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::IEXTEN) as _;
        tio.c_cc[libc::VINTR] = 0x03;  // ^C
        tio.c_cc[libc::VQUIT] = 0x1c;  // ^backslash
        tio.c_cc[libc::VERASE] = 0x7f; // DEL
        tio.c_cc[libc::VKILL] = 0x15;  // ^U
        tio.c_cc[libc::VEOF] = 0x04;   // ^D
        tio.c_cc[libc::VSTART] = 0x11; // ^Q
        tio.c_cc[libc::VSTOP] = 0x13;  // ^S

        let ws = winsize {
            ws_row: rows as c_ushort,
            ws_col: cols as c_ushort,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master: c_int = -1;
        let pid = unsafe {
            libc::forkpty(&mut master, std::ptr::null_mut(), &tio, &ws)
        };

        if pid < 0 {
            return Err(format!("forkpty failed: errno={}", std::io::Error::last_os_error()));
        }

        if pid == 0 {
            // -- child --
            if let Some(ref d) = cwd {
                let _ = std::env::set_current_dir(d);
            }

            // Reset signal dispositions (exec keeps SIG_IGN, which breaks ^C).
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
            }

            let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
            std::env::set_var("TERM", term);
            for (k, v) in &extra_env {
                std::env::set_var(k, v);
            }

            let bash = std::ffi::CString::new("/bin/bash").unwrap();
            let argv = [
                std::ffi::CString::new("bash").unwrap(),
                std::ffi::CString::new("--noprofile").unwrap(),
                std::ffi::CString::new("--norc").unwrap(),
                std::ffi::CString::new("-i").unwrap(),
            ];
            let argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null())).collect();

            unsafe {
                libc::execvp(bash.as_ptr(), argv_ptrs.as_ptr());
                libc::_exit(127);
            }
        }

        // -- parent --
        let state = Arc::new(SessionState {
            alive: std::sync::atomic::AtomicBool::new(true),
            last_activity: std::sync::atomic::AtomicI64::new(now_ms()),
            buf: Mutex::new(BufState {
                data: String::new(),
                last_read_pos: 0,
                written: 0,
                dropped: 0,
                seen_dropped: 0,
            }),
            buf_cv: std::sync::Condvar::new(),
        });

        let session = Arc::new(TerminalSession {
            id: id.clone(),
            master_fd: master,
            pid,
            state: Arc::clone(&state),
        });

        let state2 = Arc::clone(&state);
        let master_fd = master;
        let id_cloned = id.clone();
        thread::Builder::new()
            .name(format!("pty-reader-{}", id))
            .spawn(move || reader_loop(master_fd, state2, id_cloned))
            .map_err(|e| format!("spawn reader thread: {e}"))?;

        info!("terminal {} created (pid={}, {}x{})", id, pid, rows, cols);
        Ok(session)
    }

    // Send a signal to the foreground process group (tpgid of the shell).
    pub fn signal_fg(&self, sig: c_int) -> bool {
        let stat_path = format!("/proc/{}/stat", self.pid);
        let Ok(file) = File::open(&stat_path) else {
            warn!("signal_fg: cannot open {stat_path}");
            return false;
        };
        let mut line = String::new();
        let mut reader = BufReader::new(file);
        if reader.read_line(&mut line).is_err() { return false; }

        let Some(rp) = line.rfind(')') else { return false; };
        let parts: Vec<&str> = line[rp + 1..].split_whitespace().collect();
        // fields: 0 state, 1 ppid, 2 pgrp, 3 session, 4 tty_nr, 5 tpgid
        if parts.len() < 6 { return false; }
        let Ok(tpgid) = parts[5].parse::<pid_t>() else { return false; };
        if tpgid <= 0 {
            warn!("signal_fg: no foreground process group (tpgid={tpgid})");
            return false;
        }

        info!("signal_fg: SIG{sig} to pgid {tpgid} (bash pid {})", self.pid);
        unsafe {
            if libc::kill(-tpgid, sig) != 0 {
                warn!("signal_fg: kill(-{tpgid}, {sig}) failed");
                return false;
            }
        }
        true
    }

    // Write input to the pty (control characters are mapped to signals).
    pub fn write(&self, data: &str) {
        if !self.alive() { return; }
        let mut rest = Vec::new();
        let mut has_ctrl = false;

        for b in data.bytes() {
            match b {
                0x03 => { self.signal_fg(libc::SIGINT);  has_ctrl = true; } // ^C
                0x1a => { self.signal_fg(libc::SIGTSTP); has_ctrl = true; } // ^Z
                0x1c => { self.signal_fg(libc::SIGQUIT); has_ctrl = true; } // ^backslash
                _ => rest.push(b),
            }
        }

        if !rest.is_empty() {
            let mut off = 0;
            while off < rest.len() && self.alive() {
                let n = unsafe {
                    libc::write(self.master_fd, rest[off..].as_ptr() as _, rest.len() - off)
                };
                if n > 0 { off += n as usize; continue; }
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EINTR { continue; }
                if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                break; // hard error
            }
        }

        if has_ctrl {
            thread::sleep(Duration::from_millis(300));
        }
        self.state.last_activity.store(now_ms(), Ordering::Relaxed);
    }

    pub fn read_all(&self) -> String {
        self.state.buf.lock().unwrap().data.clone()
    }

    // Incremental read: return up to max_bytes of not-yet-returned output, in
    // order. Returns (text, bytes_still_unread, bytes_dropped_since_last_read).
    // The cursor only advances over what was actually returned, so a large
    // burst can be paged with repeated calls instead of being thrown away.
    pub fn read_last(&self, max_bytes: usize) -> (String, usize, u64) {
        let limit = if max_bytes == 0 { DEFAULT_TAKE } else { max_bytes };
        let mut buf = self.state.buf.lock().unwrap();
        let start = floor_boundary(&buf.data, buf.last_read_pos);
        buf.last_read_pos = start;

        let dropped = buf.dropped.saturating_sub(buf.seen_dropped);
        buf.seen_dropped = buf.dropped;

        let remaining = buf.data.len().saturating_sub(start);
        if remaining == 0 {
            return (String::new(), 0, dropped);
        }

        let mut end = floor_boundary(&buf.data, start + limit);
        if end <= start { end = ceil_boundary(&buf.data, start + 1); }
        // Prefer to stop on a line boundary so callers never get half a line.
        if end < buf.data.len() {
            if let Some(p) = buf.data[start..end].rfind('\n') {
                if p > 0 { end = start + p + 1; }
            }
        }

        let out = buf.data[start..end].to_string();
        buf.last_read_pos = end;
        let left = buf.data.len().saturating_sub(end);
        (out, left, dropped)
    }

    // Walk the lines lazily and stop as soon as `limit` bytes are gathered,
    // so asking for a huge range never materialises the whole buffer.
    // Returns (text, more). `more` is true when the byte limit or `b` stopped
    // the walk before the requested range ended, so the caller can say so
    // instead of silently looking like the end of the buffer.
    pub fn read_lines(&self, a: usize, b: usize, limit: usize) -> (String, bool) {
        let buf = self.state.buf.lock().unwrap();
        if a < 1 { return ("(no output)".into(), false); }
        let limit = limit.max(256);
        let mut out = String::new();
        let mut found = false;
        let mut idx = 0usize;
        let mut more = false;
        for line in buf.data.split('\n') {
            idx += 1;
            if idx < a { continue; }
            if b != usize::MAX && idx > b { break; }
            if found { out.push('\n'); }
            out.push_str(line);
            found = true;
            if out.len() >= limit { more = true; break; }
        }
        if !found { ("(no output)".into(), false) } else { (out, more) }
    }

    // marker mode: send "echo __DONE_x__:$?" and wait for the numeric exit code
    pub fn wait_marker(&self, mut timeout_ms: i64) -> (bool, String, i32) {
        if timeout_ms <= 0 || timeout_ms > MAX_WAIT_MS { timeout_ms = MAX_WAIT_MS; }
        const MAX_ATTEMPTS: usize = 3;
        let mut per_attempt = timeout_ms / MAX_ATTEMPTS as i64;
        if per_attempt < 1000 { per_attempt = timeout_ms; }

        let mut found = false;
        let mut ec: i32 = -1;

        for _attempt in 0..MAX_ATTEMPTS {
            if found { break; }
            let tag: String = {
                let mut rng = rand::thread_rng();
                (0..8).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
            };
            let marker = format!("__DONE_{tag}__");
            let marker_exit = format!("{marker}:");

            // Ctrl+U clears any pending input before the marker command.
            self.write(&format!("\x15echo {marker}:$?\n"));

            // written is monotonic, so a saturated ring buffer no longer looks
            // "idle" the way a plain length comparison did.
            let mut last_written = self.state.buf.lock().unwrap().written;
            let deadline = now_ms() + per_attempt;

            while now_ms() < deadline && self.alive() {
                let hit = {
                    let guard = self.state.buf.lock().unwrap();
                    let (guard, _) = self.state.buf_cv
                        .wait_timeout_while(guard, Duration::from_millis(200), |b| {
                            b.written == last_written && self.alive()
                        })
                        .unwrap();
                    last_written = guard.written;

                    // The marker is near the tail. Scanning only the newest
                    // 1 MB avoids re-walking a 4 MB buffer every 200 ms.
                    let data = &guard.data;
                    let start = floor_boundary(data, data.len().saturating_sub(1024 * 1024));
                    let mut hit: Option<i32> = None;
                    let mut search = start;
                    while let Some(pos) = data[search..].find(&marker_exit) {
                        let abs_pos = search + pos;
                        let after = &data[abs_pos + marker_exit.len()..];
                        let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if let Ok(val) = num_str.parse::<i32>() {
                            hit = Some(val);
                            break;
                        }
                        search = abs_pos + 1;
                    }
                    hit
                };
                if let Some(v) = hit { ec = v; found = true; break; }
            }
        }

        // Let the reader catch up. The marker only proves the shell ran the
        // command; bytes produced just before it may still sit in the pty
        // buffer, which used to make the first read after wait look short.
        if found {
            let t0 = now_ms();
            loop {
                let w0 = self.state.buf.lock().unwrap().written;
                thread::sleep(Duration::from_millis(30));
                let w1 = self.state.buf.lock().unwrap().written;
                if w1 == w0 || now_ms() - t0 > 500 { break; }
            }
        }

        let out = {
            let buf = self.state.buf.lock().unwrap();
            tail_str(&buf.data, DEFAULT_TAKE).to_string()
        };
        (found, out, ec)
    }

    // prompt mode: wait until the last output line ends with a shell prompt
    pub fn wait_prompt(&self, mut timeout_ms: i64) -> (bool, String) {
        if timeout_ms <= 0 || timeout_ms > MAX_WAIT_MS { timeout_ms = MAX_WAIT_MS; }
        let mut last_written = self.state.buf.lock().unwrap().written;
        let deadline = now_ms() + timeout_ms;
        let mut found = false;

        while now_ms() < deadline && self.alive() {
            let last_line = {
                let guard = self.state.buf.lock().unwrap();
                let (guard, _) = self.state.buf_cv
                    .wait_timeout_while(guard, Duration::from_millis(200), |b| {
                        b.written == last_written && self.alive()
                    })
                    .unwrap();
                last_written = guard.written;
                let tail = tail_str(&guard.data, 1024);
                strip_ansi(tail)
                    .trim_end_matches('\n')
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .trim_end()
                    .to_string()
            };
            if let Some(c) = last_line.chars().last() {
                if matches!(c, '$' | '#' | '>' | ':' | '%') {
                    found = true;
                    break;
                }
            }
        }

        let out = {
            let buf = self.state.buf.lock().unwrap();
            tail_str(&buf.data, DEFAULT_TAKE).to_string()
        };
        (found, out)
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = winsize {
            ws_row: rows as c_ushort,
            ws_col: cols as c_ushort,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws);
        }
    }

    pub fn alive(&self) -> bool {
        self.state.alive.load(Ordering::Relaxed)
    }

    pub fn last_activity_ms(&self) -> i64 {
        self.state.last_activity.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if !self.state.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        self.state.buf_cv.notify_all();
        unsafe {
            let _ = libc::write(self.master_fd, b"exit\n".as_ptr() as _, 5);
        }
        thread::sleep(Duration::from_millis(100));
        unsafe {
            libc::close(self.master_fd);
            libc::kill(self.pid, libc::SIGTERM);
            thread::sleep(Duration::from_millis(50));
            libc::waitpid(self.pid, std::ptr::null_mut(), libc::WNOHANG);
        }
        info!("terminal {} closed", self.id);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.close();
    }
}

// -- reader thread -----------------------------------------------------------
// Drains the pty until it is empty on every wakeup, then appends to the ring
// window. Partial UTF-8 sequences are carried across read() boundaries.
fn reader_loop(master_fd: RawFd, state: Arc<SessionState>, id: String) {
    let mut raw = vec![0u8; READ_CHUNK];
    let mut pending: Vec<u8> = Vec::new();

    'outer: while state.alive.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let pr = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
        if pr < 0 {
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if err == libc::EINTR { continue; }
            break;
        }
        if pr == 0 { continue; }
        if (pfd.revents & (libc::POLLIN | libc::POLLHUP)) == 0 {
            if (pfd.revents & (libc::POLLERR | libc::POLLNVAL)) != 0 { break; }
            continue;
        }

        let mut eof = false;
        loop {
            let n = unsafe { libc::read(master_fd, raw.as_mut_ptr() as _, raw.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EINTR { continue; }
                if err == libc::EAGAIN || err == libc::EWOULDBLOCK { break; }
                eof = true;
                break;
            }
            if n == 0 { eof = true; break; }

            pending.extend_from_slice(&raw[..n as usize]);
            let (consumed, text) = split_valid_prefix(&pending);
            if consumed > 0 { pending.drain(..consumed); }
            if !text.is_empty() {
                append_output(&state, &text);
            }

            // A long run of invalid bytes (binary output) would otherwise pile
            // up forever; flush it lossily once it grows past 8 KB.
            if pending.len() > 8192 {
                let text = String::from_utf8_lossy(&pending).into_owned();
                pending.clear();
                if !text.is_empty() { append_output(&state, &text); }
            }

            // Keep reading while data is already queued. This is what stops
            // the kernel pty buffer from filling up and blocking the writer.
            let mut p2 = libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let more = unsafe { libc::poll(&mut p2, 1, 0) };
            if more <= 0 || (p2.revents & libc::POLLIN) == 0 { break; }
        }

        state.last_activity.store(now_ms(), Ordering::Relaxed);
        if eof { break 'outer; }
    }

    state.alive.store(false, Ordering::SeqCst);
    state.buf_cv.notify_all();
    info!("terminal reader {id} finished");
}

fn append_output(state: &SessionState, text: &str) {
    let mut buf = state.buf.lock().unwrap();
    buf.data.push_str(text);
    buf.written = buf.written.saturating_add(text.len() as u64);

    if buf.data.len() > BUF_MAX + BUF_SLACK {
        // Keep the newest BUF_MAX bytes; cut on a character boundary and move
        // the read cursor back by exactly what was removed.
        let mut drop_to = buf.data.len() - BUF_MAX;
        drop_to = ceil_boundary(&buf.data, drop_to);
        if drop_to > 0 {
            buf.data.drain(..drop_to);
            buf.dropped = buf.dropped.saturating_add(drop_to as u64);
            buf.last_read_pos = floor_boundary(&buf.data, buf.last_read_pos.saturating_sub(drop_to));
        }
    }
    drop(buf);
    state.buf_cv.notify_all();
}

// -- TerminalManager ---------------------------------------------------------

#[derive(Clone, Default)]
pub struct TerminalManager {
    sessions: Arc<Mutex<HashMap<String, Arc<TerminalSession>>>>,
}

impl TerminalManager {
    pub fn new() -> Self { Self::default() }

    pub fn create(
        &self,
        rows: u16,
        cols: u16,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, String> {
        let id: String = {
            let mut rng = rand::thread_rng();
            (0..16).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
        };
        let session = TerminalSession::new(id.clone(), rows, cols, cwd, env)?;
        self.sessions.lock().unwrap().insert(id.clone(), session);
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn kill(&self, id: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(id) {
            s.close();
        }
    }

    pub fn list(&self) -> Vec<Arc<TerminalSession>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    // Drop sessions whose reader thread has exited (shell closed), so their
    // output buffers do not linger for the lifetime of the server.
    pub fn cleanup_dead(&self) {
        let mut map = self.sessions.lock().unwrap();
        let dead: Vec<String> = map.iter()
            .filter(|(_, s)| !s.alive())
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            if let Some(s) = map.remove(&id) { s.close(); }
        }
    }

    pub fn cleanup_idle(&self, idle_ms: i64) {
        let now = now_ms();
        let mut map = self.sessions.lock().unwrap();
        let stale: Vec<String> = map.iter()
            .filter(|(_, s)| now - s.last_activity_ms() > idle_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(s) = map.remove(&id) { s.close(); }
        }
    }
}
