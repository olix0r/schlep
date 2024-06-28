use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use std::net::SocketAddr;
use tokio::time;
use tracing::{info_span, Instrument};

use crate::summary::{self, SummaryTx};

#[derive(Clone, Debug, clap::Parser)]
pub struct Args {
    #[arg()]
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

pub async fn run(
    Args {
        address,
        rate,
        fail_rate,
        sleep_p50,
        sleep_p90,
        sleep_p99,
    }: Args,
) -> Result<()> {
    if rate.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        anyhow::bail!("--rate must be greater than zero");
    }

    let uri = format!(
        "http://{address}/?p50={sleep_p50}&p90={sleep_p90}&p99={sleep_p99}&fail-rate={fail_rate}"
    )
    .parse::<hyper::Uri>()?;

    let summary = summary::spawn(time::Duration::from_secs(10));

    loop {
        match connect(&address).await {
            Ok((client, peer)) => {
                let error = dispatch(rate, uri.clone(), client, summary.clone())
                    .instrument(info_span!("dispatch", srv.addr = %peer))
                    .await;
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
) -> Result<(
    hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
    SocketAddr,
)> {
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

type Body = http_body_util::Empty<hyper::body::Bytes>;

fn request(uri: hyper::Uri) -> hyper::Request<Body> {
    hyper::Request::builder()
        .uri(uri)
        .body(Body::default())
        .expect("Request is valid")
}

async fn http2_handshake(
    conn: tokio::net::TcpStream,
) -> hyper::Result<(
    hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
    hyper::client::conn::http2::Connection<
        hyper_util::rt::TokioIo<tokio::net::TcpStream>,
        Body,
        hyper_util::rt::TokioExecutor,
    >,
)> {
    hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
        .handshake::<_, Body>(hyper_util::rt::TokioIo::new(conn))
        .await
}

async fn dispatch(
    rate: f64,
    uri: hyper::Uri,
    mut client: hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
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
            summary.response(status, time::Instant::now());
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
