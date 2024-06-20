use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use http_body_util::Empty;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rand::Rng;
use std::{
    convert::Infallible,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};
use tokio::{
    sync::{self, watch},
    time,
};

#[tokio::main]
async fn main() -> Result<()> {
    let Args {
        command,
        log_format,
        log,
    } = Args::try_parse()?;

    log_format.try_init(log)?;

    let (shutdown, _handle) = kubert::shutdown::sigint_or_sigterm()?;

    match command {
        Cmd::Server(args) => tokio::select! {
            res = run_server(args) => res,
            _ = shutdown.signaled() => {
                tracing::info!("Shutting down");
                Ok(())
            }
        },

        Cmd::Client(ClientArgs {
            address,
            rate,
            fail_rate,
            sleep_p50,
            sleep_p90,
            sleep_p99,
        }) => {
            if rate.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                anyhow::bail!("--rate must be greater than zero");
            }

            let uri = format!("http://{address}/?p50={sleep_p50}&p90={sleep_p90}&p99={sleep_p99}&fail-rate={fail_rate}")
                .parse::<hyper::Uri>()?;

            tokio::select! {
                res = run_client(&address, uri, rate) => res,
                _ = shutdown.signaled() => {
                    tracing::info!("Shutting down");
                    Ok(())
                }
            }
        }
    }
}

#[derive(Clone, Debug, clap::Parser)]
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
    Client(ClientArgs),
    Server(ServerArgs),
}

#[derive(Clone, Debug, clap::Parser)]
struct ClientArgs {
    #[arg(short, long, default_value = "localhost:8080")]
    address: String,

    #[arg(long, default_value = "1.0")]
    rate: f64,

    #[arg(long, default_value = "0.0")]
    fail_rate: f64,

    #[arg(long, default_value = "0.0")]
    sleep_p50: f64,

    #[arg(long, default_value = "0.0")]
    sleep_p90: f64,

    #[arg(long, default_value = "0.0")]
    sleep_p99: f64,
}

#[derive(Clone, Debug, clap::Parser)]
struct ServerArgs {
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    max_concurrent_streams: Option<u32>,

    #[arg(long)]
    config: Option<PathBuf>,
}

async fn run_server(
    ServerArgs {
        port,
        max_concurrent_streams,
        config,
    }: ServerArgs,
) -> Result<()> {
    let mut server = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
    server.max_concurrent_streams(max_concurrent_streams);

    tracing::info!(?port);
    let listener = tokio::net::TcpListener::bind(
        &[
            SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port),
            SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
        ][..],
    )
    .await?;

    let params = spawn_config(config).await?;

    loop {
        let (io, _addr) = listener.accept().await?;

        let params = params.clone();
        let server = server.clone();
        tokio::spawn(server.serve_connection(
            hyper_util::rt::TokioIo::new(io),
            hyper::service::service_fn(move |req| {
                let params = params.clone();
                async move {
                    let start = time::Instant::now();

                    let mut status = 204;
                    let mut params = {
                        let p = params.borrow();
                        (*p).clone()
                    };

                    if let Some(q) = req.uri().query() {
                        for pair in q.split('&') {
                            if let Some((key, val)) = pair.split_once('=') {
                                if key.eq_ignore_ascii_case("fail-rate") {
                                    if let Ok(f) = val.parse::<f64>() {
                                        if f > 0.0 {
                                            params.fail_rate += f;
                                        }
                                    }
                                } else if key.eq_ignore_ascii_case("p50") {
                                    if let Ok(v) = val.parse::<f64>() {
                                        if v > 0.0 {
                                            params.sleep_p50 += v;
                                        }
                                    }
                                } else if key.eq_ignore_ascii_case("p90") {
                                    if let Ok(v) = val.parse::<f64>() {
                                        if v > 0.0 {
                                            params.sleep_p90 += v;
                                        }
                                    }
                                } else if key.eq_ignore_ascii_case("p99") {
                                    if let Ok(v) = val.parse::<f64>() {
                                        if v > 0.0 {
                                            params.sleep_p99 += v;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    tracing::debug!(?params);

                    let sleep = gen_sleep(&params);
                    if sleep > time::Duration::ZERO {
                        tracing::debug!(?sleep);
                        time::sleep(sleep).await;
                    }

                    if params.fail_rate > 0.0 && rand::thread_rng().gen::<f64>() < params.fail_rate
                    {
                        status = 500;
                    }

                    tracing::info!(
                        status,
                        elapsed = ?start.elapsed(),
                    );

                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(status)
                            .body(Empty::<Bytes>::default())
                            .unwrap(),
                    )
                }
            }),
        ));
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Params {
    fail_rate: f64,
    sleep_p50: f64,
    sleep_p90: f64,
    sleep_p99: f64,
}

async fn spawn_config(path: Option<PathBuf>) -> Result<watch::Receiver<Params>> {
    let Some(path) = path else {
        let (tx, rx) = sync::watch::channel(Params::default());
        tokio::spawn(async move {
            tx.closed().await;
        });
        return Ok(rx);
    };

    async fn read(path: &PathBuf) -> Result<Params> {
        let data = tokio::fs::read_to_string(&path).await?;
        if let Ok(params) = serde_json::from_str(&data) {
            return Ok(params);
        }
        serde_yml::from_str(&data).map_err(Into::into)
    }

    let (tx, rx) = sync::watch::channel(read(&path).await.unwrap_or_else(|error| {
        tracing::warn!(%error, "Failed to read config");
        Default::default()
    }));

    let mut timer = time::interval(time::Duration::from_secs(10));
    timer.reset();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = timer.tick() => {}
                _ = tx.closed() => return,
            }

            let config = match read(&path).await {
                Ok(params) => params,
                Err(error) => {
                    tracing::warn!(%error, "Failed to read config");
                    continue;
                }
            };
            tx.send_if_modified(|c| {
                if config != *c {
                    *c = config;
                    true
                } else {
                    false
                }
            });
        }
    });

    Ok(rx)
}

async fn run_client(address: &str, uri: hyper::Uri, rate: f64) -> Result<()> {
    // Connect to the server.
    let (mut client, conn) = loop {
        match tokio::net::TcpStream::connect(address).await {
            Ok(conn) => {
                match hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                    .handshake::<_, Empty<Bytes>>(TokioIo::new(conn))
                    .await
                {
                    Ok(conn) => break conn,
                    Err(error) => {
                        tracing::warn!(%error, address, "Failed to connect");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, address, "Failed to connect");
            }
        };

        time::sleep(time::Duration::from_secs(1)).await;
    };
    tokio::spawn(async move {
        if let Err(error) = conn.await {
            tracing::error!(%error, "Connection error");
        }
    });

    let mut timer = time::interval(time::Duration::from_secs_f64(1.0 / rate));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    let mut report_timer = time::interval(time::Duration::from_secs_f64(10.0));
    report_timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    report_timer.reset();

    let mut success = 0;
    let mut total = 0;
    let mut duration_histo = hdrhistogram::Histogram::<u64>::new(3).unwrap();
    let (rsp_tx, mut rsp_rx) =
        tokio::sync::mpsc::channel::<(hyper::StatusCode, time::Duration)>(1_000_000);

    loop {
        tokio::select! {
            _ = report_timer.tick() => {
                while let Ok((status, elapsed)) = rsp_rx.try_recv() {
                    duration_histo.saturating_record(elapsed.as_millis() as u64);
                    if status.is_success() {
                        success += 1;
                    }
                    total += 1;
                }

                tracing::info!("total={} success={:.01}% p50={}ms p90={}ms p99={}ms",
                    total,
                    success as f64 / total as f64 * 100.0,
                    duration_histo.value_at_quantile(0.5),
                    duration_histo.value_at_quantile(0.9),
                    duration_histo.value_at_quantile(0.99)
                );

                success = 0;
                total = 0;
                duration_histo.clear();
                continue;
            }

            _ = timer.tick() => {}
        }

        let req = hyper::Request::builder()
            .uri(uri.clone())
            .body(Empty::<Bytes>::default())
            .unwrap();

        let start = time::Instant::now();
        client.ready().await?;
        let call = client.send_request(req);
        let rsp_tx = rsp_tx.clone();
        tokio::spawn(async move {
            let res = call.await;
            let elapsed = start.elapsed();
            let status = match res {
                Ok(rsp) => {
                    tracing::debug!(status = ?rsp.status(), ?elapsed);
                    rsp.status()
                }
                Err(error) => {
                    tracing::warn!(%error, ?elapsed, "Request failed");
                    hyper::StatusCode::INTERNAL_SERVER_ERROR
                }
            };

            if rsp_tx.try_send((status, elapsed)).is_err() {
                tracing::error!("Response channel full");
            }
        });
    }
}

fn gen_sleep(
    Params {
        sleep_p50,
        sleep_p90,
        sleep_p99,
        ..
    }: &Params,
) -> time::Duration {
    let mut rng = rand::thread_rng();
    let r = rng.gen::<f64>();

    time::Duration::from_secs_f64(if r < 0.5 {
        (r / 0.5) * sleep_p50
    } else if r < 0.9 {
        sleep_p50 + (r / 0.9) * (sleep_p90 - sleep_p50)
    } else {
        sleep_p90 + r * (sleep_p99 - sleep_p90)
    })
}
