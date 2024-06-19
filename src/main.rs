use bytes::Bytes;
use clap::Parser;
use http_body_util::Empty;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rand::Rng;
use std::{
    convert::Infallible,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};
use tokio::time;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Args {
        command,
        log_format,
        log,
    } = Args::try_parse()?;

    log_format.try_init(log)?;

    let (shutdown, _handle) = kubert::shutdown::sigint_or_sigterm()?;

    match command {
        Cmd::Server(ServerArgs {
            port,
            max_concurrent_streams,
        }) => {
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

            tokio::pin! {
                let signaled = shutdown.signaled();
            }
            loop {
                let (io, _addr) = tokio::select! {
                    res = listener.accept() => res?,
                    _ = &mut signaled => {
                        tracing::info!("Shutting down");
                        break
                    }
                };

                let server = server.clone();
                tokio::spawn(server.serve_connection(
                    hyper_util::rt::TokioIo::new(io),
                    hyper::service::service_fn(|req| async move {
                        let start = time::Instant::now();

                        let mut status = 204;
                        let mut max_sleep = 0.0;
                        let mut fail_rate = 0.0;

                        if let Some(q) = req.uri().query() {
                            for pair in q.split('&') {
                                if let Some((key, val)) = pair.split_once('=') {
                                    if key.eq_ignore_ascii_case("fail-rate") {
                                        if let Ok(f) = val.parse() {
                                            if f > 0.0 {
                                                fail_rate = f;
                                            }
                                        }
                                    } else if key.eq_ignore_ascii_case("max-sleep") {
                                        if let Ok(s) = val.parse() {
                                            if s > 0.0 {
                                                max_sleep = s;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if max_sleep > 0.0 {
                            let sleep = rand::thread_rng().gen_range(0.0..=max_sleep);
                            tracing::debug!(?sleep);
                            time::sleep(time::Duration::from_secs_f64(sleep)).await;
                        }

                        if fail_rate > 0.0 {
                            if rand::thread_rng().gen::<f64>() < fail_rate {
                                status = 500;
                            }
                        }
                        tracing::info!(
                            status,
                            fail_rate,
                            elapsed = start.elapsed().as_secs_f64(),
                            max_sleep
                        );

                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .body(Empty::<Bytes>::default())
                                .unwrap(),
                        )
                    }),
                ));
            }

            Ok(())
        }

        Cmd::Client(ClientArgs {
            address,
            rate,
            max_sleep,
            fail_rate,
        }) => {
            if !(rate > 0.0) {
                anyhow::bail!("--rate must be greater than zero");
            }

            let uri = format!("http://{address}/?max-sleep={max_sleep}&fail-rate={fail_rate}")
                .parse::<hyper::Uri>()?;

            tokio::pin! {
                let signaled = shutdown.signaled();
            }

            // Connect to the server.
            let (mut client, conn) = loop {
                match tokio::net::TcpStream::connect(&address).await {
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

            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = &mut signaled => {
                        tracing::info!("Shutting down");
                        break;
                    }
                }

                let req = hyper::Request::builder()
                    .uri(uri.clone())
                    .body(Empty::<Bytes>::default())
                    .unwrap();
                let start = time::Instant::now();

                tokio::select! {
                    res = client.ready() => res?,
                    _ = &mut signaled => {
                        tracing::info!("Shutting down");
                        break;
                    }
                }

                let call = client.send_request(req);
                tokio::spawn(async move {
                    match call.await {
                        Ok(res) => {
                            tracing::info!(status = ?res.status(), elapsed = start.elapsed().as_secs_f64());
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Request failed");
                        }
                    }
                });
            }

            Ok(())
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
    max_sleep: f64,

    #[arg(long, default_value = "0.0")]
    fail_rate: f64,
}

#[derive(Clone, Debug, clap::Parser)]
struct ServerArgs {
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    max_concurrent_streams: Option<u32>,
}
