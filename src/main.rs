use cpaq::{
    cli::{Args, HELP, Mode},
    config::Config,
    paths::Paths,
};
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

fn execute(args: Args) -> Result<u8, String> {
    if args.mode == Mode::Help {
        print!("{HELP}");
        return Ok(0);
    }
    if args.mode == Mode::Version {
        println!("cpaq {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let paths = Paths::from_env()?;
    let config = Config::load(&paths.config_file())?;
    if args.mode == Mode::Ping {
        return cpaq::tokenmaxx::run(&args, &paths, &config).map(|code| code as u8);
    }
    if args.mode == Mode::Auto {
        return cpaq::auto::run(&args, &paths, &config).map(|code| code as u8);
    }
    if args.mode == Mode::UnitsApply {
        let executable = std::env::current_exe()
            .map_err(|_| "Cannot determine cpaq executable path".to_string())?;
        cpaq::systemd::apply(&config, &paths, &executable, &cpaq::systemd::SystemRunner)?;
        println!("tokenmaxx: units applied");
        return Ok(0);
    }
    if args.mode == Mode::Default && io::stdout().is_terminal() {
        cpaq::tui::run(paths, config, args)?;
        return Ok(0);
    }
    let dir = paths.resolve_auth(args.auth_dir.as_deref(), config.auth_dir())?;
    let accounts = cpaq::quota::fetch(&dir, &paths, &config, &args.providers, args.raw)?;
    let failures = cpaq::quota::query_error_count(&accounts);
    let text = if args.json {
        serde_json::to_string_pretty(&accounts)
            .map_err(|_| "Cannot serialize quotas".to_string())?
            + "\n"
    } else {
        cpaq::quota::render_human(&accounts, &dir, cpaq::time::now())
    };
    if io::stdout().lock().write_all(text.as_bytes()).is_err() {
        return Ok(1);
    }
    if failures != 0 {
        eprintln!("({failures} query error(s))");
    }
    Ok(u8::from(failures != 0))
}

fn main() -> ExitCode {
    let result = Args::parse(std::env::args().skip(1)).and_then(execute);
    match result {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}
