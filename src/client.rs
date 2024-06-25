use anyhow::Result;
use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::{TokioExecutor, TokioIo};
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
    let (client, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(conn))
        .await?;

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

        let req = hyper::Request::builder()
            .uri(uri.clone())
            .body(Empty::<Bytes>::default())
            .unwrap();

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
                Ok(rsp) => rsp.status(),
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