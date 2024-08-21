use anyhow::Result;
use futures::TryFutureExt;
use http_body_util::BodyExt;
use hyper::client::conn::http2::SendRequest;
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
    sleep: Sleep,

    /// A comma-separated list of p50, p90, and p99 data sizes in bytes.
    #[arg(long, default_value = "0,0,0")]
    data: Data,

    /// Use gRPC instead of simple HTTP.
    #[arg(long, short)]
    grpc: bool,
}

#[derive(Clone, Debug)]
struct Sleep {
    p50: f64,
    p90: f64,
    p99: f64,
}

impl std::str::FromStr for Sleep {
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

pub async fn run(
    Args {
        address,
        rate,
        fail_rate,
        sleep,
        data,
        grpc,
    }: Args,
) -> Result<()> {
    if rate.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        anyhow::bail!("--rate must be greater than zero");
    }

    let uri = {
        format!(
            "http://{address}/?fail-rate={fail_rate}&sleep.p50={}&sleep.p90={}&sleep.p99={}&data.p50={}&data.p90={}&data.p99={}",
            sleep.p50, sleep.p90, sleep.p99,
            data.p50, data.p90, data.p99,
        )
        .parse::<hyper::Uri>()?
    };

    let req = schlep_proto::Params {
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

    let summary = summary::spawn(time::Duration::from_secs(10));

    loop {
        match connect(&address).await {
            Ok((client, peer)) => {
                let error = if grpc {
                    let uri = hyper::Uri::builder()
                        .scheme("http")
                        .authority(&*address)
                        .path_and_query("")
                        .build()
                        .unwrap();
                    let client =
                        SchlepClient::with_origin(Buffer::new(GrpcClient(client), 10), uri);
                    dispatch_grpc(rate, req, client, summary.clone())
                        .instrument(info_span!("dispatch", srv.addr = %peer))
                        .await
                } else {
                    dispatch(rate, uri.clone(), client, summary.clone())
                        .instrument(info_span!("dispatch", srv.addr = %peer))
                        .await
                };
                tracing::info!(%error, srv.addr = %peer, "Dispatch error");
            }
            Err(error) => {
                tracing::warn!(%error, %address, "Connection error");
            }
        }

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
    rate: f64,
    uri: hyper::Uri,
    mut client: hyper::client::conn::http2::SendRequest<BoxBody>,
    summary: SummaryTx,
) -> anyhow::Error {
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
                    tracing::warn!(%error, "Failed");
                    hyper::StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            tracing::trace!(?elapsed, ?status);
            summary.response(status.is_success(), time::Instant::now());
        });
    }
}

async fn dispatch_grpc(
    rate: f64,
    params: schlep_proto::Params,
    client: SchlepClient<Buffer<GrpcClient, hyper::Request<BoxBody>>>,
    summary: SummaryTx,
) -> anyhow::Error {
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
            tracing::trace!(?elapsed, ?rsp);
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
