use crate::summary::{self, SummaryTx, SummaryTxRsp};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::TokioExecutor;
use rand::Rng;
use schlep_proto::Ack;
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};
use tokio::{sync::watch, time};
use tracing::{info_span, Instrument};

#[derive(Clone, Debug, clap::Parser)]
pub struct Args {
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    max_concurrent_streams: Option<u32>,

    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Params {
    fail_rate: f64,
    sleep_p50: f64,
    sleep_p90: f64,
    sleep_p99: f64,
}

#[derive(Clone, Debug)]
struct GrpcServer {
    params: watch::Receiver<Params>,
    summary: SummaryTx,
}

pub async fn run(
    Args {
        port,
        max_concurrent_streams,
        config,
    }: Args,
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

    let summary = summary::spawn(time::Duration::from_secs(10));

    loop {
        let (io, _addr) = match listener.accept().await {
            Ok(io) => io,
            Err(error) => {
                tracing::warn!(%error, "Failed to accept connection");
                continue;
            }
        };
        let grpc = schlep_proto::schlep_server::SchlepServer::new(GrpcServer {
            params: params.clone(),
            summary: summary.clone(),
        });
        tokio::spawn(
            server
                .serve_connection(
                    hyper_util::rt::TokioIo::new(io),
                    hyper::service::service_fn({
                        let params = params.clone();
                        let summary = summary.clone();
                        let grpc = grpc.clone();
                        move |req| {
                            let params = params.clone();
                            let summary = summary.clone();
                            let grpc = grpc.clone();
                            async move {
                                use futures::prelude::*;
                                use tower::ServiceExt;
                                if let Some(ct) = req.headers().get("content-type") {
                                    if ct.as_bytes()[0..16] == *b"application/grpc" {
                                        tracing::trace!("grpc");
                                        return grpc
                                            .clone()
                                            .oneshot(req.map(tonic::body::boxed))
                                            .err_into::<anyhow::Error>()
                                            .await;
                                    }
                                }
                                let p = (*params.borrow()).clone();
                                serve(req, p, summary.request(time::Instant::now()))
                                    .map_ok(|rsp| rsp.map(tonic::body::boxed))
                                    .await
                            }
                        }
                    }),
                )
                .in_current_span(),
        );
    }
}

async fn serve<B>(
    req: hyper::Request<B>,
    mut params: Params,
    tx: SummaryTxRsp,
) -> Result<hyper::Response<Empty<Bytes>>> {
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

    tx.response(status.is_success(), time::Instant::now());
    Ok(hyper::Response::builder()
        .status(status)
        .body(Empty::<Bytes>::default())
        .unwrap())
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
        (r / 0.9) * sleep_p90
    } else {
        r * sleep_p99
    })
}

async fn spawn_config(path: Option<PathBuf>) -> Result<watch::Receiver<Params>> {
    let Some(path) = path else {
        let (tx, rx) = watch::channel(Params::default());
        tokio::spawn(async move {
            tx.closed().await;
        });
        return Ok(rx);
    };

    let span = info_span!("config", path = %path.to_string_lossy());
    spawn_reload(path, time::Duration::from_secs(10))
        .instrument(span)
        .await
}

async fn spawn_reload(path: PathBuf, interval: time::Duration) -> Result<watch::Receiver<Params>> {
    let res = read(&path).await;

    let mut init = res.unwrap_or_else(|error| {
        tracing::warn!(%error, "Failed to read config");
        Default::default()
    });
    let (tx, rx) = watch::channel(init.clone());

    let mut timer = time::interval(interval);
    timer.reset();

    tokio::spawn(
        async move {
            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = tx.closed() => {
                        tracing::debug!("Config channel closed");
                        return;
                    }
                }

                let config = match read(&path).await {
                    Ok(params) => params,
                    Err(error) => {
                        tracing::warn!(%error, "Failed to read config");
                        continue;
                    }
                };
                if config != init {
                    tracing::info!(?config, "Updating");
                    if tx.send(config.clone()).is_err() {
                        tracing::debug!("Config channel closed");
                        return;
                    }
                    init = config;
                }
            }
        }
        .in_current_span(),
    );

    Ok(rx)
}

async fn read(path: &PathBuf) -> Result<Params> {
    let data = tokio::fs::read_to_string(&path).await?;
    if let Ok(params) = serde_json::from_str(&data) {
        return Ok(params);
    }
    serde_yml::from_str(&data).map_err(Into::into)
}

#[tonic::async_trait]
impl schlep_proto::schlep_server::Schlep for GrpcServer {
    async fn get(
        &self,
        req: tonic::Request<schlep_proto::Params>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        let summary = self.summary.request(time::Instant::now());
        let mut params = self.params.borrow().clone();
        let schlep_proto::Params {
            fail_rate,
            sleep_p50,
            sleep_p90,
            sleep_p99,
        } = req.into_inner();
        params.fail_rate += f64::from(fail_rate);
        params.sleep_p50 += f64::from(sleep_p50);
        params.sleep_p90 += f64::from(sleep_p90);
        params.sleep_p99 += f64::from(sleep_p99);
        tracing::debug!(?params);

        let sleep = gen_sleep(&params);
        if sleep > time::Duration::ZERO {
            tracing::debug!(?sleep);
            time::sleep(sleep).await;
        }

        if params.fail_rate > 0.0 && rand::thread_rng().gen::<f64>() < params.fail_rate {
            summary.response(false, time::Instant::now());
            return Err(tonic::Status::internal("simulated failure"));
        }

        summary.response(true, time::Instant::now());
        Ok(tonic::Response::new(Ack {}))
    }
}
