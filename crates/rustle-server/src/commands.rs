//! Command-line dispatch, separated from `main` so every command is testable.
//! Each command returns the process exit code and writes to the given streams.

use crate::cli;
use crate::config::{Config, VERSION};
use std::io::Write;
use std::path::Path;

pub const HELP: &str = "rustle - ultra-light self-hosted PaaS

USAGE:
  rustle [serve]                 run the server (default)
  rustle init [--reset-password] create master key, admin account and baseline settings
  rustle restore <file> [--force] restore an encrypted backup into the configured database
  rustle key export              print the master key (store it somewhere safe)
  rustle caddy-config            print the initial Caddy JSON configuration
  rustle import-site-ctl <dir> [--dry-run]
                                 import the sites of a site-ctl.sh directory (~/apps): env, cron, domain, RAM
  rustle version

Configuration is read from environment variables (see docs/DEV.md).";

/// What `main` should do for these arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    Init { reset_password: bool },
    Restore { file: String, force: bool },
    KeyExport,
    ImportSiteCtl { dir: String, dry: bool },
    CaddyConfig,
    Version,
    Help,
    Usage(&'static str),
    Unknown(String),
}

pub fn parse(args: &[String]) -> Command {
    let flag = |f: &str| args.iter().any(|a| a == f);
    match args.first().map(String::as_str) {
        None | Some("serve") => Command::Serve,
        Some("init") => Command::Init {
            reset_password: flag("--reset-password"),
        },
        Some("restore") => match args.get(1) {
            Some(file) => Command::Restore {
                file: file.clone(),
                force: flag("--force"),
            },
            None => Command::Usage("usage: rustle restore <file> [--force]"),
        },
        Some("key") if args.get(1).map(String::as_str) == Some("export") => Command::KeyExport,
        Some("import-site-ctl") => match args.get(1).filter(|a| !a.starts_with("--")) {
            Some(dir) => Command::ImportSiteCtl {
                dir: dir.clone(),
                dry: flag("--dry-run"),
            },
            None => Command::Usage("usage: rustle import-site-ctl <dir> [--dry-run]"),
        },
        Some("caddy-config") => Command::CaddyConfig,
        Some("version") | Some("--version") | Some("-V") => Command::Version,
        Some("help") | Some("--help") | Some("-h") => Command::Help,
        Some(other) => Command::Unknown(other.to_string()),
    }
}

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
    let level = crate::server::log_level(&std::env::var("RUST_LOG").unwrap_or_default());
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .without_time()
        .init();
}

/// One malloc arena: the default (8 x cores) multiplies resident memory for no benefit here.
fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt is called once at startup before any other thread exists.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

/// The whole program: `main` only forwards here. Returns the process exit code.
pub fn entry(args: &[String]) -> u8 {
    tune_allocator();
    let cfg = Config::from_env();
    match parse(args) {
        Command::Serve => serve(cfg),
        cmd => execute(cmd, &cfg, &mut std::io::stdout(), &mut std::io::stderr()),
    }
}

fn serve(cfg: Config) -> u8 {
    init_logging();
    tracing::info!("rustle {VERSION} starting");
    match runtime().block_on(crate::server::run(cfg)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Run a non-serve command. Returns the exit code.
pub fn execute(cmd: Command, cfg: &Config, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match cmd {
        Command::Serve => 0, // handled by main
        Command::Init { reset_password } => {
            report(cli::init(cfg, reset_password), out, err, init_text)
        }
        Command::Restore { file, force } => {
            report(cli::restore(cfg, Path::new(&file), force), out, err, |()| {
                format!("restored into {}", cfg.db_path.display())
            })
        }
        Command::KeyExport => report(cli::key_export(cfg), out, err, |k| k),
        Command::ImportSiteCtl { dir, dry } => report(
            cli::import_site_ctl(cfg, Path::new(&dir), dry),
            out,
            err,
            |r| import_text(&r, dry),
        ),
        Command::CaddyConfig => report(cli::caddy_config(cfg), out, err, |j| j),
        Command::Version => report(
            Ok::<_, String>(format!("rustle {VERSION}")),
            out,
            err,
            |v| v,
        ),
        Command::Help => report(Ok::<_, String>(HELP.to_string()), out, err, |v| v),
        Command::Usage(u) => {
            let _ = writeln!(err, "{u}");
            2
        }
        Command::Unknown(c) => {
            let _ = writeln!(err, "unknown command '{c}'\n\n{HELP}");
            2
        }
    }
}

fn report<T>(
    r: Result<T, String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    text: impl FnOnce(T) -> String,
) -> u8 {
    match r {
        Ok(v) => {
            let _ = writeln!(out, "{}", text(v));
            0
        }
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            1
        }
    }
}

fn init_text(r: cli::InitResult) -> String {
    let mut s = String::from("Rustle initialized.");
    if r.key_created {
        s += &format!(
            "\n\nMASTER KEY (save it NOW; without it backups cannot be restored):\n  {}\n",
            r.key_hex
        );
    }
    match r.password {
        Some(p) => s += &format!("Admin login:\n  username: admin\n  password: {p}\n(This password is shown only once. Change it after signing in.)"),
        None => s += "Admin account already exists; password unchanged.",
    }
    s
}

fn import_text(r: &cli::ImportReport, dry: bool) -> String {
    format!(
        "{}\n\n{} {}, {} skipped.{}",
        r.lines.join("\n"),
        r.imported,
        if dry { "would be imported" } else { "imported" },
        r.skipped,
        if dry {
            ""
        } else {
            " Open the dashboard to add a deploy token to each site and switch its workflow; traffic moves on the first deploy."
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn cfg(d: &Path) -> Config {
        let c = Config::for_tests(d);
        std::fs::create_dir_all(&c.proc_root).unwrap();
        std::fs::write(
            c.proc_root.join("meminfo"),
            "MemTotal: 1015808 kB\nMemAvailable: 760000 kB\n",
        )
        .unwrap();
        c
    }

    fn run(cmd: Command, c: &Config) -> (u8, String, String) {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let code = execute(cmd, c, &mut o, &mut e);
        (
            code,
            String::from_utf8(o).unwrap(),
            String::from_utf8(e).unwrap(),
        )
    }

    #[test]
    fn parsing() {
        assert_eq!(parse(&a(&[])), Command::Serve);
        assert_eq!(parse(&a(&["serve"])), Command::Serve);
        assert_eq!(
            parse(&a(&["init"])),
            Command::Init {
                reset_password: false
            }
        );
        assert_eq!(
            parse(&a(&["init", "--reset-password"])),
            Command::Init {
                reset_password: true
            }
        );
        assert_eq!(
            parse(&a(&["restore", "b.bin", "--force"])),
            Command::Restore {
                file: "b.bin".into(),
                force: true
            }
        );
        assert_eq!(
            parse(&a(&["restore", "b.bin"])),
            Command::Restore {
                file: "b.bin".into(),
                force: false
            }
        );
        assert!(matches!(parse(&a(&["restore"])), Command::Usage(_)));
        assert_eq!(parse(&a(&["key", "export"])), Command::KeyExport);
        assert!(matches!(parse(&a(&["key"])), Command::Unknown(_)));
        assert!(matches!(parse(&a(&["key", "rotate"])), Command::Unknown(_)));
        assert_eq!(
            parse(&a(&["import-site-ctl", "/d", "--dry-run"])),
            Command::ImportSiteCtl {
                dir: "/d".into(),
                dry: true
            }
        );
        assert_eq!(
            parse(&a(&["import-site-ctl", "/d"])),
            Command::ImportSiteCtl {
                dir: "/d".into(),
                dry: false
            }
        );
        assert!(matches!(parse(&a(&["import-site-ctl"])), Command::Usage(_)));
        assert!(matches!(
            parse(&a(&["import-site-ctl", "--dry-run"])),
            Command::Usage(_)
        ));
        assert_eq!(parse(&a(&["caddy-config"])), Command::CaddyConfig);
        for v in ["version", "--version", "-V"] {
            assert_eq!(parse(&a(&[v])), Command::Version);
        }
        for h in ["help", "--help", "-h"] {
            assert_eq!(parse(&a(&[h])), Command::Help);
        }
        assert_eq!(
            parse(&a(&["frobnicate"])),
            Command::Unknown("frobnicate".into())
        );
    }

    #[test]
    fn entry_runs_non_serve_commands() {
        assert_eq!(entry(&a(&["version"])), 0);
        assert_eq!(entry(&a(&["bogus-command"])), 2);
        assert_eq!(entry(&a(&["restore"])), 2);
    }

    #[test]
    fn simple_commands_write_to_the_right_stream() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        let (code, o, e) = run(Command::Version, &c);
        assert_eq!(
            (code, o.trim(), e.as_str()),
            (0, format!("rustle {VERSION}").as_str(), "")
        );
        let (code, o, _) = run(Command::Help, &c);
        assert!(code == 0 && o.contains("USAGE:"));
        let (code, o, e) = run(Command::Unknown("zzz".into()), &c);
        assert!(
            code == 2
                && o.is_empty()
                && e.contains("unknown command 'zzz'")
                && e.contains("USAGE:")
        );
        let (code, _, e) = run(Command::Usage("usage: x"), &c);
        assert!(code == 2 && e.trim() == "usage: x");
        assert_eq!(run(Command::Serve, &c).0, 0);
    }

    #[test]
    fn init_key_export_caddy_config_and_restore_flow() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        // before init there is no key
        let (code, o, e) = run(Command::KeyExport, &c);
        assert!(code == 1 && o.is_empty() && e.starts_with("error: "), "{e}");

        let (code, o, _) = run(
            Command::Init {
                reset_password: false,
            },
            &c,
        );
        assert_eq!(code, 0);
        assert!(
            o.contains("MASTER KEY") && o.contains("username: admin") && o.contains("password: "),
            "{o}"
        );
        let (code, o2, _) = run(
            Command::Init {
                reset_password: false,
            },
            &c,
        );
        assert!(
            code == 0 && !o2.contains("MASTER KEY") && o2.contains("already exists"),
            "{o2}"
        );

        let (code, key, _) = run(Command::KeyExport, &c);
        assert!(code == 0 && key.trim().len() == 64, "{key}");
        let (code, j, _) = run(Command::CaddyConfig, &c);
        assert!(code == 0 && j.contains("\"apps\""), "{j}");

        let (code, _, e) = run(
            Command::Restore {
                file: "/nonexistent".into(),
                force: true,
            },
            &c,
        );
        assert!(code == 1 && e.starts_with("error: "), "{e}");
    }

    #[test]
    fn import_reports_dry_run_and_errors() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        run(
            Command::Init {
                reset_password: false,
            },
            &c,
        );
        let (code, _, e) = run(
            Command::ImportSiteCtl {
                dir: "/nonexistent-dir".into(),
                dry: true,
            },
            &c,
        );
        assert!(code == 1 && e.starts_with("error: "), "{e}");
        let apps = d.path().join("apps");
        std::fs::create_dir_all(apps.join("sites.d")).unwrap();
        let (code, o, _) = run(
            Command::ImportSiteCtl {
                dir: apps.display().to_string(),
                dry: true,
            },
            &c,
        );
        assert!(
            code == 0 && o.contains("would be imported") && !o.contains("Open the dashboard"),
            "{o}"
        );
        let (code, o, _) = run(
            Command::ImportSiteCtl {
                dir: apps.display().to_string(),
                dry: false,
            },
            &c,
        );
        assert!(
            code == 0 && o.contains(" imported, ") && o.contains("Open the dashboard"),
            "{o}"
        );
    }
}
