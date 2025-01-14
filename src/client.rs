use anyhow::Result;
use futures::TryFutureExt;
use http_body_util::BodyExt;
use hyper::client::conn::http2::SendRequest;
use prometheus_client::{
    metrics::{counter::Counter, gauge::Gauge},
    registry::Registry,
};
use schlep_proto::schlep_client::SchlepClient;
use std::net::SocketAddr;
use tokio::time;
use tonic::body::BoxBody;
use tower::buffer::Buffer;
use tracing::{info_span, Instrument};

use crate::summary::{self, SummaryTx};

#[derive(Clone, Debug, clap::Parser)]
pub struct Args {
    #[arg()]
    address: String,

    /// The rate at which requests should be sent.
    #[arg(long, default_value = "1.0")]
    rate: f64,

    /// The rate at which requests should fail.
    #[arg(long, default_value = "0.0")]
    fail_rate: f64,

    /// A comma-separated list of p50, p90, and p99 sleep durations in seconds.
    #[arg(long, default_value = "0.0,0.0,0.0")]
    sleep: Durations,

    /// A comma-separated list of p50, p90, and p99 data sizes in bytes.
    #[arg(long, default_value = "0,0,0")]
    data: Data,

    /// Use gRPC instead of simple HTTP.
    #[arg(long, short)]
    grpc: bool,

    /// When using gRPC, use a request streaming client.
    #[arg(long)]
    sink: bool,

    /// When using gRPC, use a request streaming client.
    #[arg(long, default_value = "0.0,0.0,0.0")]
    sink_lifetime: Durations,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    grpc_sinks: Gauge,
    grpc_sink_events: Counter,
}

#[derive(Clone, Debug)]
struct Durations {
    p50: f64,
    p90: f64,
    p99: f64,
}

impl std::str::FromStr for Durations {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.split(',').collect::<Vec<_>>();
        if parts.len() != 3 {
            anyhow::bail!("expected p50,p90,p99");
        }
        Ok(Self {
            p99: parts.pop().unwrap_or_default().parse()?,
            p90: parts.pop().unwrap_or_default().parse()?,
            p50: parts.pop().unwrap_or_default().parse()?,
        })
    }
}

#[derive(Clone, Debug)]
struct Data {
    p50: u32,
    p90: u32,
    p99: u32,
}

impl std::str::FromStr for Data {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.split(',').collect::<Vec<_>>();
        if parts.len() != 3 {
            anyhow::bail!("expected p50:p90:p99");
        }
        Ok(Self {
            p99: parts.pop().unwrap_or_default().parse()?,
            p90: parts.pop().unwrap_or_default().parse()?,
            p50: parts.pop().unwrap_or_default().parse()?,
        })
    }
}

#[derive(Clone, Debug)]
struct GrpcClient(SendRequest<BoxBody>);

pub async fn run(args: Args, metrics: Metrics) -> Result<()> {
    if args.rate.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        anyhow::bail!("--rate must be greater than zero");
    }

    let summary = summary::spawn(time::Duration::from_secs(10));

    let address = args.address.clone();
    loop {
        let (client, peer) = match connect(&address).await {
            Ok((client, peer)) => (client, peer),
            Err(error) => {
                tracing::warn!(%error, %address, "Connection error");
                time::sleep(time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if !args.grpc {
            let error = dispatch(args.clone(), client, summary.clone())
                .instrument(info_span!("dispatch", srv.addr = %peer))
                .await;
            tracing::warn!(%error, %address, "HTTP dispatch error");
            time::sleep(time::Duration::from_secs(1)).await;
            continue;
        }

        let uri = hyper::Uri::builder()
            .scheme("http")
            .authority(&*address)
            .path_and_query("")
            .build()
            .unwrap();
        let client = SchlepClient::with_origin(Buffer::new(GrpcClient(client), 10), uri);
        let error = if args.sink {
            dispatch_grpc_sink(args.clone(), client, summary.clone(), metrics.clone())
                .instrument(info_span!("dispatch", srv.addr = %peer))
                .await
        } else {
            dispatch_grpc(args.clone(), client, summary.clone())
                .instrument(info_span!("dispatch", srv.addr = %peer))
                .await
        };
        tracing::info!(%error, srv.addr = %peer, "gRPC Dispatch error");
        time::sleep(time::Duration::from_secs(1)).await;
    }
}

async fn connect(
    address: &str,
) -> Result<(hyper::client::conn::http2::SendRequest<BoxBody>, SocketAddr)> {
    tracing::debug!(%address, "Connecting");
    let conn = tokio::net::TcpStream::connect(&address).await?;
    let peer = conn.peer_addr()?;

    tracing::info!(%address, srv.addr = %peer, "Connected");
    let (client, conn) = http2_handshake(conn).await?;

    tokio::spawn(
        async move {
            if let Err(error) = conn.await {
                tracing::error!(%error, srv.addr = %peer, "Connection error");
            }
        }
        .in_current_span(),
    );

    Ok((client, peer))
}

fn request(uri: hyper::Uri) -> hyper::Request<BoxBody> {
    hyper::Request::builder()
        .uri(uri)
        .body(BoxBody::default())
        .expect("Request is valid")
}

async fn http2_handshake(
    conn: tokio::net::TcpStream,
) -> hyper::Result<(
    hyper::client::conn::http2::SendRequest<BoxBody>,
    hyper::client::conn::http2::Connection<
        hyper_util::rt::TokioIo<tokio::net::TcpStream>,
        BoxBody,
        hyper_util::rt::TokioExecutor,
    >,
)> {
    hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
        .handshake::<_, BoxBody>(hyper_util::rt::TokioIo::new(conn))
        .await
}

async fn dispatch(
    Args {
        address,
        rate,
        sleep,
        data,
        fail_rate,
        ..
    }: Args,
    mut client: hyper::client::conn::http2::SendRequest<BoxBody>,
    summary: SummaryTx,
) -> anyhow::Error {
    let uri = match format!(
        "http://{address}/?fail-rate={fail_rate}&sleep.p50={}&sleep.p90={}&sleep.p99={}&data.p50={}&data.p90={}&data.p99={}",
        sleep.p50, sleep.p90, sleep.p99,
        data.p50, data.p90, data.p99,
    )
    .parse::<hyper::Uri>() {
        Ok(uri) => uri,
        Err(error) => return error.into(),
    };

    let mut timer = time::interval(time::Duration::from_secs_f64(1.0 / rate));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        timer.tick().await;
        tracing::trace!("Tick");

        let req = request(uri.clone());

        let start = time::Instant::now();
        if let Err(e) = client.ready().await {
            return e.into();
        }
        let call = client.send_request(req);

        let summary = summary.clone();
        tokio::spawn(async move {
            tracing::trace!("Sending request");
            let summary = summary.request(start);
            let res = call.await;
            let elapsed = start.elapsed();
            let status = match res {
                Ok(rsp) => drain(rsp).await,
                Err(error) => {
                    if let Some(error) = std::error::Error::source(&error)
                        .and_then(|e| e.downcast_ref::<h2::Error>())
                    {
                        tracing::info!(%error, "Failed");
                    } else {
                        tracing::warn!(%error, "Failed");
                    }
                    hyper::StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            tracing::trace!(?elapsed, ?status);
            summary.response(status.is_success(), time::Instant::now());
        });
    }
}

async fn dispatch_grpc(
    Args {
        rate,
        sleep,
        data,
        fail_rate,
        ..
    }: Args,
    client: SchlepClient<Buffer<GrpcClient, hyper::Request<BoxBody>>>,
    summary: SummaryTx,
) -> anyhow::Error {
    let params = schlep_proto::Params {
        fail_rate: fail_rate as f32,
        sleep: Some(schlep_proto::params::Sleep {
            p50: sleep.p50 as f32,
            p90: sleep.p90 as f32,
            p99: sleep.p99 as f32,
        }),
        data: Some(schlep_proto::params::Data {
            p50: data.p50,
            p90: data.p90,
            p99: data.p99,
        }),
    };

    let mut timer = time::interval(time::Duration::from_secs_f64(1.0 / rate));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        timer.tick().await;
        tracing::trace!("Tick");

        let mut client = client.clone();
        let summary = summary.clone();
        tokio::spawn(async move {
            tracing::trace!("Sending request");
            let start = time::Instant::now();
            let summary = summary.request(start);
            let rsp = client.get(tonic::Request::new(params)).await;
            let elapsed = start.elapsed();
            tracing::trace!(?elapsed, ?rsp, "get");
            summary.response(rsp.is_ok(), time::Instant::now());
        });
    }
}

async fn dispatch_grpc_sink(
    Args {
        rate,
        sleep,
        data,
        sink_lifetime,
        ..
    }: Args,
    client: SchlepClient<Buffer<GrpcClient, hyper::Request<BoxBody>>>,
    summary: SummaryTx,
    metrics: Metrics,
) -> anyhow::Error {
    let mut timer = time::interval(time::Duration::from_secs_f64(1.0 / rate));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    let mut sink_count = 0;
    loop {
        timer.tick().await;
        tracing::trace!("Tick");

        let mut client = client.clone();
        let summary = summary.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            tracing::trace!("Sending request");
            let start = time::Instant::now();
            let summary = summary.request(start);

            struct Guard(Gauge);
            impl Drop for Guard {
                #[inline]
                fn drop(&mut self) {
                    self.0.dec();
                }
            }
            metrics.grpc_sinks.inc();
            let guard = Guard(metrics.grpc_sinks.clone());

            let (tx, rx) = tokio::sync::mpsc::channel(2);
            sink_count += 1;
            let sink_events = metrics.grpc_sink_events.clone();
            tokio::spawn(
                async move {
                    let lifetime = crate::gen_sleep(
                        0.0,
                        sink_lifetime.p50,
                        sink_lifetime.p90,
                        sink_lifetime.p99,
                        24.0 * 60.0 * 60.0,
                    );
                    tokio::pin! {
                        let expiry = time::sleep(lifetime);
                    }

                    loop {
                        let tx = tokio::select! {
                            biased;
                            _ = &mut expiry => break,
                            res = tx.reserve() => {
                                let Ok(tx) = res else {  break };
                                tx
                            }
                        };

                        let data = crate::gen_bytes(0, data.p50, data.p90, data.p99, 4194000);
                        tx.send(schlep_proto::Ack { data });
                        sink_events.inc();

                        let pause = crate::gen_sleep(
                            0.0,
                            sleep.p50,
                            sleep.p90,
                            sleep.p99,
                            12.0 * 60.0 * 60.0,
                        );
                        tokio::select! {
                            biased;
                            _ = &mut expiry => break,
                            _ = time::sleep(pause) => {},
                        }
                    }

                    drop(guard);
                }
                .instrument(info_span!("sink", n = %sink_count)),
            );

            let rsp = client
                .sink(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
                .await;
            let elapsed = start.elapsed();
            tracing::trace!(?elapsed, ?rsp, "sink");

            summary.response(rsp.is_ok(), time::Instant::now());
        });
    }
}

async fn drain(rsp: hyper::Response<hyper::body::Incoming>) -> hyper::StatusCode {
    let (head, mut body) = rsp.into_parts();
    while let Some(res) = body.frame().await {
        if res.is_err() {
            return hyper::StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    head.status
}

impl tower::Service<hyper::Request<BoxBody>> for GrpcClient {
    type Response = hyper::Response<BoxBody>;
    type Error = hyper::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = hyper::Result<Self::Response>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<hyper::Result<()>> {
        self.0.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: hyper::Request<BoxBody>) -> Self::Future {
        Box::pin(
            self.0
                .send_request(req)
                .map_ok(|rsp| rsp.map(tonic::body::boxed)),
        )
    }
}

impl Metrics {
    pub fn register(reg: &mut Registry) -> Self {
        let grpc_sinks = Gauge::default();
        reg.register(
            "grpc_sinks",
            "The number of active gRPC sinkc alls",
            grpc_sinks.clone(),
        );

        let grpc_sink_events = Counter::default();
        reg.register(
            "grpc_sink_events",
            "The number of gRPC sink events",
            grpc_sink_events.clone(),
        );

        Self {
            grpc_sinks,
            grpc_sink_events,
        }
    }
}
