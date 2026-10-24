mod http_transport;
mod server;
mod terminal;
mod util;

use server::AppState;
use util::{audit_path, set_audit_path};

fn main() {
    // init tracing (honors RUST_LOG or SHELL_MCP_LOG, defaults to info)
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| {
            std::env::var("SHELL_MCP_LOG")
                .map(|lvl| tracing_subscriber::EnvFilter::new(lvl))
        })
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8001;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => { i += 1; if i < args.len() { host = args[i].clone(); } }
            "--port" | "-p" => { i += 1; if i < args.len() { port = args[i].parse().unwrap_or(8001); } }
            "--mode" | "-m" => { i += 1; } // 兼容旧参数 (--mode http)
            "--audit-log" => { i += 1; if i < args.len() { set_audit_path(args[i].clone()); } }
            "--help" | "-h" => {
                eprintln!("Usage: vibeterminalmcp-server [--host HOST] [--port PORT] [--audit-log PATH]");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let state = AppState::new();

    tracing::info!("audit log: {}", audit_path().display());
    // Reap sessions whose shell has exited. Without this the map only grows.
    {
        let st = state.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            st.cleanup_dead();
        });
    }


    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(http_transport::run(state, &host, port));
}
