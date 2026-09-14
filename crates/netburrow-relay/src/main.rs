use std::{future, net::SocketAddr, process::ExitCode};

use netburrow_relay::{Config, serve};

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
        Ok((config, false)) => match serve(config, future::pending()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("relay failed: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!(
                "{error}\nusage: netburrow-relay [--bind ADDRESS] [--max-clients COUNT] [--check-config]"
            );
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<(Config, bool), String> {
    let mut config = Config::default();
    let mut check = false;
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
            "--check-config" => check = true,
            "--help" | "-h" => return Err("".into()),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((config, check))
}
