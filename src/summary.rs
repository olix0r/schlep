use anyhow::Result;
use tokio::{sync::mpsc, time};
use tracing::Instrument;

pub struct Summary {
    total: u64,
    success_rate: f64,
    cancel_rate: f64,
    load: f64,
    p50: time::Duration,
    p90: time::Duration,
    p99: time::Duration,
}

#[derive(Clone, Debug)]
pub struct SummaryTx {
    tx: mpsc::Sender<Event>,
}

#[derive(Clone, Debug)]
pub struct SummaryTxRsp {
    start: time::Instant,
    tx: Option<mpsc::Sender<Event>>,
}

enum Event {
    Request(time::Instant),
    Response {
        status: Option<hyper::StatusCode>,
        elapsed: time::Duration,
        time: time::Instant,
    },
}

#[derive(Debug)]
struct Ewma {
    value: f64,
    decay: f64,
    timestamp: time::Instant,
}

#[derive(Debug)]
struct LoadAvg {
    avg: Ewma,
    in_flight: u32,
}

pub fn spawn(interval: time::Duration) -> SummaryTx {
    let (tx, mut rx) = mpsc::channel(1_000_000);
    let mut durations = hdrhistogram::Histogram::<u64>::new(3).unwrap();

    let mut load = LoadAvg {
        avg: Ewma::new(interval, time::Instant::now()),
        in_flight: 0,
    };

    tokio::spawn(
        async move {
            let mut timer = time::interval(interval);
            timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            timer.reset();

            loop {
                timer.tick().await;
                durations.clear();
                let Ok(Summary {
                    total,
                    success_rate,
                    cancel_rate,
                    load,
                    p50,
                    p90,
                    p99,
                }) = summarize(&mut rx, &mut durations, &mut load)
                else {
                    return;
                };
                tracing::info!(
                    target: "schlep",
                    rsps = total,
                    ok = %format_args!("{:.1}", success_rate * 100.0),
                    x = %format_args!("{:.1}", cancel_rate * 100.0),
                    load = %format_args!("{:.01}", cancel_rate * load),
                    p50 = %format_args!("{:.03}", p50.as_secs_f64()),
                    p90 = %format_args!("{:.03}", p90.as_secs_f64()),
                    p99 = %format_args!("{:.03}", p99.as_secs_f64()),
                );
            }
        }
        .in_current_span(),
    );

    SummaryTx { tx }
}

fn summarize(
    rx: &mut mpsc::Receiver<Event>,
    durations: &mut hdrhistogram::Histogram<u64>,
    load: &mut LoadAvg,
) -> Result<Summary> {
    let (mut total, mut success, mut cancel) = (0, 0, 0);

    loop {
        let ev = match rx.try_recv() {
            Ok(ev) => ev,
            Err(mpsc::error::TryRecvError::Disconnected) if total == 0 => {
                anyhow::bail!("disonnected")
            }
            Err(_) => break,
        };
        match ev {
            Event::Request(time) => {
                load.avg.add(load.in_flight.into(), time);
                load.in_flight += 1;
            }
            Event::Response {
                status,
                elapsed,
                time,
            } => {
                load.avg.add(load.in_flight.into(), time);
                load.in_flight -= 1;
                durations.saturating_record(elapsed.as_millis() as u64);
                match status {
                    Some(s) if s.is_success() => success += 1,
                    Some(_) => {}
                    None => cancel += 1,
                }
                total += 1;
            }
        }
    }

    let cancel_rate = cancel as f64 / total as f64;
    let success_rate = success as f64 / (total - cancel) as f64;
    let p50 = durations.value_at_quantile(0.5);
    let p90 = durations.value_at_quantile(0.9);
    let p99 = durations.value_at_quantile(0.99);

    durations.clear();

    Ok(Summary {
        total,
        success_rate,
        cancel_rate,
        load: load.avg.get(),
        p50: time::Duration::from_millis(p50),
        p90: time::Duration::from_millis(p90),
        p99: time::Duration::from_millis(p99),
    })
}

impl SummaryTx {
    pub fn request(&self, start: time::Instant) -> SummaryTxRsp {
        if self.tx.try_send(Event::Request(start)).is_err() {
            tracing::error!("Request channel full");
            return SummaryTxRsp { start, tx: None };
        }

        SummaryTxRsp {
            start,
            tx: Some(self.tx.clone()),
        }
    }
}

impl SummaryTxRsp {
    pub fn response(mut self, status: hyper::StatusCode, end: time::Instant) {
        self.respond(Some(status), end);
    }

    fn respond(&mut self, status: Option<hyper::StatusCode>, end: time::Instant) {
        if let Some(tx) = self.tx.take() {
            if tx
                .try_send(Event::Response {
                    status,
                    elapsed: end.saturating_duration_since(self.start),
                    time: end,
                })
                .is_err()
            {
                tracing::error!("Response channel full");
            }
        }
    }
}

impl Drop for SummaryTxRsp {
    fn drop(&mut self) {
        self.respond(None, time::Instant::now())
    }
}

// === impl Ewma ===

impl Ewma {
    fn new(decay: time::Duration, timestamp: time::Instant) -> Self {
        Self {
            decay: decay.as_secs_f64(),
            timestamp,
            value: f64::INFINITY,
        }
    }

    fn get(&self) -> f64 {
        self.value
    }

    fn add(&mut self, value: f64, ts: time::Instant) {
        if ts <= self.timestamp {
            return;
        }
        if self.value == f64::INFINITY {
            self.value = value;
            self.timestamp = ts;
            return;
        }

        self.value = {
            let elapsed = ts.saturating_duration_since(self.timestamp);
            let alpha = 1.0 - (-elapsed.as_secs_f64() / self.decay).exp();
            self.value * (1.0 - alpha) + value * alpha
        };

        self.timestamp = ts;
    }
}
