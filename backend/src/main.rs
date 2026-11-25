mod auth;
mod config;
mod cooldown;
mod http;
mod serve;
mod share;
mod store;

use clap::{Parser, Subcommand};
use config::ServeArguments;
use share::ShareArguments;

#[derive(Parser)]
#[command(
    version,
    about = "Pigeon is an extremely simple file sharing service for self-hosting."
)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the Pigeon server.
    Serve(ServeArguments),
    /// Upload files or directories and create a share.
    Share(ShareArguments),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Arguments::parse().command {
        Command::Serve(args) => serve::run(args).await,
        Command::Share(args) => share::run(args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_a_subcommand_and_scopes_server_options() {
        assert!(Arguments::try_parse_from(["pigeon"]).is_err());
        assert!(Arguments::try_parse_from(["pigeon", "--port", "4000"]).is_err());
        assert!(
            Arguments::try_parse_from(["pigeon", "serve", "--endpoint", "https://host"]).is_err()
        );
        let args = Arguments::try_parse_from(["pigeon", "serve", "--port", "4000"]).unwrap();
        assert!(matches!(args.command, Command::Serve(args) if args.port == 4000));
    }
}
