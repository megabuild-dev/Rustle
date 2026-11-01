use rustle_server::config::{Config, VERSION};
use rustle_server::{cli, server};
use std::process::ExitCode;

const HELP: &str = "rustle - ultra-light self-hosted PaaS

USAGE:
  rustle [serve]                 run the server (default)
  rustle init [--reset-password] create master key, admin account and baseline settings
  rustle restore <file> [--force] restore an encrypted backup into the configured database
  rustle key export              print the master key (store it somewhere safe)
  rustle caddy-config            print the initial Caddy JSON configuration
  rustle version

Configuration is read from environment variables (see docs/DEV.md).";

fn runtime() -> tokio::runtime::Runtime {
    // One thread for the event loop; at most two blocking threads with small stacks. The admin
    // process has a hard 48 MB cap, so thread count is memory.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .thread_stack_size(512 * 1024)
        .build()
        .expect("tokio runtime")
}

fn init_logging() {
    let level = match std::env::var("RUST_LOG").unwrap_or_default().to_ascii_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt().with_max_level(level).with_target(false).without_time().init();
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Config::from_env();
    match args.first().map(String::as_str) {
        None | Some("serve") => {
            init_logging();
            tracing::info!("rustle {VERSION} starting");
            match runtime().block_on(server::run(cfg)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("init") => match cli::init(&cfg, args.iter().any(|a| a == "--reset-password")) {
            Ok(r) => {
                println!("Rustle initialized.");
                if r.key_created {
                    println!("\nMASTER KEY (save it NOW; without it backups cannot be restored):\n  {}\n", r.key_hex);
                }
                match r.password {
                    Some(p) => println!("Admin login:\n  username: admin\n  password: {p}\n(This password is shown only once. Change it after signing in.)"),
                    None => println!("Admin account already exists; password unchanged."),
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Some("restore") => {
            let Some(file) = args.get(1) else {
                eprintln!("usage: rustle restore <file> [--force]");
                return ExitCode::from(2);
            };
            match cli::restore(&cfg, std::path::Path::new(file), args.iter().any(|a| a == "--force")) {
                Ok(()) => {
                    println!("restored into {}", cfg.db_path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("key") if args.get(1).map(String::as_str) == Some("export") => match cli::key_export(&cfg) {
            Ok(k) => {
                println!("{k}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Some("caddy-config") => match cli::caddy_config(&cfg) {
            Ok(j) => {
                println!("{j}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Some("version") | Some("--version") | Some("-V") => {
            println!("rustle {VERSION}");
            ExitCode::SUCCESS
        }
        Some("help") | Some("--help") | Some("-h") => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}
