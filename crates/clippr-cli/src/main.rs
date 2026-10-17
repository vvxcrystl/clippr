use anyhow::{bail, Context};
use clippr_core::{
    client::Client,
    config::Config,
    ipc::{CaptureState, Request, Response},
    library::Library,
};
use std::process::{Command, Stdio};

const USAGE: &str = "Usage: clippr save [SECONDS] | status | pause | resume | toggle | overlay | open | list [--json] | config path|show | reset-source | quit";
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        std::process::exit(if args.is_empty() { 2 } else { 0 });
    }
    let valid = match args[0].as_str() {
        "save" => {
            args.len() == 1 || (args.len() == 2 && args[1].parse::<u32>().is_ok_and(|s| s > 0))
        }
        "list" => args.len() == 1 || (args.len() == 2 && args[1] == "--json"),
        "config" => args.len() == 2 && matches!(args[1].as_str(), "path" | "show"),
        "status" | "pause" | "resume" | "toggle" | "overlay" | "open" | "reset-source" | "quit" => {
            args.len() == 1
        }
        _ => false,
    };
    if !valid {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if let Err(e) = run(&args) {
        eprintln!("clippr: {e:#}");
        std::process::exit(1);
    }
}
fn run(args: &[String]) -> anyhow::Result<()> {
    match args[0].as_str() {
        "open" => {
            use std::os::unix::process::CommandExt;
            let sibling = std::env::current_exe()?.with_file_name("clippr-gui");
            let mut cmd = Command::new(if sibling.is_file() {
                sibling
            } else {
                "clippr-gui".into()
            });
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Only async-signal-safe setsid runs between fork and exec.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            cmd.spawn().context("could not launch clippr-gui")?;
            return Ok(());
        }
        "config" => {
            if args[1] == "path" {
                println!("{}", clippr_core::paths::config_file().display());
            } else {
                print!("{}", toml::to_string_pretty(&Config::load()?)?);
            }
            return Ok(());
        }
        "list" => {
            let clips = Library::open(Config::load()?.general.clips_dir).list()?;
            if args.len() == 2 {
                println!("{}", serde_json::to_string_pretty(&clips)?);
            } else {
                for clip in clips {
                    println!("{}\t{:.1}s\t{}", clip.id, clip.duration_secs, clip.title);
                }
            }
            return Ok(());
        }
        _ => (),
    }
    let mut client = Client::connect()?;
    let req = match args[0].as_str() {
        "save" => Request::Save {
            seconds: args.get(1).map(|s| s.parse()).transpose()?,
        },
        "status" => Request::Status,
        "pause" => Request::SetPaused { paused: true },
        "resume" => Request::SetPaused { paused: false },
        "toggle" => match client.request(&Request::Status)? {
            Response::Status { status } => Request::SetPaused {
                paused: status.state != CaptureState::Paused,
            },
            _ => bail!("daemon returned an unexpected response to status"),
        },
        "overlay" => Request::ToggleOverlay,
        "reset-source" => Request::ResetCaptureSource,
        "quit" => Request::Quit,
        _ => unreachable!(),
    };
    match client.request(&req)? {
        Response::Ok => (),
        Response::Status { status } => println!("{}", serde_json::to_string_pretty(&status)?),
        Response::Saved { clip } => println!("{}", clip.file),
        response => bail!("unexpected daemon response: {response:?}"),
    }
    Ok(())
}
