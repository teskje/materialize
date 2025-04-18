// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Open-loop benchmark for measuring UPSERT performance.
//!
//! Run using
//!
//! ```
//! $ cargo run --release --example upsert_open_loop -- --runtime 5sec \
//!   --records-per-second 200000 --num-keys 200000 --batch-size 20000 \
//!   --key-value-store in-memory-hash-map \
//!   --num-async-workers 16 --num-timely-workers=16 --num-sources=16
//! ```
//!
//! ## Description
//!
//! The benchmark emulates a source-ingestion pipeline and looks like this:
//!
//! ```
//! generator(async task) ->(tokio channel) source(timely operator)
//!   ->(exchange channel) upsert(timely operator) ->(exchange channel) stats(timely operator)
//! ```
//!
//! The main part of the pipeline runs in a (configurable) timely cluster but we generate data
//! outside (in async/blocking tokio tasks) and ship it to the pipeline via tokio channels. For each
//! source, we have one generator task and we designate only one of the timely workers as the
//! _active worker_, which listens on the other end of the channel and imports data into the timely
//! cluster.
//!
//! The _upsert_ operator runs on all timely workers, so work (for example interacting with RocksDB)
//! is distributed. The _stats_ operator is again only active on one worker, we ship all the
//! updates/retractions for a given source to its active worker, where the operator prints stats
//! (throughput, processing lag, etc).
//!
//! There are three types of configuration options (plus miscellaneous options, like runtime):
//!  - _data_: These control record size, size of key space, number of records per second, batch
//!  size
//!  - scenario: These control how many threads we use for the data generators, how many timely
//!  workers we use, and how many sources we use. The _data_ options are _per source_, meaning you
//!  will get _num sources * records per second_ of throughput in total.
//!  - _key-value store_: Most important parameter, controls what backend we're testing.
//!
//!  It's important to note that you will get one instance of an _upsert_ operator on each timely
//!  worker, per source. For example, if you configure `4` workers and `4` sources, you will get
//!  `16` operators instances that each have their own RocksDB instance (if configured).
//!
//! ## Usage
//!
//! We give a basic sets of steps that you can follow. However, coming up with a good set of
//! parameters requires some finesse!
//!
//! The basic idea of the benchmark is: Dial in a set of parameters, then see if the data generators
//! and upsert operators can keep up with the workload. We regularly print throughput and
//! _processing lag_, the processing lag should stay low and not grow over time. If it does grow,
//! that means the pipeline cannot keep up with the data volume and you either need to reduce the
//! data volume or change how many timely workers you use (or both!).
//!
//! This way, you can find a sustainable upper limit for an in-memory key-value store, then switch
//! to the RocksDB store and see how far we have to reduce the data volume to keep it from falling
//! over.
//!
//! Steps:
//!  1. Decide on the basic scenario: largely, how many timely workers and how many sources. For
//!     example, pick a representative example from early customer data.
//!  2. Use the `noop` key-value store or one of the in-memory backends to figure out a ceiling for
//!     the data generators and the throughput you can sustain. The data generators will log
//!     warnings if they cannot keep up with the requested parameters. And the stats operators will
//!     print the processing lag, which should also stay low.
//!  3. Switch to RocksDB and fiddle with data parameters while keeping the shape of the ingestion
//!     pipeline constant.
//!
//! ## Notes
//!
//! Notes from @guswynn's testing, and remaining work before this
//! benchmark can be trusted:
//!
//! - Ensure that rocksb has read-your-writes, in-process, without "transactions" (docs are unclear here)
//! - Limit the size of write batches (and possibly multi-gets, based on
//!     <https://github.com/facebook/rocksdb/wiki/RocksDB-FAQ#basic-readwrite>).
//! - Ensure sst files are actually being written, from all workers.
//! - Figure out why this workload has large numbers of empty batches (???)
//! - Sort values before writing them.
//! - Add deletes.
//!
//! Additional notes:
//! - Its unclear if the single-thread-per-rocksdb-instance model is performant, or considered a
//! reasonable model.
//! - Its unclear if fsync can be entirely turned off in rocksdb. If it can, we should turn that
//! off
//! - I think shutdown is a bit iffy right now, sometimes a pthread error can happen...
//!

#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::bail;
use differential_dataflow::Hashable;
use futures::StreamExt;
use itertools::Itertools;
use mz_build_info::{BuildInfo, build_info};
use mz_orchestrator_tracing::{StaticTracingConfig, TracingCliArgs};
use mz_ore::cast::CastFrom;
use mz_ore::cli::{self, CliConfig};
use mz_ore::metrics::MetricsRegistry;
use mz_ore::task;
use mz_persist::indexed::columnar::ColumnarRecords;
use mz_timely_util::builder_async::{Event as AsyncEvent, OperatorBuilder as AsyncOperatorBuilder};
use mz_timely_util::probe::{Handle, ProbeNotify};
use timely::PartialOrder;
use timely::container::CapacityContainerBuilder;
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::operators::Operator;
use timely::dataflow::{Scope, Stream};
use timely::progress::Antichain;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::SendError;
use tracing::{Instrument, debug, error, info, info_span, trace, warn};

// TODO(aljoscha): Make workload configurable: cardinality of keyspace, hot vs. cold keys, the
// works.
#[path = "upsert_open_loop/workload.rs"]
mod workload;

const BUILD_INFO: BuildInfo = build_info!();

const MIB: u64 = 1024 * 1024;

/// Open-loop benchmark for persistence.
#[derive(Debug, Clone, clap::Parser)]
pub struct Args {
    /// Number of sources.
    #[clap(long, value_name = "W", default_value_t = 1)]
    num_sources: usize,

    /// Number of async (tokio) workers/threads.
    #[clap(long, value_name = "R", default_value_t = 1)]
    num_async_workers: usize,

    /// Number of (timely) workers/threads.
    #[clap(long, value_name = "R", default_value_t = 1)]
    num_timely_workers: usize,

    /// Runtime in a whole number of seconds
    #[clap(long, value_parser = humantime::parse_duration, value_name = "S", default_value = "60s")]
    runtime: Duration,

    /// How many records writers should emit per second, per source.
    #[clap(long, value_name = "R", default_value_t = 10000)]
    records_per_second: usize,

    /// How many unique keys we should generate, that is, the cardinality of the key space. This
    /// controls how large the upsert state will grow, at steady state.
    #[clap(long, value_name = "R", default_value_t = 1000)]
    num_keys: usize,

    /// Size of keys (goodbytes) in bytes.
    #[clap(long, value_name = "B", default_value_t = 64)]
    key_record_size_bytes: usize,

    /// Size of values (goodbytes) in bytes.
    #[clap(long, value_name = "B", default_value_t = 64)]
    value_record_size_bytes: usize,

    /// Batch size in number of records (if applicable).
    #[clap(long, env = "", value_name = "R", default_value_t = 100)]
    batch_size: usize,

    /// Duration between subsequent informational log outputs.
    #[clap(long, value_parser = humantime::parse_duration, value_name = "L", default_value = "1s")]
    logging_granularity: Duration,

    /// The address of the internal HTTP server.
    #[clap(long, value_name = "HOST:PORT", default_value = "127.0.0.1:6878")]
    internal_http_listen_addr: SocketAddr,

    /// Path of a file to write metrics at the end of the run.
    #[clap(long)]
    metrics_file: Option<String>,

    #[clap(flatten)]
    tracing: TracingCliArgs,

    /// The type of key-value store to use.
    #[clap(value_enum, long, default_value_t = KeyValueStore::Noop)]
    key_value_store: KeyValueStore,

    /// Wether to buffer (and reduce) batches of records in the UPSERT operator. The materialize
    /// production UPSERT operator does this, and it hammers the state backend less.
    #[clap(long)]
    upsert_pre_reduce: bool,

    /// Whether or not to use the WAL in rocksdb.
    #[clap(long)]
    rocksdb_use_wal: bool,

    /// Whether or not to use a global `Env` in rocksdb.
    ///
    /// On a laptop, this appears to have very little
    /// effect.
    #[clap(long)]
    rocksdb_global_env: bool,

    /// Whether or not to cleanup the rocksdb instances
    /// in the temporary directory.
    #[clap(long)]
    rocksdb_no_cleanup: bool,

    /// Whether or not to use the rocksdb `Vector`
    /// memtable. Currently this seems to use
    /// so much cpu that the data generators
    /// fall behind.
    #[clap(long)]
    rocksdb_use_vector_memtable: bool,
    /*
    /// Use rocksdb transactional db
    #[clap(long)]
    use_rocksb_transactions: bool,
    */
    /// What directory to place the rocksdb instances.
    /// Default is a tempdir.
    #[clap(long)]
    rocksdb_instance_dir: Option<PathBuf>,

    /// Whether or not to cleanup the rocksdb instances
    /// before using them.
    #[clap(long)]
    rocksdb_clear_before_use: bool,

    /// Print some additional stats from rocksdb, periodically.
    /// Note that `LOG` in the rocksdb dirs have more information.
    #[clap(long)]
    rocksdb_print_stats: bool,
}

/// Different key-value stores under examination.
#[derive(clap::ValueEnum, Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
enum KeyValueStore {
    /// Pass data straight through, without running any upsert logic.
    Noop,
    /// Use an in-memory [`HashMap`] as the key-value store.
    InMemoryHashMap,
    /// Use an in-memory [`BTreeMap`] as the key-value store.
    InMemoryBTreeMap,
    /// Use an on-disk RocksDB as the key-value store.
    RocksDB,
}

impl std::fmt::Display for KeyValueStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyValueStore::Noop => write!(f, "noop"),
            KeyValueStore::InMemoryHashMap => write!(f, "hash-map"),
            KeyValueStore::InMemoryBTreeMap => write!(f, "btree-map"),
            KeyValueStore::RocksDB => write!(f, "rocksdb"),
        }
    }
}

fn main() {
    let args: Args = cli::parse_args(CliConfig::default());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.num_async_workers)
        .enable_all()
        .build()
        .expect("Failed building the Runtime");

    let _ = runtime
        .block_on(args.tracing.configure_tracing(
            StaticTracingConfig {
                service_name: "upsert-open-loop",
                build_info: BUILD_INFO,
            },
            MetricsRegistry::new(),
        ))
        .expect("failed to init tracing");

    let root_span = info_span!("upsert_open_loop");
    let res = runtime.block_on(run(args).instrument(root_span));

    if let Err(err) = res {
        eprintln!("error: {:#}", err);
        std::process::exit(1);
    }
}

pub async fn run(args: Args) -> Result<(), anyhow::Error> {
    let metrics_registry = MetricsRegistry::new();
    {
        let metrics_registry = metrics_registry.clone();
        info!(
            "serving internal HTTP server on http://{}/metrics",
            args.internal_http_listen_addr
        );

        let listener = TcpListener::bind(&args.internal_http_listen_addr)
            .await
            .expect("can bind");
        mz_ore::task::spawn(
            || "http_server",
            axum::serve(
                listener,
                axum::Router::new()
                    .route(
                        "/metrics",
                        axum::routing::get(move || async move {
                            mz_http_util::handle_prometheus(&metrics_registry).await
                        }),
                    )
                    .into_make_service(),
            )
            .into_future(),
        );
    }

    let num_sources = args.num_sources;
    let num_workers = args.num_timely_workers;
    run_benchmark(args, metrics_registry, num_sources, num_workers).await
}

async fn run_benchmark(
    args: Args,
    _metrics_registry: MetricsRegistry,
    num_sources: usize,
    num_workers: usize,
) -> Result<(), anyhow::Error> {
    let num_records_total = args.records_per_second * usize::cast_from(args.runtime.as_secs());
    let data_generator = workload::DataGenerator::new_with_key_cardinality(
        args.num_keys,
        num_records_total,
        args.key_record_size_bytes,
        args.value_record_size_bytes,
        args.batch_size,
    );

    let benchmark_description = format!(
        "key-value-backend={} upsert-pre-reduce={} num-sources={} num-async-workers={} \
        num-timely-workers={} runtime={:?} num_records_total={} records-per-second={} \
        num-keys={} key-record-size-bytes={} value-record-size-bytes={} batch-size={}",
        args.key_value_store,
        args.upsert_pre_reduce,
        args.num_sources,
        args.num_async_workers,
        args.num_timely_workers,
        args.runtime,
        num_records_total,
        args.records_per_second,
        args.num_keys,
        args.key_record_size_bytes,
        args.value_record_size_bytes,
        args.batch_size
    );

    info!("starting benchmark: {}", benchmark_description);

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_tx = Arc::new(Mutex::new(Some(progress_tx)));

    let mut generator_handles: Vec<task::JoinHandle<Result<String, anyhow::Error>>> = vec![];
    let mut source_rxs = BTreeMap::new();

    // All workers should have the starting time (so they can consistently track progress
    // and reason about lag independently).
    let start = Instant::now();

    // This controls the time that data generators (and in turn sources) downgrade to. We know this,
    // and probe the time at the end of the pipeline to figure out the lag.
    let (shared_source_time_tx, shared_source_time_rx) = tokio::sync::watch::channel(0u64);

    // The batch interarrival time. We'll use this quantity to rate limit the
    // data generation.
    // No other known way to convert `usize` to `f64`.
    #[allow(clippy::as_conversions)]
    let time_per_batch = {
        let records_per_second_f64 = args.records_per_second as f64;
        let batch_size_f64 = args.batch_size as f64;

        let batches_per_second = records_per_second_f64 / batch_size_f64;
        Duration::from_secs(1).div_f64(batches_per_second)
    };

    for source_id in 0..num_sources {
        let data_generator = data_generator.clone();
        let start = start.clone();

        let (generator_tx, generator_rx) = tokio::sync::mpsc::unbounded_channel();
        source_rxs.insert(source_id, generator_rx);

        let mut shared_source_time_rx = shared_source_time_rx.clone();

        // Intentionally create the span outside the task to set the parent.
        let generator_span = info_span!("generator", source_id);
        let data_generator_handle = mz_ore::task::spawn(
            || format!("data-generator-{}", source_id),
            async move {
                info!("starting data generator {}", source_id);

                // The number of batches this data generator has sent over to the
                // corresponding writer task.
                let mut batch_idx = 0;
                // The last time we emitted progress information to stdout, expressed
                // as a relative duration from start.
                let mut prev_log = Duration::from_millis(0);

                let mut current_source_time = shared_source_time_rx.borrow().clone();


                // Sleep so this doesn't busy wait if it's ahead of schedule.
                let mut elapsed = start.elapsed();
                let mut next_batch_time = time_per_batch * (batch_idx);

                // We make sure that we downgrade the timestamp when there's a new one,
                // otherwise we wait, so that we're trottled.
                loop {
                    if elapsed.as_millis() > next_batch_time.as_millis() {
                        warn!(next_batch_time = ?next_batch_time, elapsed = ?elapsed, "data generator for source {source_id} is not keeping up!");
                    }

                    let sleep = next_batch_time.saturating_sub(elapsed);
                    debug!(next_batch_time = ?next_batch_time, elapsed = ?elapsed, "Data generator ahead of schedule, sleeping for {:?}", sleep);

                    // Data generation can be CPU expensive, so generate it
                    // in a spawn_blocking to play nicely with the rest of
                    // the async code.
                    let mut data_generator_cloned = data_generator.clone();

                    tokio::select! {
                        _res = tokio::time::sleep(sleep) => {

                            // Intentionally create the span outside the task to set the
                            // parent.
                            let batch_span = info_span!("batch", batch_idx);
                            let batch = mz_ore::task::spawn_blocking(
                                || "data_generator-batch",
                                move || {
                                    batch_span
                                        .in_scope(|| data_generator_cloned.gen_batch(usize::cast_from(batch_idx)))
                                },
                            )
                            .await
                            .expect("task failed");
                            trace!("data generator {} made a batch", source_id);
                            let batch = match batch {
                                Some(x) => x,
                                None => {
                                    let records_sent = usize::cast_from(batch_idx) * args.batch_size;
                                    let finished = format!(
                                        "Data generator {} finished after {} ms and sent {} records",
                                        source_id,
                                        start.elapsed().as_millis(),
                                        records_sent
                                    );
                                    return Ok(finished);
                                }
                            };
                            batch_idx += 1;

                            // send will only error if the matching receiver has been dropped.
                            if let Err(SendError(_)) = generator_tx.send(GeneratorEvent::Data(batch)) {
                                bail!("receiver unexpectedly dropped");
                            }
                            debug!("data generator {} wrote a batch", source_id);

                            next_batch_time = time_per_batch * (batch_idx);
                        }
                        Ok(()) = shared_source_time_rx.changed() => {
                            let new_source_time = shared_source_time_rx.borrow().clone();
                            assert!(new_source_time > current_source_time);
                            current_source_time = new_source_time;
                            if let Err(SendError(_)) =
                                generator_tx.send(GeneratorEvent::Progress(current_source_time))
                            {
                                bail!("receiver unexpectedly dropped");
                            }
                            debug!("data generator {source_id} downgraded to {current_source_time}");
                        }
                    }

                    elapsed = start.elapsed();

                    if elapsed - prev_log > args.logging_granularity {
                        let records_sent = usize::cast_from(batch_idx) * args.batch_size;
                        debug!(
                            "After {} ms data generator {} has sent {} records.",
                            start.elapsed().as_millis(),
                            source_id,
                            records_sent
                        );
                        prev_log = elapsed;
                    }
                }

            }
            .instrument(generator_span),
        );

        generator_handles.push(data_generator_handle);
    }

    let source_rxs = Arc::new(Mutex::new(source_rxs));

    let (dir_path, temp_dir_to_drop) = if let Some(rocks_dir) = &args.rocksdb_instance_dir {
        (rocks_dir.clone(), None)
    } else {
        let rocks_dir = tempfile::tempdir().unwrap();
        let path = rocks_dir.path().to_owned();
        (path, Some(rocks_dir))
    };
    info!(
        "RocksDB instances will be hosted at: {}",
        dir_path.display()
    );

    if args.rocksdb_no_cleanup {
        std::mem::forget(temp_dir_to_drop);
    }

    let global_rocks_env = rocksdb::Env::new().unwrap();
    let mut rocksdb_options = vec![];
    for _ in 0..num_sources {
        // One rocksdb options object so we can share stats.
        let mut rocks_options = rocksdb::Options::default();
        rocks_options.create_if_missing(true);
        // Dumped every 600 seconds.
        rocks_options.enable_statistics();
        rocks_options.set_report_bg_io_stats(true);
        rocks_options.set_compression_type(rocksdb::DBCompressionType::None);
        rocks_options.set_blob_compression_type(rocksdb::DBCompressionType::None);

        if args.rocksdb_use_vector_memtable {
            rocks_options.set_memtable_factory(rocksdb::MemtableFactory::Vector);
            // Required to use the vector memtable.
            rocks_options.set_allow_concurrent_memtable_write(false);
        }
        if args.rocksdb_global_env {
            rocks_options.set_env(&global_rocks_env);
        }

        rocksdb_options.push(Arc::new(rocks_options))
    }

    let timely_config = timely::Config::process(num_workers);
    let args_cloned = args.clone();
    let timely_guards = timely::execute::execute(timely_config, move |timely_worker| {
        let progress_tx = Arc::clone(&progress_tx);
        let source_rxs = Arc::clone(&source_rxs);

        let dir_path = dir_path.clone();
        let args = args_cloned.clone();
        let rocksdb_options = rocksdb_options.clone();

        let probe = timely_worker.dataflow::<u64, _, _>(move |scope| {
            let mut source_streams = Vec::new();

            for (source_id, rocks_options) in (0..num_sources).zip(rocksdb_options.iter()) {
                let source_rxs = Arc::clone(&source_rxs);

                let source_stream = generator_source(scope, source_id, source_rxs);

                let upsert_stream = upsert(
                    scope,
                    &source_stream,
                    source_id,
                    &dir_path,
                    args.clone(),
                    rocks_options,
                );

                // Choose a different worker for the counting.
                // TODO(aljoscha): Factor out into function.
                let worker_id = scope.index();
                let worker_count = scope.peers();
                let chosen_worker =
                    usize::cast_from((source_id + 1).hashed() % u64::cast_from(worker_count));
                let active_worker = chosen_worker == worker_id;

                let rocks_options = Arc::clone(rocks_options);
                let mut num_additions = 0;
                let mut num_retractions = 0;

                let mut frontier = Antichain::from_elem(0);
                let mut max_lag = 0;
                upsert_stream.sink(
                    Exchange::new(move |_| u64::cast_from(chosen_worker)),
                    &format!("source-{source_id}-counter"),
                    move |input| {
                        if !active_worker {
                            return;
                        }
                        input.for_each(|_time, data| {
                            for (_k, _v, diff) in data.drain(..) {
                                if diff == 1 {
                                    num_additions += 1;
                                } else if diff == -1 {
                                    num_retractions += 1;
                                } else {
                                    panic!("unexpected record diff: {diff}");
                                }
                            }
                        });

                        if input.frontier().is_empty() {
                            assert_eq!(num_records_total, num_additions);
                            info!(
                                "Processing source {source_id} finished \
                                    after {} ms and processed {num_additions} additions and \
                                    {num_retractions} retractions",
                                start.elapsed().as_millis(),
                            );
                        } else if PartialOrder::less_than(
                            &frontier.borrow(),
                            &input.frontier().frontier(),
                        ) {
                            frontier = input.frontier().frontier().to_owned();
                            let data_timestamp = frontier.clone().into_option().unwrap();
                            let elapsed = start.elapsed();

                            #[allow(clippy::as_conversions)]
                            {
                                let lag = elapsed.as_millis() as u64 - data_timestamp;
                                max_lag = std::cmp::max(max_lag, lag);
                                let elapsed_seconds = elapsed.as_secs();
                                let key_mb_read = (num_additions * args.key_record_size_bytes)
                                    as f64
                                    / MIB as f64;
                                let mb_read = (num_additions
                                    * (args.key_record_size_bytes + args.value_record_size_bytes))
                                    as f64
                                    / MIB as f64;
                                let key_throughput = key_mb_read / elapsed_seconds as f64;
                                let throughput = mb_read / elapsed_seconds as f64;

                                let rocksdb_stats = if args.rocksdb_print_stats {
                                    calculate_rocksdb_stats(Some(&rocks_options), elapsed_seconds)
                                        .unwrap_or_else(String::new)
                                } else {
                                    "".to_string()
                                };

                                info!(
                                    "After {} ms, source {source_id} has read {num_additions} \
                                    records (throughput {:.3} MiB/s, key throughput {:.3} MiB/s). \
                                    Max processing lag {max_lag}ms, \
                                    most recent processing lag {lag}ms.{}",
                                    elapsed.as_millis(),
                                    throughput,
                                    key_throughput,
                                    rocksdb_stats
                                );
                            }
                        }
                    },
                );

                source_streams.push(upsert_stream);
            }

            let probe = Handle::default();

            for source_stream in source_streams {
                source_stream.probe_notify_with(vec![probe.clone()]);
            }

            let worker_id = scope.index();

            let active_worker = 0 == worker_id;

            let progress_op =
                AsyncOperatorBuilder::new("progress-bridge".to_string(), scope.clone());

            let probe_clone = probe.clone();
            let _shutdown_button = progress_op.build(move |_capabilities| async move {
                if !active_worker {
                    return;
                }

                let progress_tx = progress_tx
                    .lock()
                    .expect("lock poisoned")
                    .take()
                    .expect("someone took our progress_tx");

                loop {
                    let _progressed = probe_clone.progressed().await;
                    let mut frontier = Antichain::new();
                    probe_clone.with_frontier(|new_frontier| {
                        frontier.clone_from(&new_frontier.to_owned())
                    });
                    if !frontier.is_empty() {
                        let progress_ts = frontier.into_option().unwrap();
                        if let Err(SendError(_)) = progress_tx.send(progress_ts) {
                            return;
                        }
                    } else {
                        // We're done!
                        return;
                    }
                }
            });

            probe
        });

        // Step until our sources shut down.
        while probe.less_than(&u64::MAX) {
            timely_worker.step_or_park(None);
        }
    })
    .unwrap();

    let start_clone = start.clone();
    task::spawn(|| "lag-observer", async move {
        while let Some(observed_time) = progress_rx.recv().await {
            // TODO(aljoscha): The lag here also depends on when the generator task downgrades the
            // time, which it only does _after_ it creates a new batch. We have to change it to
            // immediately downgrade when we change the shared source timestamp.
            // TODO(aljoscha): Make the output similar to the persist open-loop benchmark, where we
            // output throughput, num records, etc.
            let now: u64 = start_clone.elapsed().as_millis().try_into().unwrap();
            let diff = now - observed_time;
            info!("global processing lag: {diff}ms");
        }
        trace!("progress channel closed!");
    });

    let mut current_time = 0u64;
    drop(shared_source_time_rx);
    loop {
        let new_time: u64 = start.elapsed().as_millis().try_into().unwrap();

        if new_time > current_time + 1000 {
            current_time = new_time;
            if let Err(_err) = shared_source_time_tx.send(new_time) {
                // All source generators finished!
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for handle in generator_handles {
        match handle.await? {
            Ok(finished) => info!("{}", finished),
            Err(e) => error!("error: {:?}", e),
        }
    }

    for g in timely_guards.join() {
        g.expect("timely process failed");
    }

    Ok(())
}

enum GeneratorEvent {
    Progress(u64),
    Data(ColumnarRecords),
}

/// A source that reads from it's unbounded channel and returns a `Stream` of the contained records.
///
/// Only one worker is expected to read from the channel that the associated generator is writing
/// to.
fn generator_source<G>(
    scope: &G,
    source_id: usize,
    generator_rxs: Arc<Mutex<BTreeMap<usize, UnboundedReceiver<GeneratorEvent>>>>,
) -> Stream<G, (Vec<u8>, Vec<u8>)>
where
    G: Scope<Timestamp = u64>,
{
    let generator_rxs = Arc::clone(&generator_rxs);

    let scope = scope.clone();
    let worker_id = scope.index();
    let worker_count = scope.peers();

    let chosen_worker = usize::cast_from(source_id.hashed() % u64::cast_from(worker_count));
    let active_worker = chosen_worker == worker_id;

    let mut source_op = AsyncOperatorBuilder::new(format!("source-{source_id}"), scope);

    let (output, output_stream) = source_op.new_output::<CapacityContainerBuilder<_>>();

    let _shutdown_button = source_op.build(move |mut capabilities| async move {
        if !active_worker {
            return;
        }

        let mut cap = capabilities.pop().expect("missing capability");

        let mut generator_rx = {
            let mut generator_rxs = generator_rxs.lock().expect("lock poisoned");
            generator_rxs
                .remove(&source_id)
                .expect("someone else took our channel")
        };

        while let Some(event) = generator_rx.recv().await {
            match event {
                GeneratorEvent::Progress(ts) => {
                    trace!("source {source_id}, progress: {ts}");
                    cap.downgrade(&ts);
                }
                GeneratorEvent::Data(batch) => {
                    let mut batch = batch
                        .iter()
                        .map(|((key, value), _ts, _diff)| (key.to_vec(), value.to_vec()))
                        .collect_vec();

                    output.give_container(&cap, &mut batch);
                }
            }
        }
    });

    output_stream
}

/// A representative upsert operator.
fn upsert<G>(
    scope: &G,
    source_stream: &Stream<G, (Vec<u8>, Vec<u8>)>,
    source_id: usize,
    instance_dir: &PathBuf,
    args: Args,
    rocksdb_options: &rocksdb::Options,
) -> Stream<G, (Vec<u8>, Vec<u8>, i32)>
where
    G: Scope<Timestamp = u64>,
{
    // TODO(aljoscha): Not liking this duplications...!
    if args.upsert_pre_reduce {
        match args.key_value_store {
            KeyValueStore::Noop => upsert_core_pre_reduce(scope, source_stream, source_id, NoopMap),
            KeyValueStore::InMemoryHashMap =>
            {
                #[allow(clippy::disallowed_types)]
                upsert_core_pre_reduce(
                    scope,
                    source_stream,
                    source_id,
                    std::collections::HashMap::new(),
                )
            }
            KeyValueStore::InMemoryBTreeMap => {
                upsert_core_pre_reduce(scope, source_stream, source_id, BTreeMap::new())
            }
            KeyValueStore::RocksDB => {
                let rocksdb = IoThreadRocksDB::new(
                    instance_dir,
                    rocksdb_options,
                    scope.index(),
                    source_id,
                    args.rocksdb_use_wal,
                    args.rocksdb_clear_before_use,
                );

                upsert_core_pre_reduce(scope, source_stream, source_id, rocksdb)
            }
        }
    } else {
        match args.key_value_store {
            KeyValueStore::Noop => upsert_core(scope, source_stream, source_id, NoopMap),
            KeyValueStore::InMemoryHashMap =>
            {
                #[allow(clippy::disallowed_types)]
                upsert_core(
                    scope,
                    source_stream,
                    source_id,
                    std::collections::HashMap::new(),
                )
            }
            KeyValueStore::InMemoryBTreeMap => {
                upsert_core(scope, source_stream, source_id, BTreeMap::new())
            }
            KeyValueStore::RocksDB => {
                let rocksdb = IoThreadRocksDB::new(
                    instance_dir,
                    rocksdb_options,
                    scope.index(),
                    source_id,
                    args.rocksdb_use_wal,
                    args.rocksdb_clear_before_use,
                );

                upsert_core(scope, source_stream, source_id, rocksdb)
            }
        }
    }
}

fn upsert_core_pre_reduce<G, M: Map + 'static>(
    scope: &G,
    source_stream: &Stream<G, (Vec<u8>, Vec<u8>)>,
    source_id: usize,
    mut current_values: M,
) -> Stream<G, (Vec<u8>, Vec<u8>, i32)>
where
    G: Scope<Timestamp = u64>,
{
    let mut upsert_op =
        AsyncOperatorBuilder::new(format!("source-{source_id}-upsert"), scope.clone());

    let (output, output_stream): (_, Stream<_, (Vec<u8>, Vec<u8>, i32)>) = upsert_op.new_output();
    let mut input = upsert_op.new_input_for(
        source_stream,
        Exchange::new(|d: &(Vec<u8>, Vec<u8>)| d.0.hashed()),
        &output,
    );

    upsert_op.build(move |capabilities| async move {
        drop(capabilities);

        // Just use a basic nested-map like the old upsert implementation to buffer values by time.
        let mut pending_values: BTreeMap<u64, (_, BTreeMap<_, _>)> = BTreeMap::new();

        let mut frontier = Antichain::from_elem(0);
        while let Some(event) = input.next().await {
            match event {
                AsyncEvent::Data(cap, buffer) => {
                    for (k, v) in buffer {
                        let time = *cap.time();
                        let map = &mut pending_values
                            .entry(time)
                            .or_insert_with(|| (cap.delayed(cap.time()), BTreeMap::new()))
                            .1;
                        // In real upsert we sort by offset, but here we just
                        // choose the latest one.
                        map.insert(k, v);
                    }
                }
                AsyncEvent::Progress(new_frontier) => frontier = new_frontier,
            }
            let mut removed_times = Vec::new();

            // TODO(guswynn): All this code is pretty gross, but I couldn't figure out a better
            // way to share `&mut Capability`s and also provide the `Map` implementations the
            // largest batches possible.
            let mut batches = Vec::new();
            let mut caps = Vec::new();
            for (time, (cap, map)) in pending_values.iter_mut() {
                if frontier.less_equal(time) {
                    break;
                }
                let len = map.len();
                batches.push(std::mem::take(map).into_iter().collect());
                caps.push((cap, len));

                removed_times.push(*time)
            }

            // `caps` holds references to `Capability`s, and also the number of values
            // in the mega-batches they correspond too.
            let mut cap_iter = caps.into_iter();
            let mut cur_cap = None;
            for (k, v, previous_v) in current_values.ingest(batches).await {
                let cap = match &mut cur_cap {
                    None => {
                        let stuff = cur_cap.insert(cap_iter.next().unwrap());
                        stuff.1 -= 1;
                        &mut stuff.0
                    }
                    Some((_cap, num)) if *num == 0 => {
                        &mut cur_cap.insert(cap_iter.next().unwrap()).0
                    }
                    Some((cap, num)) => {
                        *num -= 1;
                        cap
                    }
                };
                if let Some(previous_v) = previous_v {
                    // we might be able to avoid this extra key clone here,
                    // if we really tried
                    output.give(*cap, (k.clone(), previous_v, -1));
                }
                // we don't do deletes right now
                output.give(*cap, (k, v, 1));
            }

            // Discard entries, capabilities for complete times.
            for time in removed_times {
                pending_values.remove(&time);
            }
        }
    });

    output_stream
}

fn upsert_core<G, M: Map + 'static>(
    scope: &G,
    source_stream: &Stream<G, (Vec<u8>, Vec<u8>)>,
    source_id: usize,
    mut current_values: M,
) -> Stream<G, (Vec<u8>, Vec<u8>, i32)>
where
    G: Scope<Timestamp = u64>,
{
    let mut upsert_op =
        AsyncOperatorBuilder::new(format!("source-{source_id}-upsert"), scope.clone());

    let (output, output_stream): (_, Stream<_, (Vec<u8>, Vec<u8>, i32)>) = upsert_op.new_output();
    let mut input = upsert_op.new_input_for(
        source_stream,
        Exchange::new(|d: &(Vec<u8>, Vec<u8>)| d.0.hashed()),
        &output,
    );

    upsert_op.build(move |capabilities| async move {
        drop(capabilities);

        while let Some(event) = input.next().await {
            match event {
                AsyncEvent::Data(cap, mut buffer) => {
                    let mut batch = Vec::new();
                    batch.append(&mut buffer);

                    for (k, v, previous_v) in current_values.ingest(vec![batch]).await {
                        if let Some(previous_v) = previous_v {
                            // we might be able to avoid this extra key clone here,
                            // if we really tried
                            output.give(&cap, (k.clone(), previous_v, -1));
                        }
                        // we don't do deletes right now
                        output.give(&cap, (k, v, 1));
                    }
                }
                AsyncEvent::Progress(_new_frontier) => (),
            }
        }
    });

    output_stream
}

type KV = (Vec<u8>, Vec<u8>);
type KVAndPrevious = (Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type Batch<T = KV> = Vec<T>;

/// A "map" we can ingest data into.
#[async_trait::async_trait]
trait Map {
    /// Ingest the batches of `KV`s. Return the `KV`s back in a single
    /// allocation, along with that key's previous value, if it existed.
    ///
    /// The input and output use different nesting schemes
    /// (nested batches vs flat) to reduce additional allocations in
    /// the `upsert` operator.
    async fn ingest(&mut self, batches: Vec<Batch>) -> Batch<KVAndPrevious>;
}

/// A [`Map`] that simply passes through values, emitting the new value as both new and current, to
/// keep the number of emitted updates roughly similar to the other implementations which run real
/// UPSERT logic.
struct NoopMap;

#[async_trait::async_trait]
impl Map for NoopMap {
    async fn ingest(&mut self, batches: Vec<Batch>) -> Batch<KVAndPrevious> {
        let mut out = Vec::new();
        for batch in batches {
            for (k, v) in batch {
                // TODO(guswynn): reduce clones.
                // NOTE: Keep similar number of clones to HashMap and BTreeMap impl.
                let _in_k = k.clone();
                let in_v = v.clone();
                out.push((k, v, Some(in_v)))
            }
        }
        out
    }
}

#[async_trait::async_trait]
#[allow(clippy::disallowed_types)]
impl Map for std::collections::HashMap<Vec<u8>, Vec<u8>> {
    async fn ingest(&mut self, batches: Vec<Batch>) -> Batch<KVAndPrevious> {
        let mut out = Vec::new();
        for batch in batches {
            for (k, v) in batch {
                // TODO(guswynn): reduce clones.
                let in_k = k.clone();
                let in_v = v.clone();
                out.push((k, v, self.insert(in_k, in_v)))
            }
        }
        out
    }
}

#[async_trait::async_trait]
impl Map for BTreeMap<Vec<u8>, Vec<u8>> {
    async fn ingest(&mut self, batches: Vec<Batch>) -> Batch<KVAndPrevious> {
        let mut out = Vec::new();
        for batch in batches {
            for (k, v) in batch {
                // TODO(guswynn): reduce clones.
                let in_k = k.clone();
                let in_v = v.clone();
                out.push((k, v, self.insert(in_k, in_v)))
            }
        }
        out
    }
}

use std::path::{Path, PathBuf};

use rocksdb::{DB, Error};
use tokio::sync::oneshot::{Sender, channel};

#[derive(Clone)]
struct IoThreadRocksDB {
    tx: crossbeam_channel::Sender<(Vec<Batch>, Sender<Result<Batch<KVAndPrevious>, Error>>)>,
}

impl IoThreadRocksDB {
    fn new(
        temp_dir: &Path,
        options: &rocksdb::Options,
        worker_id: usize,
        source_id: usize,
        use_wal: bool,
        destroy_before_use: bool,
    ) -> Self {
        // bounded??
        let (tx, rx): (
            _,
            crossbeam_channel::Receiver<(Vec<Batch>, Sender<Result<Batch<KVAndPrevious>, Error>>)>,
        ) = crossbeam_channel::unbounded();

        let instance_path = temp_dir
            .join(format!("worker_id:{}", worker_id))
            .join(format!("source_id:{}", source_id));

        if destroy_before_use && instance_path.exists() {
            DB::destroy(&rocksdb::Options::default(), &*instance_path).unwrap();
        }

        let db: DB = DB::open(options, instance_path).unwrap();
        std::thread::spawn(move || {
            let mut wo = rocksdb::WriteOptions::new();
            wo.disable_wal(!use_wal);

            'batch: while let Ok((batches, resp)) = rx.recv() {
                let size: usize = batches.iter().map(|b| b.len()).sum::<usize>();

                // TODO(guswynn): this should probably be lifted into the upsert operator.
                if size == 0 {
                    let _ = resp.send(Ok(Vec::new()));
                    continue;
                }

                let gets = db.multi_get(
                    batches
                        .iter()
                        .flat_map(|b| b.iter().map(|(k, _)| k.as_slice())),
                );

                let mut previous = Vec::new();
                let mut writes = rocksdb::WriteBatch::default();

                // TODO(guswynn): sort by key before writing.
                for ((k, v), get) in batches.into_iter().flat_map(|b| b.into_iter()).zip(gets) {
                    writes.put(k.as_slice(), v.as_slice());

                    match get {
                        Ok(prev) => {
                            previous.push((k, v, prev));
                        }
                        Err(e) => {
                            // Give up on the batch on errors.
                            let _ = resp.send(Err(e));
                            continue 'batch;
                        }
                    }
                }
                match db.write_opt(writes, &wo) {
                    Ok(()) => {}
                    Err(e) => {
                        // Give up on the batch on errors.
                        let _ = resp.send(Err(e));
                        continue 'batch;
                    }
                }
                debug!(
                    "finished writing batch size({size}) for worker {worker_id}, source {source_id}"
                );

                let _ = resp.send(Ok(previous));
            }
        });

        Self { tx }
    }

    async fn ingest_inner(&self, batches: Vec<Batch>) -> Batch<KVAndPrevious> {
        let (tx, rx) = channel();

        // We assume the rocksdb thread doesnt shutdown before timely
        self.tx.send((batches, tx)).unwrap();

        // We also unwrap all rocksdb errors here.
        rx.await.unwrap().unwrap()
    }
}

#[async_trait::async_trait]
impl Map for IoThreadRocksDB {
    async fn ingest(&mut self, batches: Vec<Batch>) -> Batch<KVAndPrevious> {
        self.ingest_inner(batches).await
    }
}

#[allow(clippy::as_conversions)]
fn calculate_rocksdb_stats(
    opts: Option<&rocksdb::Options>,
    elapsed_seconds: u64,
) -> Option<String> {
    let mut read_rate: f64 = -1.0;
    let mut write_rate: f64 = -1.0;
    let mut compact_read_rate: f64 = -1.0;
    let mut compact_write_rate: f64 = -1.0;
    if let Some(opts) = opts {
        if let Some(stats) = opts.get_statistics() {
            // If the given variable is not there, the rate ends up being
            // `-1`, to communicate to the user that something is wrong.
            for line in stats.split('\n') {
                for (stat, rate_var) in [
                    ("rocksdb.number.multiget.bytes.read", &mut read_rate),
                    ("rocksdb.bytes.written", &mut write_rate),
                    ("rocksdb.compact.read.bytes", &mut compact_read_rate),
                    ("rocksdb.compact.write.bytes", &mut compact_write_rate),
                ] {
                    if line.starts_with(stat) {
                        let val: isize = line
                            .strip_prefix(&format!("{stat} COUNT : "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        *rate_var = ((val) as f64 / MIB as f64) / elapsed_seconds as f64;
                    }
                }
            }

            return Some(format!(
                "\n\tRocksDB read throughput {read_rate:.3} MiB/s\n\
                \tRocksDB write throughput {write_rate:.3} MiB/s\n\
                \tRocksDB compact read throughput {compact_read_rate:.3} MiB/s\n\
                \tRocksDB compact write throughput {compact_write_rate:.3} MiB/s",
            ));
        }
    }
    None
}
