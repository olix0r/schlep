use anyhow::{bail, Result};
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
    sync::{self, mpsc, watch},
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

    let rsp_tx = Summary::spawn(time::Duration::from_secs(10));

    loop {
        let (io, _addr) = match listener.accept().await {
            Ok(io) => io,
            Err(error) => {
                tracing::warn!(%error, "Failed to accept connection");
                continue;
            }
        };
        tokio::spawn(server.serve_connection(
            hyper_util::rt::TokioIo::new(io),
            hyper::service::service_fn({
                let params = params.clone();
                let tx = rsp_tx.clone();
                move |req| serve(req, (*params.borrow()).clone(), tx.clone())
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

async fn serve<B>(
    req: hyper::Request<B>,
    mut params: Params,
    tx: mpsc::Sender<Event>,
) -> Result<hyper::Response<Empty<Bytes>>, Infallible> {
    let start = time::Instant::now();

    let mut status = hyper::StatusCode::NO_CONTENT;

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

    if params.fail_rate > 0.0 && rand::thread_rng().gen::<f64>() < params.fail_rate {
        status = hyper::StatusCode::INTERNAL_SERVER_ERROR;
    }

    tracing::debug!(%status, elapsed = ?start.elapsed());
    if tx.try_send((status, start.elapsed())).is_err() {
        tracing::error!("Response channel full");
    }

    Ok::<_, Infallible>(
        hyper::Response::builder()
            .status(status)
            .body(Empty::<Bytes>::default())
            .unwrap(),
    )
}

async fn spawn_config(path: Option<PathBuf>) -> Result<watch::Receiver<Params>> {
    let Some(path) = path else {
        let (tx, rx) = watch::channel(Params::default());
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

struct SummaryRx {
    durations: hdrhistogram::Histogram<u64>,
    rx: mpsc::Receiver<Event>,
}

type Event = (hyper::StatusCode, time::Duration);

struct Summary {
    total: u64,
    success_rate: f64,
    p50: time::Duration,
    p90: time::Duration,
    p99: time::Duration,
}

impl Summary {
    fn spawn(interval: time::Duration) -> mpsc::Sender<Event> {
        let (tx, rx) = mpsc::channel(1_000_000);
        let mut rx = SummaryRx {
            durations: hdrhistogram::Histogram::<u64>::new(3).unwrap(),
            rx,
        };

        tokio::spawn(async move {
            let mut timer = time::interval(interval);
            timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            timer.reset();

            tracing::info!(" TOTAL  SUCCESS     P50     P90     P99");
            loop {
                timer.tick().await;
                let Ok(Summary {
                    total,
                    success_rate,
                    p50,
                    p90,
                    p99,
                }) = rx.summarize()
                else {
                    return;
                };
                tracing::info!(
                    "{total:6}  {:6.1}%  {:4}ms  {:4}ms  {:4}ms",
                    success_rate * 100.0,
                    p50.as_millis(),
                    p90.as_millis(),
                    p99.as_millis()
                );
            }
        });

        tx
    }
}

impl SummaryRx {
    fn summarize(&mut self) -> Result<Summary> {
        let (mut total, mut success) = (0, 0);

        loop {
            let (status, elapsed) = match self.rx.try_recv() {
                Ok(ev) => ev,
                Err(mpsc::error::TryRecvError::Disconnected) if total == 0 => bail!("disonnected"),
                Err(_) => break,
            };
            self.durations.saturating_record(elapsed.as_millis() as u64);
            if status.is_success() {
                success += 1;
            }
            total += 1;
        }

        let success_rate = success as f64 / total as f64;
        let p50 = self.durations.value_at_quantile(0.5);
        let p90 = self.durations.value_at_quantile(0.9);
        let p99 = self.durations.value_at_quantile(0.99);

        self.durations.clear();

        Ok(Summary {
            total,
            success_rate,
            p50: time::Duration::from_millis(p50),
            p90: time::Duration::from_millis(p90),
            p99: time::Duration::from_millis(p99),
        })
    }
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

    let rsp_tx = Summary::spawn(time::Duration::from_secs(10));

    loop {
        timer.tick().await;

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
