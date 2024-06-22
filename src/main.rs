use anyhow::Result;
use clap::Parser;

mod client;
mod server;
mod summary;

#[derive(Clone, Debug, Parser)]
struct Args {
    #[command(subcommand)]
    command: Cmd,

    #[clap(long, env = "SCHLEP_LOG", default_value = "schlep=info,warn")]
    log: kubert::LogFilter,

    #[arg(long, env = "SCHLEP_LOG_FORMAT", default_value = "plain")]
    log_format: kubert::LogFormat,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum Cmd {
    Client(client::Args),
    Server(server::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let Args {
        command,
        log_format,
        log,
    } = Args::try_parse()?;

    log_format.try_init(log)?;

    let (shutdown, handle) = kubert::shutdown::sigint_or_sigterm()?;
    tokio::spawn(async move {
        let release = handle.signaled().await;
        tracing::info!("Shutting down");
        drop(release);
    });

    let run = async move {
        match command {
            Cmd::Server(args) => server::run(args).await,
            Cmd::Client(args) => client::run(args).await,
        }
    };

    tokio::select! {
        res = run => res,
        _ = shutdown.signaled() => Ok(()),
    }
}
