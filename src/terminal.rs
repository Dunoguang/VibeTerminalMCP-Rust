use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::io::{RawFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration};

use libc::{c_int, c_ushort, pid_t, winsize, termios};
use rand::Rng;
use tracing::{info, warn};

use crate::util::{now_ms, strip_ansi};

const BUF_MAX: usize = 64 * 1024; // 64 KB 环形缓冲
const POLL_MS: i32 = 100;
const MAX_WAIT_MS: i64 = 115000;

// ── TerminalSession ──────────────────────────────────────────────────────────

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
    last_read_pos: usize,
}

impl TerminalSession {
    pub fn new(
        id: String,
        rows: u16,
        cols: u16,
        cwd: Option<String>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Arc<Self>, String> {
        // 构建 termios (显式交互式配置，移除 ISTRIP 保证 UTF-8 正常)
        let mut tio: termios = unsafe { std::mem::zeroed() };
        tio.c_iflag = (libc::ICRNL | libc::IXON | libc::IUTF8 | libc::BRKINT) as _;
        tio.c_oflag = (libc::OPOST | libc::ONLCR) as _;
        tio.c_cflag = (libc::B38400 | libc::CS8 | libc::CREAD) as _;
        tio.c_lflag = (libc::ISIG | libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::IEXTEN) as _;
        tio.c_cc[libc::VINTR] = 0x03;  // ^C
        tio.c_cc[libc::VQUIT] = 0x1c;  // ^\
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
            // ── 子进程 ──
            if let Some(ref d) = cwd {
                let _ = std::env::set_current_dir(d);
            }

            // 重置信号处置 (exec 会保留 SIG_IGN，恢复默认处置让 Ctrl+C 有效)
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
            }

            // 环境变量
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

        // ── 父进程 ──
        let state = Arc::new(SessionState {
            alive: std::sync::atomic::AtomicBool::new(true),
            last_activity: std::sync::atomic::AtomicI64::new(now_ms()),
            buf: Mutex::new(BufState { data: String::new(), last_read_pos: 0 }),
            buf_cv: std::sync::Condvar::new(),
        });

        let session = Arc::new(TerminalSession {
            id: id.clone(),
            master_fd: master,
            pid,
            state: Arc::clone(&state),
        });

        // 读线程
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

    /// 向前台进程组发信号 (从 /proc/<bash_pid>/stat 读 tpgid)
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
        // 字段索引: 0:state, 1:ppid, 2:pgrp, 3:session, 4:tty_nr, 5:tpgid
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

    /// 写输入到 pty (支持控制字符映射)
    pub fn write(&self, data: &str) {
        if !self.alive() { return; }
        let mut rest = Vec::new();
        let mut has_ctrl = false;

        for b in data.bytes() {
            match b {
                0x03 => { self.signal_fg(libc::SIGINT);  has_ctrl = true; } // ^C
                0x1a => { self.signal_fg(libc::SIGTSTP); has_ctrl = true; } // ^Z
                0x1c => { self.signal_fg(libc::SIGQUIT); has_ctrl = true; } // ^\
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
        self.state.last_activity.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn read_all(&self) -> String {
        self.state.buf.lock().unwrap().data.clone()
    }

    pub fn read_last(&self) -> String {
        let mut buf = self.state.buf.lock().unwrap();
        let pos = buf.last_read_pos;
        let out = buf.data[pos..].to_string();
        buf.last_read_pos = buf.data.len();
        out
    }

    pub fn read_lines(&self, a: usize, b: usize) -> String {
        let buf = self.state.buf.lock().unwrap();
        if a < 1 { return "(no output)".into(); }
        let lines: Vec<&str> = buf.data.split('\n').collect();
        let total = lines.len();
        let lo = a.saturating_sub(1).min(total);
        let hi = b.min(total);
        if lo >= hi { return "(no output)".into(); }
        lines[lo..hi].join("\n")
    }

    /// marker 模式等待 (10-02 重构全缓冲搜索版)
    pub fn wait_marker(&self, mut timeout_ms: i64) -> (bool, String, i32) {
        if timeout_ms <= 0 || timeout_ms > MAX_WAIT_MS { timeout_ms = MAX_WAIT_MS; }
        const MAX_ATTEMPTS: usize = 3;
        let mut per_attempt = timeout_ms / MAX_ATTEMPTS as i64;
        if per_attempt < 1000 { per_attempt = timeout_ms; }

        let mut found = false;
        let mut ec = -1;

        for _attempt in 0..MAX_ATTEMPTS {
            if found { break; }
            let tag: String = {
                let mut rng = rand::thread_rng();
                (0..8).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
            };
            let marker = format!("__DONE_{tag}__");
            let marker_exit = format!("{marker}:");

            // 发送 marker 命令 (先发 \x15 = Ctrl+U 清除前台输入行)
            self.write(&format!("\x15echo {marker}:$?\n"));

            let mut last_size = self.state.buf.lock().unwrap().data.len();
            let deadline = now_ms() + per_attempt;

            while now_ms() < deadline && self.alive() {
                let buf_data = {
                    let guard = self.state.buf.lock().unwrap();
                    let (guard, _) = self.state.buf_cv
                        .wait_timeout_while(guard, Duration::from_millis(200), |b| {
                            b.data.len() == last_size && self.alive()
                        })
                        .unwrap();
                    last_size = guard.data.len();
                    guard.data.clone()
                };

                // 全缓冲循环搜索：跳过命令回显中的 marker:$?，找到真实的数字退出码
                let mut search = 0;
                while let Some(pos) = buf_data[search..].find(&marker_exit) {
                    let abs_pos = search + pos;
                    let after = &buf_data[abs_pos + marker_exit.len()..];
                    // 解析数字
                    let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(val) = num_str.parse::<i32>() {
                        ec = val;
                        found = true;
                        break;
                    }
                    search = abs_pos + 1;
                }
                if found { break; }
            }
        }

        // 取尾部最多 4096 字节
        let out = {
            let buf = self.state.buf.lock().unwrap();
            let len = buf.data.len();
            if len <= 4096 { buf.data.clone() } else { buf.data[len - 4096..].to_string() }
        };

        (found, out, ec)
    }

    /// prompt 模式等待
    pub fn wait_prompt(&self, mut timeout_ms: i64) -> (bool, String) {
        if timeout_ms <= 0 || timeout_ms > MAX_WAIT_MS { timeout_ms = MAX_WAIT_MS; }
        let init_len = self.state.buf.lock().unwrap().data.len();
        let deadline = now_ms() + timeout_ms;
        let mut found = false;

        while now_ms() < deadline && self.alive() {
            let buf_data = {
                let guard = self.state.buf.lock().unwrap();
                let (guard, _) = self.state.buf_cv
                    .wait_timeout_while(guard, Duration::from_millis(200), |b| {
                        b.data.len() == init_len && self.alive()
                    })
                    .unwrap();
                guard.data.clone()
            };

            // 取最后 1024 字节找提示符 (剥离 ANSI 避免干扰)
            let tail_start = buf_data.len().saturating_sub(1024);
            let tail = strip_ansi(&buf_data[tail_start..]);
            let last_line = tail.trim_end_matches('\n').rsplit('\n').next().unwrap_or("").trim_end();
            if let Some(c) = last_line.chars().last() {
                if matches!(c, '$' | '#' | '>' | ':' | '%') {
                    found = true;
                    break;
                }
            }
        }

        let out = {
            let buf = self.state.buf.lock().unwrap();
            let len = buf.data.len();
            if len <= 4096 { buf.data.clone() } else { buf.data[len - 4096..].to_string() }
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
        self.state.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn last_activity_ms(&self) -> i64 {
        self.state.last_activity.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn close(&self) {
        if !self.state.alive.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.state.buf_cv.notify_all();
        // 写 exit + 等待 100ms
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

fn reader_loop(master_fd: RawFd, state: Arc<SessionState>, id: String) {
    let mut raw = [0u8; 8192];
    while state.alive.load(std::sync::atomic::Ordering::Relaxed) {
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
        if pr > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP)) != 0 {
            let n = unsafe {
                libc::read(master_fd, raw.as_mut_ptr() as _, raw.len())
            };
            if n <= 0 { break; }
            let chunk = String::from_utf8_lossy(&raw[..n as usize]).into_owned();
            state.last_activity.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
            let mut buf = state.buf.lock().unwrap();
            buf.data.push_str(&chunk);
            // 环形缓冲：超 64KB 丢弃头部
            if buf.data.len() > BUF_MAX {
                let drop_bytes = buf.data.len() - BUF_MAX;
                buf.last_read_pos = buf.last_read_pos.saturating_sub(drop_bytes);
                let mut start = drop_bytes;
                while start < buf.data.len() && !buf.data.is_char_boundary(start) { start += 1; }
                buf.data = buf.data[start..].to_string();
            }
            drop(buf);
            state.buf_cv.notify_all();
        }
    }
    state.alive.store(false, std::sync::atomic::Ordering::SeqCst);
    state.buf_cv.notify_all();
    info!("terminal reader {id} finished");
}

// ── TerminalManager ──────────────────────────────────────────────────────────

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
