use crate::summary::{self, SummaryTx, SummaryTxRsp};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
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
    sleep: SleepParams,
    data: DataParams,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct SleepParams {
    min: f64,
    p50: f64,
    p90: f64,
    p99: f64,
    max: f64,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct DataParams {
    min: u32,
    p50: u32,
    p90: u32,
    p99: u32,
    max: u32,
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
) -> Result<hyper::Response<Full<Bytes>>> {
    let mut status = hyper::StatusCode::NO_CONTENT;

    if let Some(q) = req.uri().query() {
        for param in q.split('&') {
            if let Some((key, val)) = param.split_once('=') {
                if key.eq_ignore_ascii_case("fail-rate") {
                    if let Ok(f) = val.parse::<f64>() {
                        params.fail_rate += f;
                    }
                } else if key.eq_ignore_ascii_case("sleep.min") {
                    if let Ok(v) = val.parse::<f64>() {
                        params.sleep.min += v;
                    }
                } else if key.eq_ignore_ascii_case("sleep.p50") {
                    if let Ok(v) = val.parse::<f64>() {
                        params.sleep.p50 += v;
                    }
                } else if key.eq_ignore_ascii_case("sleep.p90") {
                    if let Ok(v) = val.parse::<f64>() {
                        params.sleep.p90 += v;
                    }
                } else if key.eq_ignore_ascii_case("sleep.p99") {
                    if let Ok(v) = val.parse::<f64>() {
                        params.sleep.p99 += v;
                    }
                } else if key.eq_ignore_ascii_case("sleep.max") {
                    if let Ok(v) = val.parse::<f64>() {
                        params.sleep.max += v;
                    }
                } else if key.eq_ignore_ascii_case("data.min") {
                    if let Ok(v) = val.parse::<u32>() {
                        params.data.min += v;
                    }
                } else if key.eq_ignore_ascii_case("data.p50") {
                    if let Ok(v) = val.parse::<u32>() {
                        params.data.p50 += v;
                    }
                } else if key.eq_ignore_ascii_case("data.p90") {
                    if let Ok(v) = val.parse::<u32>() {
                        params.data.p90 += v;
                    }
                } else if key.eq_ignore_ascii_case("data.p99") {
                    if let Ok(v) = val.parse::<u32>() {
                        params.data.p99 += v;
                    }
                } else if key.eq_ignore_ascii_case("data.max") {
                    if let Ok(v) = val.parse::<u32>() {
                        params.data.max += v;
                    }
                } else {
                    tracing::debug!(key, val, "Unknown query parameter");
                }
            } else {
                tracing::debug!(param, "Unknown query parameter");
            }
        }
    }

    tracing::debug!(?params);

    let sleep = params.gen_sleep();
    if sleep > time::Duration::ZERO {
        tracing::debug!(?sleep);
        time::sleep(sleep).await;
    }

    let mut body = Default::default();
    if params.fail_rate > 0.0 && rand::thread_rng().gen::<f64>() < params.fail_rate {
        status = hyper::StatusCode::INTERNAL_SERVER_ERROR;
    } else {
        body = Bytes::from(params.gen_bytes());
        if !body.is_empty() {
            status = hyper::StatusCode::OK;
        }
    }

    tx.response(status.is_success(), time::Instant::now());
    Ok(hyper::Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(body))
        .unwrap())
}

impl Params {
    fn gen_sleep(&self) -> time::Duration {
        let Self { sleep, .. } = self;
        let mut rng = rand::thread_rng();
        let r = rng.gen::<f64>();

        let secs = if r < 0.5 {
            (r / 0.5) * (sleep.p50 - sleep.min) + sleep.min
        } else if r < 0.9 {
            (r / 0.9) * (sleep.p90 - sleep.p50) + sleep.p50
        } else {
            r * (sleep.p99 - sleep.p90) + sleep.p90
        };
        time::Duration::from_secs_f64(secs.clamp(sleep.min, sleep.max))
    }

    fn gen_bytes(&self) -> Vec<u8> {
        let Self { data, .. } = self;
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
            (r / 0.5) * (data.p50 as f64 - data.min as f64) + data.min as f64
        } else if r < 0.9 {
            (r / 0.9) * (data.p90 as f64 - data.p50 as f64) + data.p50 as f64
        } else {
            r * (data.p99 as f64 - data.p90 as f64) + data.p90 as f64
        }
        .floor())
        .clamp(data.min as f64, data.max as f64) as usize;
        tracing::trace!(?len, ?r, "gen_bytes");

        rng.sample_iter(&LowercaseAlphanumeric).take(len).collect()
    }
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
            sleep,
            data,
        } = req.into_inner();
        params.fail_rate += f64::from(fail_rate);
        if let Some(sleep) = sleep {
            params.sleep.p50 += f64::from(sleep.p50);
            params.sleep.p90 += f64::from(sleep.p90);
            params.sleep.p99 += f64::from(sleep.p99);
        }
        if let Some(data) = data {
            params.data.p50 += data.p50;
            params.data.p90 += data.p90;
            params.data.p99 += data.p99;
        }
        tracing::debug!(?params);

        let sleep = params.gen_sleep();
        if sleep > time::Duration::ZERO {
            tracing::debug!(?sleep);
            time::sleep(sleep).await;
        }

        if params.fail_rate > 0.0 && rand::thread_rng().gen::<f64>() < params.fail_rate {
            summary.response(false, time::Instant::now());
            return Err(tonic::Status::internal("simulated failure"));
        }

        let data = params.gen_bytes();
        tracing::debug!(bytes = data.len());

        summary.response(true, time::Instant::now());
        Ok(tonic::Response::new(Ack { data }))
    }
}
