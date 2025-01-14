use anyhow::Result;
use clap::Parser;
use tracing::{info_span, Instrument};

mod client;
mod server;
mod summary;

#[derive(Clone, Debug, Parser)]
struct Args {
    #[command(subcommand)]
    command: Cmd,

    #[clap(flatten)]
    admin: kubert::AdminArgs,

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
        admin,
    } = Args::try_parse()?;

    log_format.try_init(log)?;
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());

    let (shutdown, handle) = kubert::shutdown::sigint_or_sigterm()?;
    tokio::spawn(async move {
        let release = handle.signaled().await;
        tracing::info!("Shutting down");
        drop(release);
    });

    match command {
        Cmd::Server(args) => {
            let mut reg = prometheus_client::registry::Registry::default();
            let metrics = server::Metrics::register(reg.sub_registry_with_prefix("server"));

            admin.into_builder().with_prometheus(reg).bind()?.spawn();
            tokio::select! {
                res = server::run(args, metrics).instrument(info_span!("server", %host)) => res,
                _ = shutdown.signaled() => Ok(()),
            }
        }

        Cmd::Client(args) => {
            let mut reg = prometheus_client::registry::Registry::default();
            let metrics = client::Metrics::register(reg.sub_registry_with_prefix("client"));

            admin.into_builder().with_prometheus(reg).bind()?.spawn();
            tokio::select! {
                res = client::run(args, metrics).instrument(info_span!("client", %host)) => res,
                _ = shutdown.signaled() => Ok(()),
            }
        }
    }
}

/// Generate a sleep duration based on the given percentiles.
fn gen_sleep(min: f64, p50: f64, p90: f64, p99: f64, max: f64) -> std::time::Duration {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let r = rng.gen::<f64>();

    let secs = if r < 0.5 {
        (r / 0.5) * (p50 - min) + min
    } else if r < 0.9 {
        (r / 0.9) * (p90 - p50) + p50
    } else {
        r * (p99 - p90) + p90
    };
    std::time::Duration::from_secs_f64(secs.clamp(min, max))
}

fn gen_bytes(min: u32, p50: u32, p90: u32, p99: u32, max: u32) -> Vec<u8> {
    use rand::Rng;
    struct LowercaseAlphanumeric;

    // Modified from `rand::distributions::Alphanumeric`
    //
    // Copyright 2018 Developers of the Rand project
    // Copyright (c) 2014 The Rust Project Developers
    //
    // Licensed under the Apache License, Version 2.0 (the "License");
    // you may not use this file except in compliance with the License.
    // You may obtain a copy of the License at
    //
    //     http://www.apache.org/licenses/LICENSE-2.0
    //
    // Unless required by applicable law or agreed to in writing, software
    // distributed under the License is distributed on an "AS IS" BASIS,
    // WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
    // See the License for the specific language governing permissions and
    // limitations under the License.
    impl rand::distributions::Distribution<u8> for LowercaseAlphanumeric {
        fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> u8 {
            const RANGE: u32 = 26 + 10;
            const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
            loop {
                let var = rng.next_u32() >> (32 - 6);
                if var < RANGE {
                    return CHARSET[var as usize];
                }
            }
        }
    }

    let mut rng = rand::thread_rng();
    let r = rng.gen::<f64>();

    let len = (if r < 0.5 {
        (r / 0.5) * (p50 as f64 - min as f64) + min as f64
    } else if r < 0.9 {
        (r / 0.9) * (p90 as f64 - p50 as f64) + p50 as f64
    } else {
        r * (p99 as f64 - p90 as f64) + p90 as f64
    }
    .floor())
    .clamp(min as f64, max as f64) as usize;
    tracing::trace!(?len, ?r, "gen_bytes");

    rng.sample_iter(&LowercaseAlphanumeric).take(len).collect()
}
