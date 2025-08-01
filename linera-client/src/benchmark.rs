// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    iter,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use hdrhistogram::Histogram;
use linera_base::{
    data_types::Amount,
    identifiers::{AccountOwner, ApplicationId, ChainId},
    listen_for_shutdown_signals,
    time::Instant,
};
use linera_core::{client::ChainClient, Environment};
use linera_execution::{
    committee::Committee,
    system::{Recipient, SystemOperation},
    Operation,
};
use linera_sdk::abis::fungible::{self, FungibleOperation};
use num_format::{Locale, ToFormattedString};
use prometheus_parse::{HistogramCount, Scrape, Value};
use tokio::{
    sync::{mpsc, Barrier, Notify},
    task, time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _};

const PROXY_LATENCY_P99_THRESHOLD: f64 = 400.0;
const LATENCY_METRIC_PREFIX: &str = "linera_proxy_request_latency";

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("Failed to join task: {0}")]
    JoinError(#[from] task::JoinError),
    #[error("Chain client error: {0}")]
    ChainClient(#[from] linera_core::client::ChainClientError),
    #[error("Current histogram count is less than previous histogram count")]
    HistogramCountMismatch,
    #[error("Expected histogram value, got {0:?}")]
    ExpectedHistogramValue(Value),
    #[error("Expected untyped value, got {0:?}")]
    ExpectedUntypedValue(Value),
    #[error("Incomplete histogram data")]
    IncompleteHistogramData,
    #[error("Could not compute quantile")]
    CouldNotComputeQuantile,
    #[error("Bucket boundaries do not match: {0} vs {1}")]
    BucketBoundariesDoNotMatch(f64, f64),
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Io error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Previous histogram snapshot does not exist: {0}")]
    PreviousHistogramSnapshotDoesNotExist(String),
    #[error("No data available yet to calculate p99")]
    NoDataYetForP99Calculation,
    #[error("Unexpected empty bucket")]
    UnexpectedEmptyBucket,
    #[error("Failed to send unit message: {0}")]
    TokioSendUnitError(#[from] mpsc::error::SendError<()>),
    #[error("Failed to create histogram: {0}")]
    HistogramCreationError(#[from] hdrhistogram::CreationError),
    #[error("Failed to record histogram: {0}")]
    HistogramRecordError(#[from] hdrhistogram::RecordError),
    #[error("Failed to send block timings message: {0}")]
    TokioSendBlockTimingsError(#[from] mpsc::error::SendError<BlockTimings>),
}

struct SubmitFastBlockProposalTimings {
    creating_proposal_ms: u64,
    stage_block_execution_ms: u64,
    creating_confirmed_block_ms: u64,
    submitting_block_proposal_ms: u64,
}

struct BlockTimeTimings {
    get_pending_message_bundles_ms: u64,
    submit_fast_block_proposal_ms: u64,
    submit_fast_block_proposal_timings: SubmitFastBlockProposalTimings,
    communicate_chain_updates_ms: u64,
}

pub struct BlockTimings {
    block_time_ms: u64,
    block_time_timings: BlockTimeTimings,
}

struct SubmitFastBlockProposalTimingsHistograms {
    creating_proposal_histogram: Histogram<u64>,
    stage_block_execution_histogram: Histogram<u64>,
    creating_confirmed_block_histogram: Histogram<u64>,
    submitting_block_proposal_histogram: Histogram<u64>,
}

impl SubmitFastBlockProposalTimingsHistograms {
    pub fn new() -> Result<Self, BenchmarkError> {
        Ok(Self {
            creating_proposal_histogram: Histogram::<u64>::new(2)?,
            stage_block_execution_histogram: Histogram::<u64>::new(2)?,
            creating_confirmed_block_histogram: Histogram::<u64>::new(2)?,
            submitting_block_proposal_histogram: Histogram::<u64>::new(2)?,
        })
    }

    pub fn record(
        &mut self,
        submit_fast_block_proposal_timings: SubmitFastBlockProposalTimings,
    ) -> Result<(), BenchmarkError> {
        self.creating_proposal_histogram
            .record(submit_fast_block_proposal_timings.creating_proposal_ms)?;
        self.stage_block_execution_histogram
            .record(submit_fast_block_proposal_timings.stage_block_execution_ms)?;
        self.creating_confirmed_block_histogram
            .record(submit_fast_block_proposal_timings.creating_confirmed_block_ms)?;
        self.submitting_block_proposal_histogram
            .record(submit_fast_block_proposal_timings.submitting_block_proposal_ms)?;
        Ok(())
    }
}

struct BlockTimeTimingsHistograms {
    get_pending_message_bundles_histogram: Histogram<u64>,
    submit_fast_block_proposal_histogram: Histogram<u64>,
    submit_fast_block_proposal_timings_histograms: SubmitFastBlockProposalTimingsHistograms,
    communicate_chain_updates_histogram: Histogram<u64>,
}

impl BlockTimeTimingsHistograms {
    pub fn new() -> Result<Self, BenchmarkError> {
        Ok(Self {
            get_pending_message_bundles_histogram: Histogram::<u64>::new(2)?,
            submit_fast_block_proposal_histogram: Histogram::<u64>::new(2)?,
            submit_fast_block_proposal_timings_histograms:
                SubmitFastBlockProposalTimingsHistograms::new()?,
            communicate_chain_updates_histogram: Histogram::<u64>::new(2)?,
        })
    }

    pub fn record(&mut self, block_time_timings: BlockTimeTimings) -> Result<(), BenchmarkError> {
        self.get_pending_message_bundles_histogram
            .record(block_time_timings.get_pending_message_bundles_ms)?;
        self.submit_fast_block_proposal_histogram
            .record(block_time_timings.submit_fast_block_proposal_ms)?;
        self.submit_fast_block_proposal_timings_histograms
            .record(block_time_timings.submit_fast_block_proposal_timings)?;
        self.communicate_chain_updates_histogram
            .record(block_time_timings.communicate_chain_updates_ms)?;
        Ok(())
    }
}

struct BlockTimingsHistograms {
    block_time_histogram: Histogram<u64>,
    block_time_timings_histograms: BlockTimeTimingsHistograms,
}

impl BlockTimingsHistograms {
    pub fn new() -> Result<Self, BenchmarkError> {
        Ok(Self {
            block_time_histogram: Histogram::<u64>::new(2)?,
            block_time_timings_histograms: BlockTimeTimingsHistograms::new()?,
        })
    }

    pub fn record(&mut self, block_timings: BlockTimings) -> Result<(), BenchmarkError> {
        self.block_time_histogram
            .record(block_timings.block_time_ms)?;
        self.block_time_timings_histograms
            .record(block_timings.block_time_timings)?;
        Ok(())
    }
}

#[derive(Debug)]
struct HistogramSnapshot {
    buckets: Vec<HistogramCount>,
    count: f64,
    sum: f64,
}

pub struct Benchmark<Env: Environment> {
    _phantom: std::marker::PhantomData<Env>,
}

impl<Env: Environment> Benchmark<Env> {
    #[expect(clippy::too_many_arguments)]
    pub async fn run_benchmark(
        num_chain_groups: usize,
        transactions_per_block: usize,
        bps: usize,
        chain_clients: Vec<Vec<ChainClient<Env>>>,
        blocks_infos: Vec<Vec<(Vec<Operation>, AccountOwner)>>,
        committee: Committee,
        health_check_endpoints: Option<String>,
        runtime_in_seconds: Option<u64>,
        delay_between_chain_groups_ms: Option<u64>,
    ) -> Result<(), BenchmarkError> {
        let bps_counts = (0..num_chain_groups)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect::<Vec<_>>();
        let notifier = Arc::new(Notify::new());
        let barrier = Arc::new(Barrier::new(num_chain_groups + 1));

        let shutdown_notifier = CancellationToken::new();
        tokio::spawn(listen_for_shutdown_signals(shutdown_notifier.clone()));

        let bps_control_task = Self::bps_control_task(
            &barrier,
            &shutdown_notifier,
            &bps_counts,
            &notifier,
            transactions_per_block,
            bps,
        );

        let (block_time_quantiles_sender, block_time_quantiles_task) =
            Self::block_time_quantiles_task(&shutdown_notifier);

        let (runtime_control_task, runtime_control_sender) =
            Self::runtime_control_task(&shutdown_notifier, runtime_in_seconds, num_chain_groups);

        let bps_initial_share = bps / num_chain_groups;
        let mut bps_remainder = bps % num_chain_groups;
        let mut join_set = task::JoinSet::<Result<(), BenchmarkError>>::new();
        for (chain_group_index, (chain_group, chain_clients)) in blocks_infos
            .into_iter()
            .zip(chain_clients.into_iter())
            .enumerate()
        {
            let shutdown_notifier_clone = shutdown_notifier.clone();
            let committee = committee.clone();
            let barrier_clone = barrier.clone();
            let block_time_quantiles_sender = block_time_quantiles_sender.clone();
            let bps_count_clone = bps_counts[chain_group_index].clone();
            let notifier_clone = notifier.clone();
            let runtime_control_sender_clone = runtime_control_sender.clone();
            let bps_share = if bps_remainder > 0 {
                bps_remainder -= 1;
                bps_initial_share + 1
            } else {
                bps_initial_share
            };
            join_set.spawn(
                async move {
                    Box::pin(Self::run_benchmark_internal(
                        chain_group_index,
                        bps_share,
                        chain_group,
                        chain_clients,
                        shutdown_notifier_clone,
                        bps_count_clone,
                        committee,
                        block_time_quantiles_sender,
                        barrier_clone,
                        notifier_clone,
                        runtime_control_sender_clone,
                        delay_between_chain_groups_ms,
                    ))
                    .await?;

                    Ok(())
                }
                .instrument(
                    tracing::info_span!("chain_group", chain_group_index = ?chain_group_index),
                ),
            );
        }

        let metrics_watcher =
            Self::metrics_watcher(health_check_endpoints, shutdown_notifier.clone()).await?;

        join_set
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        info!("All benchmark tasks completed");
        bps_control_task.await?;
        if let Some(metrics_watcher) = metrics_watcher {
            metrics_watcher.await??;
        }
        if let Some(runtime_control_task) = runtime_control_task {
            runtime_control_task.await?;
        }
        drop(block_time_quantiles_sender);
        block_time_quantiles_task.await??;

        Ok(())
    }

    // The bps control task will control the BPS from the threads.
    fn bps_control_task(
        barrier: &Arc<Barrier>,
        shutdown_notifier: &CancellationToken,
        bps_counts: &[Arc<AtomicUsize>],
        notifier: &Arc<Notify>,
        transactions_per_block: usize,
        bps: usize,
    ) -> task::JoinHandle<()> {
        let shutdown_notifier = shutdown_notifier.clone();
        let bps_counts = bps_counts.to_vec();
        let notifier = notifier.clone();
        let barrier = barrier.clone();
        task::spawn(
            async move {
                barrier.wait().await;
                let mut one_second_interval = time::interval(time::Duration::from_secs(1));
                loop {
                    if shutdown_notifier.is_cancelled() {
                        info!("Shutdown signal received in bps control task");
                        break;
                    }
                    one_second_interval.tick().await;
                    let current_bps_count: usize = bps_counts
                        .iter()
                        .map(|count| count.swap(0, Ordering::Relaxed))
                        .sum();
                    notifier.notify_waiters();
                    let formatted_current_bps = current_bps_count.to_formatted_string(&Locale::en);
                    let formatted_current_tps = (current_bps_count * transactions_per_block)
                        .to_formatted_string(&Locale::en);
                    let formatted_tps_goal =
                        (bps * transactions_per_block).to_formatted_string(&Locale::en);
                    let formatted_bps_goal = bps.to_formatted_string(&Locale::en);
                    if current_bps_count >= bps {
                        info!(
                            "Achieved {} BPS/{} TPS",
                            formatted_current_bps, formatted_current_tps
                        );
                    } else {
                        warn!(
                            "Failed to achieve {} BPS/{} TPS, only achieved {} BPS/{} TPS",
                            formatted_bps_goal,
                            formatted_tps_goal,
                            formatted_current_bps,
                            formatted_current_tps,
                        );
                    }
                }

                info!("Exiting bps control task");
            }
            .instrument(tracing::info_span!("bps_control")),
        )
    }

    fn block_time_quantiles_task(
        shutdown_notifier: &CancellationToken,
    ) -> (
        mpsc::UnboundedSender<BlockTimings>,
        task::JoinHandle<Result<(), BenchmarkError>>,
    ) {
        let shutdown_notifier = shutdown_notifier.clone();
        let (block_time_quantiles_sender, mut block_time_quantiles_receiver) =
            mpsc::unbounded_channel();
        let block_time_quantiles_task: task::JoinHandle<Result<(), BenchmarkError>> = task::spawn(
            async move {
                let mut histograms = BlockTimingsHistograms::new()?;
                let mut block_time_quantiles_timer = Instant::now();

                while let Some(block_timings) = block_time_quantiles_receiver.recv().await {
                    if shutdown_notifier.is_cancelled() {
                        info!("Shutdown signal received on block time quantiles task");
                        break;
                    }

                    histograms.record(block_timings)?;

                    // Print block time quantiles every 5 seconds.
                    if block_time_quantiles_timer.elapsed().as_secs() >= 5 {
                        for quantile in [0.99, 0.95, 0.90, 0.50] {
                            let formatted_quantile = (quantile * 100.0) as usize;

                            // Overall block timing
                            info!(
                                "Block time p{}: {} ms",
                                formatted_quantile,
                                histograms.block_time_histogram.value_at_quantile(quantile)
                            );

                            // Block time breakdown
                            info!(
                                "  ├─ Get pending message bundles p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .get_pending_message_bundles_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  ├─ Submit fast block proposal p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .submit_fast_block_proposal_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  │  ├─ Creating proposal p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .submit_fast_block_proposal_timings_histograms
                                    .creating_proposal_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  │  ├─ Stage block execution p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .submit_fast_block_proposal_timings_histograms
                                    .stage_block_execution_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  │  ├─ Creating confirmed block p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .submit_fast_block_proposal_timings_histograms
                                    .creating_confirmed_block_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  │  └─ Submitting block proposal p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .submit_fast_block_proposal_timings_histograms
                                    .submitting_block_proposal_histogram
                                    .value_at_quantile(quantile)
                            );
                            info!(
                                "  └─ Communicate chain updates p{}: {} ms",
                                formatted_quantile,
                                histograms
                                    .block_time_timings_histograms
                                    .communicate_chain_updates_histogram
                                    .value_at_quantile(quantile)
                            );
                        }
                        block_time_quantiles_timer = Instant::now();
                    }
                }

                info!("Exiting block time quantiles task");
                Ok(())
            }
            .instrument(tracing::info_span!("block_time_quantiles")),
        );
        (block_time_quantiles_sender, block_time_quantiles_task)
    }

    async fn metrics_watcher(
        health_check_endpoints: Option<String>,
        shutdown_notifier: CancellationToken,
    ) -> Result<Option<task::JoinHandle<Result<(), BenchmarkError>>>, BenchmarkError> {
        if let Some(health_check_endpoints) = health_check_endpoints {
            let metrics_addresses = health_check_endpoints
                .split(',')
                .map(|address| format!("http://{}/metrics", address.trim()))
                .collect::<Vec<_>>();

            let mut previous_histogram_snapshots: HashMap<String, HistogramSnapshot> =
                HashMap::new();
            let scrapes = Self::get_scrapes(&metrics_addresses).await?;
            for (metrics_address, scrape) in scrapes {
                previous_histogram_snapshots.insert(
                    metrics_address,
                    Self::parse_histogram(&scrape, LATENCY_METRIC_PREFIX)?,
                );
            }

            let metrics_watcher: task::JoinHandle<Result<(), BenchmarkError>> = tokio::spawn(
                async move {
                    let mut health_interval = time::interval(time::Duration::from_secs(5));
                    let mut shutdown_interval = time::interval(time::Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            biased;
                            _ = health_interval.tick() => {
                                let result = Self::validators_healthy(&metrics_addresses, &mut previous_histogram_snapshots).await;
                                if let Err(ref err) = result {
                                    info!("Shutting down benchmark due to error: {}", err);
                                    shutdown_notifier.cancel();
                                    break;
                                } else if !result? {
                                    info!("Shutting down benchmark due to unhealthy validators");
                                    shutdown_notifier.cancel();
                                    break;
                                }
                            }
                            _ = shutdown_interval.tick() => {
                                if shutdown_notifier.is_cancelled() {
                                    info!("Shutdown signal received, stopping metrics watcher");
                                    break;
                                }
                            }
                        }
                    }

                    Ok(())
                }
                .instrument(tracing::info_span!("metrics_watcher")),
            );

            Ok(Some(metrics_watcher))
        } else {
            Ok(None)
        }
    }

    fn runtime_control_task(
        shutdown_notifier: &CancellationToken,
        runtime_in_seconds: Option<u64>,
        num_chain_groups: usize,
    ) -> (Option<task::JoinHandle<()>>, Option<mpsc::Sender<()>>) {
        if let Some(runtime_in_seconds) = runtime_in_seconds {
            let (runtime_control_sender, mut runtime_control_receiver) =
                mpsc::channel(num_chain_groups);
            let shutdown_notifier = shutdown_notifier.clone();
            let runtime_control_task = task::spawn(
                async move {
                    let mut chains_started = 0;
                    while runtime_control_receiver.recv().await.is_some() {
                        chains_started += 1;
                        if chains_started == num_chain_groups {
                            break;
                        }
                    }
                    time::sleep(time::Duration::from_secs(runtime_in_seconds)).await;
                    shutdown_notifier.cancel();
                }
                .instrument(tracing::info_span!("runtime_control")),
            );
            (Some(runtime_control_task), Some(runtime_control_sender))
        } else {
            (None, None)
        }
    }

    async fn validators_healthy(
        metrics_addresses: &[String],
        previous_histogram_snapshots: &mut HashMap<String, HistogramSnapshot>,
    ) -> Result<bool, BenchmarkError> {
        let scrapes = Self::get_scrapes(metrics_addresses).await?;
        for (metrics_address, scrape) in scrapes {
            let histogram = Self::parse_histogram(&scrape, LATENCY_METRIC_PREFIX)?;
            let diff = Self::diff_histograms(
                previous_histogram_snapshots.get(&metrics_address).ok_or(
                    BenchmarkError::PreviousHistogramSnapshotDoesNotExist(metrics_address.clone()),
                )?,
                &histogram,
            )?;
            let p99 = match Self::compute_quantile(&diff.buckets, diff.count, 0.99) {
                Ok(p99) => p99,
                Err(BenchmarkError::NoDataYetForP99Calculation) => {
                    info!(
                        "No data available yet to calculate p99 for {}",
                        metrics_address
                    );
                    continue;
                }
                Err(e) => {
                    error!("Error computing p99 for {}: {}", metrics_address, e);
                    return Err(e);
                }
            };

            let last_bucket_boundary = diff.buckets[diff.buckets.len() - 2].less_than;
            if p99 == f64::INFINITY {
                info!(
                    "{} -> Estimated p99 for {} is higher than the last bucket boundary of {:?} ms",
                    metrics_address, LATENCY_METRIC_PREFIX, last_bucket_boundary
                );
            } else {
                info!(
                    "{} -> Estimated p99 for {}: {:.2} ms",
                    metrics_address, LATENCY_METRIC_PREFIX, p99
                );
            }
            if p99 > PROXY_LATENCY_P99_THRESHOLD {
                if p99 == f64::INFINITY {
                    error!(
                        "Proxy of validator {} unhealthy! Latency p99 is too high, it is higher than \
                        the last bucket boundary of {:.2} ms",
                        metrics_address, last_bucket_boundary
                    );
                } else {
                    error!(
                        "Proxy of validator {} unhealthy! Latency p99 is too high: {:.2} ms",
                        metrics_address, p99
                    );
                }
                return Ok(false);
            }
            previous_histogram_snapshots.insert(metrics_address.clone(), histogram);
        }

        Ok(true)
    }

    fn diff_histograms(
        previous: &HistogramSnapshot,
        current: &HistogramSnapshot,
    ) -> Result<HistogramSnapshot, BenchmarkError> {
        if current.count < previous.count {
            return Err(BenchmarkError::HistogramCountMismatch);
        }
        let total_diff = current.count - previous.count;
        let mut buckets_diff: Vec<HistogramCount> = Vec::new();
        for (before, after) in previous.buckets.iter().zip(current.buckets.iter()) {
            let bound_before = before.less_than;
            let bound_after = after.less_than;
            let cumulative_before = before.count;
            let cumulative_after = after.count;
            if (bound_before - bound_after).abs() > f64::EPSILON {
                return Err(BenchmarkError::BucketBoundariesDoNotMatch(
                    bound_before,
                    bound_after,
                ));
            }
            let diff = (cumulative_after - cumulative_before).max(0.0);
            buckets_diff.push(HistogramCount {
                less_than: bound_after,
                count: diff,
            });
        }
        Ok(HistogramSnapshot {
            buckets: buckets_diff,
            count: total_diff,
            sum: current.sum - previous.sum,
        })
    }

    async fn get_scrapes(
        metrics_addresses: &[String],
    ) -> Result<Vec<(String, Scrape)>, BenchmarkError> {
        let mut scrapes = Vec::new();
        for metrics_address in metrics_addresses {
            let response = reqwest::get(metrics_address)
                .await
                .map_err(BenchmarkError::Reqwest)?;
            let metrics = response.text().await.map_err(BenchmarkError::Reqwest)?;
            let scrape = Scrape::parse(metrics.lines().map(|line| Ok(line.to_owned())))
                .map_err(BenchmarkError::IoError)?;
            scrapes.push((metrics_address.clone(), scrape));
        }
        Ok(scrapes)
    }

    fn parse_histogram(
        scrape: &Scrape,
        metric_prefix: &str,
    ) -> Result<HistogramSnapshot, BenchmarkError> {
        let mut buckets: Vec<HistogramCount> = Vec::new();
        let mut total_count: Option<f64> = None;
        let mut total_sum: Option<f64> = None;

        // Iterate over each metric in the scrape.
        for sample in &scrape.samples {
            if sample.metric == metric_prefix {
                if let Value::Histogram(histogram) = &sample.value {
                    buckets.extend(histogram.iter().cloned());
                } else {
                    return Err(BenchmarkError::ExpectedHistogramValue(sample.value.clone()));
                }
            } else if sample.metric == format!("{}_count", metric_prefix) {
                if let Value::Untyped(count) = sample.value {
                    total_count = Some(count);
                } else {
                    return Err(BenchmarkError::ExpectedUntypedValue(sample.value.clone()));
                }
            } else if sample.metric == format!("{}_sum", metric_prefix) {
                if let Value::Untyped(sum) = sample.value {
                    total_sum = Some(sum);
                } else {
                    return Err(BenchmarkError::ExpectedUntypedValue(sample.value.clone()));
                }
            }
        }

        match (total_count, total_sum) {
            (Some(count), Some(sum)) if !buckets.is_empty() => {
                buckets.sort_by(|a, b| {
                    a.less_than
                        .partial_cmp(&b.less_than)
                        .expect("Comparison should not fail")
                });
                Ok(HistogramSnapshot {
                    buckets,
                    count,
                    sum,
                })
            }
            _ => Err(BenchmarkError::IncompleteHistogramData),
        }
    }

    fn compute_quantile(
        buckets: &[HistogramCount],
        total_count: f64,
        quantile: f64,
    ) -> Result<f64, BenchmarkError> {
        if total_count == 0.0 {
            // Had no samples in the last 5s.
            return Err(BenchmarkError::NoDataYetForP99Calculation);
        }
        // Compute the target cumulative count.
        let target = (quantile * total_count).ceil();
        let mut prev_cumulative = 0.0;
        let mut prev_bound = 0.0;
        for bucket in buckets {
            if bucket.count >= target {
                let bucket_count = bucket.count - prev_cumulative;
                if bucket_count == 0.0 {
                    // Bucket that is supposed to contain the target quantile is empty, unexpectedly.
                    return Err(BenchmarkError::UnexpectedEmptyBucket);
                }
                let fraction = (target - prev_cumulative) / bucket_count;
                return Ok(prev_bound + (bucket.less_than - prev_bound) * fraction);
            }
            prev_cumulative = bucket.count;
            prev_bound = bucket.less_than;
        }
        Err(BenchmarkError::CouldNotComputeQuantile)
    }

    #[expect(clippy::too_many_arguments)]
    async fn run_benchmark_internal(
        chain_group_index: usize,
        bps: usize,
        chain_group: Vec<(Vec<Operation>, AccountOwner)>,
        chain_clients: Vec<ChainClient<Env>>,
        shutdown_notifier: CancellationToken,
        bps_count: Arc<AtomicUsize>,
        committee: Committee,
        block_time_quantiles_sender: mpsc::UnboundedSender<BlockTimings>,
        barrier: Arc<Barrier>,
        notifier: Arc<Notify>,
        runtime_control_sender: Option<mpsc::Sender<()>>,
        delay_between_chain_groups_ms: Option<u64>,
    ) -> Result<(), BenchmarkError> {
        barrier.wait().await;
        if let Some(delay_between_chain_groups_ms) = delay_between_chain_groups_ms {
            time::sleep(time::Duration::from_millis(
                (chain_group_index as u64) * delay_between_chain_groups_ms,
            ))
            .await;
        }
        info!("Starting benchmark for chain group {:?}", chain_group_index);

        if let Some(runtime_control_sender) = runtime_control_sender {
            runtime_control_sender.send(()).await?;
        }

        for ((operations, chain_owner), chain_client) in chain_group
            .into_iter()
            .zip(chain_clients.into_iter())
            .cycle()
        {
            if shutdown_notifier.is_cancelled() {
                info!("Shutdown signal received, stopping benchmark");
                break;
            }

            let block_time_start = Instant::now();
            let submit_fast_block_proposal_start = Instant::now();
            let get_pending_message_bundles_start = Instant::now();
            let incoming_bundles = chain_client.pending_message_bundles().await?;
            let get_pending_message_bundles_ms =
                get_pending_message_bundles_start.elapsed().as_millis() as u64;
            let (
                creating_proposal_ms,
                stage_block_execution_ms,
                creating_confirmed_block_ms,
                submitting_block_proposal_ms,
            ) = chain_client
                .submit_fast_block_proposal(&committee, &operations, &incoming_bundles, chain_owner)
                .await
                .map_err(BenchmarkError::ChainClient)?;
            let submit_fast_block_proposal_ms =
                submit_fast_block_proposal_start.elapsed().as_millis() as u64;
            let communicate_chain_updates_start = Instant::now();
            // We assume the committee will not change during the benchmark.
            chain_client
                .communicate_chain_updates(&committee)
                .await
                .map_err(BenchmarkError::ChainClient)?;
            let communicate_chain_updates_ms =
                communicate_chain_updates_start.elapsed().as_millis() as u64;
            let block_time_ms = block_time_start.elapsed().as_millis() as u64;
            let block_metrics = BlockTimings {
                block_time_ms,
                block_time_timings: BlockTimeTimings {
                    get_pending_message_bundles_ms,
                    submit_fast_block_proposal_ms,
                    submit_fast_block_proposal_timings: SubmitFastBlockProposalTimings {
                        creating_proposal_ms,
                        stage_block_execution_ms,
                        creating_confirmed_block_ms,
                        submitting_block_proposal_ms,
                    },
                    communicate_chain_updates_ms,
                },
            };
            if let Err(e) = block_time_quantiles_sender.send(block_metrics) {
                // The quantiles task might receive the shutdown signal first and exit before this
                // one receives it.
                warn!("Failed to send block time quantiles: {}", e);
            }

            let current_bps_count = bps_count.fetch_add(1, Ordering::Relaxed) + 1;
            if current_bps_count >= bps {
                notifier.notified().await;
            }
        }

        info!("Exiting task...");
        Ok(())
    }

    /// Closes the chain that was created for the benchmark.
    pub async fn close_benchmark_chain(
        chain_client: &ChainClient<Env>,
    ) -> Result<(), BenchmarkError> {
        let start = Instant::now();
        chain_client
            .execute_operation(Operation::system(SystemOperation::CloseChain))
            .await?
            .expect("Close chain operation should not fail!");

        debug!(
            "Closed chain {:?} in {} ms",
            chain_client.chain_id(),
            start.elapsed().as_millis()
        );

        Ok(())
    }

    /// Generates information related to one block per chain.
    pub fn make_benchmark_block_info(
        benchmark_chains: Vec<Vec<(ChainId, AccountOwner)>>,
        transactions_per_block: usize,
        fungible_application_id: Option<ApplicationId>,
    ) -> Vec<Vec<(Vec<Operation>, AccountOwner)>> {
        let mut blocks_infos = Vec::new();
        for chains in benchmark_chains {
            let mut infos = Vec::new();
            let chains_len = chains.len();
            let amount = Amount::from(1);
            for i in 0..chains_len {
                let owner = chains[i].1;
                let recipient_chain_id = chains[(i + chains_len - 1) % chains_len].0;
                let operation = match fungible_application_id {
                    Some(application_id) => Self::fungible_transfer(
                        application_id,
                        recipient_chain_id,
                        owner,
                        owner,
                        amount,
                    ),
                    None => Operation::system(SystemOperation::Transfer {
                        owner: AccountOwner::CHAIN,
                        recipient: Recipient::chain(recipient_chain_id),
                        amount,
                    }),
                };
                let operations = iter::repeat_n(operation, transactions_per_block).collect();
                infos.push((operations, owner));
            }
            blocks_infos.push(infos);
        }
        blocks_infos
    }

    /// Creates a fungible token transfer operation.
    pub fn fungible_transfer(
        application_id: ApplicationId,
        chain_id: ChainId,
        sender: AccountOwner,
        receiver: AccountOwner,
        amount: Amount,
    ) -> Operation {
        let target_account = fungible::Account {
            chain_id,
            owner: receiver,
        };
        let bytes = bcs::to_bytes(&FungibleOperation::Transfer {
            owner: sender,
            amount,
            target_account,
        })
        .expect("should serialize fungible token operation");
        Operation::User {
            application_id,
            bytes,
        }
    }
}
