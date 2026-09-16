use std::{net::SocketAddr, process::ExitCode};

use netburrow_relay::{AllowedGroups, Config, serve};

#[tokio::main]
async fn main() -> ExitCode {
    match parse_args() {
        Ok((config, true)) => {
            if let Err(error) = config.validate() {
                eprintln!("invalid configuration: {error}");
                return ExitCode::FAILURE;
            }
            println!("configuration is valid");
            ExitCode::SUCCESS
        }
        Ok((config, false)) => match serve(config, shutdown_signal()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("relay failed: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!(
                "{error}\nusage: netburrow-relay [--bind ADDRESS] [--max-clients COUNT] --allowed-groups-file PATH [--check-config]"
            );
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result { eprintln!("shutdown signal failed: {error}"); }
                    }
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => eprintln!("cannot install termination handler: {error}"),
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("shutdown signal failed: {error}");
    }
}

fn parse_args() -> Result<(Config, bool), String> {
    let mut config = Config::default();
    let mut check = false;
    let mut allowed_groups_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bind" => {
                config.bind = args
                    .next()
                    .ok_or("--bind needs an address")?
                    .parse::<SocketAddr>()
                    .map_err(|_| "invalid --bind address")?
            }
            "--max-clients" => {
                config.max_clients = args
                    .next()
                    .ok_or("--max-clients needs a value")?
                    .parse()
                    .map_err(|_| "invalid --max-clients value")?
            }
            "--allowed-groups-file" => {
                allowed_groups_path =
                    Some(args.next().ok_or("--allowed-groups-file needs a path")?);
            }
            "--check-config" => check = true,
            "--help" | "-h" => return Err("".into()),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let path = allowed_groups_path.ok_or("--allowed-groups-file is required")?;
    config.allowed_groups =
        AllowedGroups::from_file(std::path::Path::new(&path)).map_err(|error| error.to_string())?;
    Ok((config, check))
}
