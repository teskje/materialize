// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Renders a plan into a timely/differential dataflow computation.
//!
//! ## Error handling
//!
//! Timely and differential have no idioms for computations that can error. The
//! philosophy is, reasonably, to define the semantics of the computation such
//! that errors are unnecessary: e.g., by using wrap-around semantics for
//! integer overflow.
//!
//! Unfortunately, SQL semantics are not nearly so elegant, and require errors
//! in myriad cases. The classic example is a division by zero, but invalid
//! input for casts, overflowing integer operations, and dozens of other
//! functions need the ability to produce errors ar runtime.
//!
//! At the moment, only *scalar* expression evaluation can fail, so only
//! operators that evaluate scalar expressions can fail. At the time of writing,
//! that includes map, filter, reduce, and join operators. Constants are a bit
//! of a special case: they can be either a constant vector of rows *or* a
//! constant, singular error.
//!
//! The approach taken is to build two parallel trees of computation: one for
//! the rows that have been successfully evaluated (the "oks tree"), and one for
//! the errors that have been generated (the "errs tree"). For example:
//!
//! ```text
//!    oks1  errs1       oks2  errs2
//!      |     |           |     |
//!      |     |           |     |
//!   project  |           |     |
//!      |     |           |     |
//!      |     |           |     |
//!     map    |           |     |
//!      |\    |           |     |
//!      | \   |           |     |
//!      |  \  |           |     |
//!      |   \ |           |     |
//!      |    \|           |     |
//!   project  +           +     +
//!      |     |          /     /
//!      |     |         /     /
//!    join ------------+     /
//!      |     |             /
//!      |     | +----------+
//!      |     |/
//!     oks   errs
//! ```
//!
//! The project operation cannot fail, so errors from errs1 are propagated
//! directly. Map operators are fallible and so can inject additional errors
//! into the stream. Join operators combine the errors from each of their
//! inputs.
//!
//! The semantics of the error stream are minimal. From the perspective of SQL,
//! a dataflow is considered to be in an error state if there is at least one
//! element in the final errs collection. The error value returned to the user
//! is selected arbitrarily; SQL only makes provisions to return one error to
//! the user at a time. There are plans to make the err collection accessible to
//! end users, so they can see all errors at once.
//!
//! To make errors transient, simply ensure that the operator can retract any
//! produced errors when corrected data arrives. To make errors permanent, write
//! the operator such that it never retracts the errors it produced. Future work
//! will likely want to introduce some sort of sort order for errors, so that
//! permanent errors are returned to the user ahead of transient errors—probably
//! by introducing a new error type a la:
//!
//! ```no_run
//! # struct EvalError;
//! # struct SourceError;
//! enum DataflowError {
//!     Transient(EvalError),
//!     Permanent(SourceError),
//! }
//! ```
//!
//! If the error stream is empty, the oks stream must be correct. If the error
//! stream is non-empty, then there are no semantics for the oks stream. This is
//! sufficient to support SQL in its current form, but is likely to be
//! unsatisfactory long term. We suspect that we can continue to imbue the oks
//! stream with semantics if we are very careful in describing what data should
//! and should not be produced upon encountering an error. Roughly speaking, the
//! oks stream could represent the correct result of the computation where all
//! rows that caused an error have been pruned from the stream. There are
//! strange and confusing questions here around foreign keys, though: what if
//! the optimizer proves that a particular key must exist in a collection, but
//! the key gets pruned away because its row participated in a scalar expression
//! evaluation that errored?
//!
//! In the meantime, it is probably wise for operators to keep the oks stream
//! roughly "as correct as possible" even when errors are present in the errs
//! stream. This reduces the amount of recomputation that must be performed
//! if/when the errors are retracted.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use differential_dataflow::dynamic::pointstamp::PointStamp;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::arrange::{Arranged, TraceAgent};
use differential_dataflow::trace::{Batch, Batcher, Trace, TraceReader};
use differential_dataflow::{AsCollection, Collection, Data, ExchangeData, Hashable};
use itertools::izip;
use mz_compute_types::dataflows::{BuildDesc, DataflowDescription, IndexDesc};
use mz_compute_types::plan::Plan;
use mz_expr::{EvalError, Id};
use mz_persist_client::operators::shard_source::SnapshotMode;
use mz_repr::{Diff, GlobalId};
use mz_storage_operators::persist_source;
use mz_storage_types::controller::CollectionMetadata;
use mz_storage_types::errors::DataflowError;
use mz_timely_util::operator::CollectionExt;
use timely::communication::Allocate;
use timely::container::columnation::Columnation;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::to_stream::ToStream;
use timely::dataflow::operators::{BranchWhen, Operator};
use timely::dataflow::scopes::Child;
use timely::dataflow::{Scope, Stream};
use timely::order::Product;
use timely::progress::timestamp::Refines;
use timely::progress::{Antichain, Timestamp};
use timely::worker::Worker as TimelyWorker;
use timely::PartialOrder;

use crate::arrangement::manager::TraceBundle;
use crate::compute_state::ComputeState;
use crate::extensions::arrange::{ArrangementSize, KeyCollection, MzArrange};
use crate::extensions::reduce::MzReduce;
use crate::logging::compute::{LogDataflowErrors, LogImportFrontiers};
use crate::render::context::{
    ArrangementFlavor, Context, RootContext, ShutdownToken, SpecializedArrangement,
};
use crate::render::flat_map::render_flat_map;
use crate::render::join::{render_delta_join, render_join};
use crate::render::reduce::render_reduce;
use crate::render::sinks::export_sink;
use crate::render::threshold::render_threshold;
use crate::render::top_k::render_topk;
use crate::typedefs::{ErrSpine, KeySpine};

pub mod context;
mod errors;
mod flat_map;
mod join;
mod reduce;
pub mod sinks;
mod threshold;
mod top_k;

pub use context::CollectionBundle;
pub use join::{LinearJoinImpl, LinearJoinSpec};

/// Assemble the "compute"  side of a dataflow, i.e. all but the sources.
///
/// This method imports sources from provided assets, and then builds the remaining
/// dataflow using "compute-local" assets like shared arrangements, and producing
/// both arrangements and sinks.
pub fn build_compute_dataflow<A: Allocate>(
    timely_worker: &mut TimelyWorker<A>,
    compute_state: &mut ComputeState,
    dataflow: DataflowDescription<Plan, CollectionMetadata>,
) {
    // Mutually recursive view definitions require special handling.
    let recursive = dataflow.objects_to_build.iter().any(|object| {
        if let Plan::LetRec { .. } = object.plan {
            true
        } else {
            false
        }
    });

    // Determine indexes to export, and their dependencies.
    let indexes = dataflow
        .index_exports
        .iter()
        .map(|(idx_id, (idx, _typ))| (*idx_id, dataflow.depends_on(idx.on_id), idx.clone()))
        .collect::<Vec<_>>();

    // Determine sinks to export, and their dependencies.
    let sinks = dataflow
        .sink_exports
        .iter()
        .map(|(sink_id, sink)| (*sink_id, dataflow.depends_on(sink.from), sink.clone()))
        .collect::<Vec<_>>();

    let worker_logging = timely_worker.log_register().get("timely");

    let name = format!("Dataflow: {}", &dataflow.debug_name);
    let input_name = format!("InputRegion: {}", &dataflow.debug_name);
    let build_name = format!("BuildRegion: {}", &dataflow.debug_name);

    timely_worker.dataflow_core(&name, worker_logging, Box::new(()), |_, scope| {
        // The scope.clone() occurs to allow import in the region.
        // We build a region here to establish a pattern of a scope inside the dataflow,
        // so that other similar uses (e.g. with iterative scopes) do not require weird
        // alternate type signatures.
        let mut imported_sources = Vec::new();
        let mut tokens = BTreeMap::new();
        scope.clone().region_named(&input_name, |region| {
            // Import declared sources into the rendering context.
            for (source_id, (source, _monotonic)) in dataflow.source_imports.iter() {
                region.region_named(&format!("Source({:?})", source_id), |inner| {
                    let mut mfp = source.arguments.operators.clone().map(|ops| {
                        mz_expr::MfpPlan::create_from(ops)
                            .expect("Linear operators should always be valid")
                    });

                    // Note: For correctness, we require that sources only emit times advanced by
                    // `dataflow.as_of`. `persist_source` is documented to provide this guarantee.
                    let (mut ok_stream, err_stream, token) = persist_source::persist_source(
                        inner,
                        *source_id,
                        Arc::clone(&compute_state.persist_clients),
                        source.storage_metadata.clone(),
                        dataflow.as_of.clone(),
                        SnapshotMode::Include,
                        dataflow.until.clone(),
                        mfp.as_mut(),
                        compute_state.dataflow_max_inflight_bytes,
                    );

                    // If `mfp` is non-identity, we need to apply what remains.
                    // For the moment, assert that it is either trivial or `None`.
                    assert!(mfp.map(|x| x.is_identity()).unwrap_or(true));

                    // To avoid a memory spike during arrangement hydration (#21165), need to
                    // ensure that the first frontier we report into the dataflow is beyond the
                    // `as_of`.
                    if let Some(as_of) = dataflow.as_of.clone() {
                        ok_stream = suppress_early_progress(ok_stream, as_of);
                    }

                    // If logging is enabled, log source frontier advancements. Note that we do
                    // this here instead of in the server.rs worker loop since we want to catch the
                    // wall-clock time of the frontier advancement for each dataflow as early as
                    // possible.
                    if let Some(logger) = compute_state.compute_logger.clone() {
                        let export_ids = dataflow.export_ids().collect();
                        ok_stream = ok_stream.log_import_frontiers(logger, *source_id, export_ids);
                    }

                    let (oks, errs) = (
                        ok_stream.as_collection().leave_region().leave_region(),
                        err_stream.as_collection().leave_region().leave_region(),
                    );

                    imported_sources.push((mz_expr::Id::Global(*source_id), (oks, errs)));

                    // Associate returned tokens with the source identifier.
                    let token: Rc<dyn Any> = Rc::new(token);
                    tokens.insert(*source_id, token);
                });
            }
        });

        // If there exists a recursive expression, we'll need to use a non-region scope,
        // in order to support additional timestamp coordinates for iteration.
        if recursive {
            scope.clone().iterative::<PointStamp<u64>, _, _>(|region| {
                let mut context = RootContext::new(
                    &dataflow,
                    region.clone(),
                    compute_state.linear_join_spec,
                    compute_state.enable_specialized_arrangements,
                );

                for (id, (oks, errs)) in imported_sources.into_iter() {
                    let bundle = crate::render::CollectionBundle::from_collections(
                        oks.enter(region),
                        errs.enter(region),
                    );
                    // Associate collection bundle with the source identifier.
                    context.insert_id(id, bundle);
                }

                // Import declared indexes into the rendering context.
                for (idx_id, idx) in &dataflow.index_imports {
                    let export_ids = dataflow.export_ids().collect();
                    import_index(
                        &mut context,
                        compute_state,
                        &mut tokens,
                        export_ids,
                        *idx_id,
                        &idx.desc,
                    );
                }

                // Build declared objects.
                for object in dataflow.objects_to_build {
                    let object_token = Rc::new(());
                    context.set_shutdown_token(ShutdownToken::new(Rc::downgrade(&object_token)));
                    tokens.insert(object.id, object_token);

                    let bundle = render_recursive_plan(&mut context, 0, object.plan);
                    context.insert_id(Id::Global(object.id), bundle);
                }

                // Export declared indexes.
                for (idx_id, dependencies, idx) in indexes {
                    export_index_iterative(
                        &context,
                        compute_state,
                        &tokens,
                        dependencies,
                        idx_id,
                        &idx,
                    );
                }

                // Export declared sinks.
                for (sink_id, dependencies, sink) in sinks {
                    export_sink(&context, compute_state, &tokens, dependencies, sink_id, &sink);
                }
            });
        } else {
            scope.clone().region_named(&build_name, |region| {
                let mut context = RootContext::new(
                    &dataflow,
                    region.clone(),
                    compute_state.linear_join_spec,
                    compute_state.enable_specialized_arrangements,
                );

                for (id, (oks, errs)) in imported_sources.into_iter() {
                    let bundle = crate::render::CollectionBundle::from_collections(
                        oks.enter_region(region),
                        errs.enter_region(region),
                    );
                    // Associate collection bundle with the source identifier.
                    context.insert_id(id, bundle);
                }

                // Import declared indexes into the rendering context.
                for (idx_id, idx) in &dataflow.index_imports {
                    let export_ids = dataflow.export_ids().collect();
                    import_index(
                        &mut context,
                        compute_state,
                        &mut tokens,
                        export_ids,
                        *idx_id,
                        &idx.desc,
                    );
                }

                // Build declared objects.
                for object in dataflow.objects_to_build {
                    let object_token = Rc::new(());
                    context.set_shutdown_token(ShutdownToken::new(Rc::downgrade(&object_token)));
                    tokens.insert(object.id, object_token);

                    build_object(&mut context, object);
                }

                // Export declared indexes.
                for (idx_id, dependencies, idx) in indexes {
                    export_index(&context, compute_state, &tokens, dependencies, idx_id, &idx);
                }

                // Export declared sinks.
                for (sink_id, dependencies, sink) in sinks {
                    export_sink(&context, compute_state, &tokens, dependencies, sink_id, &sink);
                }
            });
        }
    })
}

fn import_index<'g, C, G, T>(
    ctx: &mut C,
    compute_state: &mut ComputeState,
    tokens: &mut BTreeMap<GlobalId, Rc<dyn std::any::Any>>,
    export_ids: Vec<GlobalId>,
    idx_id: GlobalId,
    idx: &IndexDesc,
) where
    C: Context<Scope = Child<'g, G, T>>,
    G: Scope<Timestamp = mz_repr::Timestamp>,
    T: RenderTimestamp,
{
    if let Some(traces) = compute_state.traces.get_mut(&idx_id) {
        assert!(
            PartialOrder::less_equal(&traces.compaction_frontier(), ctx.as_of()),
            "Index {idx_id} has been allowed to compact beyond the dataflow as_of"
        );

        let token = traces.to_drop().clone();
        // Import the specialized trace handle as a specialized arrangement import.
        // Note that we incorporate optional logging setup as part of this process,
        // since a specialized arrangement import require us to enter a scope, but
        // we can only enter after logging is set up. We attach logging here instead
        // of implementing it in the server.rs worker loop since we want to catch the
        // wall-clock time of the frontier advancement for each dataflow as early as
        // possible.
        let (ok_arranged, ok_button) = traces.oks_mut().import_frontier_logged(
            ctx.scope(),
            &format!("Index({}, {:?})", idx.on_id, idx.key),
            ctx.as_of().clone(),
            ctx.until().clone(),
            compute_state.compute_logger.clone(),
            idx_id,
            export_ids,
        );
        let (err_arranged, err_button) = traces.errs_mut().import_frontier_core(
            &ctx.scope().parent,
            &format!("ErrIndex({}, {:?})", idx.on_id, idx.key),
            ctx.as_of().clone(),
            ctx.until().clone(),
        );
        let err_arranged = err_arranged.enter(ctx.scope());

        ctx.update_id(
            Id::Global(idx.on_id),
            CollectionBundle::from_expressions(
                idx.key.clone(),
                ArrangementFlavor::Trace(idx_id, ok_arranged, err_arranged),
            ),
        );
        tokens.insert(
            idx_id,
            Rc::new((ok_button.press_on_drop(), err_button.press_on_drop(), token)),
        );
    } else {
        panic!(
            "import of index {} failed while building dataflow {}",
            idx_id,
            ctx.dataflow_id()
        );
    }
}

fn build_object<C: Context>(ctx: &mut C, object: BuildDesc<Plan>) {
    // First, transform the relation expression into a render plan.
    let bundle = render_plan(ctx, object.plan);
    ctx.insert_id(Id::Global(object.id), bundle);
}

fn export_index<'g, C, G>(
    ctx: &C,
    compute_state: &mut ComputeState,
    tokens: &BTreeMap<GlobalId, Rc<dyn std::any::Any>>,
    dependency_ids: BTreeSet<GlobalId>,
    idx_id: GlobalId,
    idx: &IndexDesc,
) where
    C: Context<Scope = Child<'g, G, G::Timestamp>>,
    G: Scope<Timestamp = mz_repr::Timestamp>,
{
    // put together tokens that belong to the export
    let mut needed_tokens = Vec::new();
    for dep_id in dependency_ids {
        if let Some(token) = tokens.get(&dep_id) {
            needed_tokens.push(Rc::clone(token));
        }
    }
    let bundle = ctx.lookup_id(Id::Global(idx_id)).unwrap_or_else(|| {
        panic!(
            "Arrangement alarmingly absent! id: {:?}",
            Id::Global(idx_id)
        )
    });

    match bundle.arrangement(&idx.key) {
        Some(ArrangementFlavor::Local(oks, errs)) => {
            // Obtain a specialized handle matching the specialized arrangement.
            let oks_trace = oks.trace_handle();

            // Attach logging of dataflow errors.
            if let Some(logger) = compute_state.compute_logger.clone() {
                errs.stream.log_dataflow_errors(logger, idx_id);
            }

            compute_state.traces.set(
                idx_id,
                TraceBundle::new(oks_trace, errs.trace).with_drop(needed_tokens),
            );
        }
        Some(ArrangementFlavor::Trace(gid, _, _)) => {
            // Duplicate of existing arrangement with id `gid`, so
            // just create another handle to that arrangement.
            let trace = compute_state.traces.get(&gid).unwrap().clone();
            compute_state.traces.set(idx_id, trace);
        }
        None => {
            println!("collection available: {:?}", bundle.collection.is_none());
            println!(
                "keys available: {:?}",
                bundle.arranged.keys().collect::<Vec<_>>()
            );
            panic!(
                "Arrangement alarmingly absent! id: {:?}, keys: {:?}",
                Id::Global(idx_id),
                &idx.key
            );
        }
    };
}

fn export_index_iterative<'g, C, G, T>(
    ctx: &C,
    compute_state: &mut ComputeState,
    tokens: &BTreeMap<GlobalId, Rc<dyn std::any::Any>>,
    dependency_ids: BTreeSet<GlobalId>,
    idx_id: GlobalId,
    idx: &IndexDesc,
) where
    C: Context<Scope = Child<'g, G, T>>,
    G: Scope<Timestamp = mz_repr::Timestamp>,
    T: RenderTimestamp,
{
    // put together tokens that belong to the export
    let mut needed_tokens = Vec::new();
    for dep_id in dependency_ids {
        if let Some(token) = tokens.get(&dep_id) {
            needed_tokens.push(Rc::clone(token));
        }
    }
    let bundle = ctx.lookup_id(Id::Global(idx_id)).unwrap_or_else(|| {
        panic!(
            "Arrangement alarmingly absent! id: {:?}",
            Id::Global(idx_id)
        )
    });

    match bundle.arrangement(&idx.key) {
        Some(ArrangementFlavor::Local(oks, errs)) => {
            let oks = dispatch_rearrange_iterative(oks, "Arrange export iterative");
            let oks_trace = oks.trace_handle();

            let errs = errs
                .as_collection(|k, v| (k.clone(), v.clone()))
                .leave()
                .mz_arrange("Arrange export iterative err");

            // Attach logging of dataflow errors.
            if let Some(logger) = compute_state.compute_logger.clone() {
                errs.stream.log_dataflow_errors(logger, idx_id);
            }

            compute_state.traces.set(
                idx_id,
                TraceBundle::new(oks_trace, errs.trace).with_drop(needed_tokens),
            );
        }
        Some(ArrangementFlavor::Trace(gid, _, _)) => {
            // Duplicate of existing arrangement with id `gid`, so
            // just create another handle to that arrangement.
            let trace = compute_state.traces.get(&gid).unwrap().clone();
            compute_state.traces.set(idx_id, trace);
        }
        None => {
            println!("collection available: {:?}", bundle.collection.is_none());
            println!(
                "keys available: {:?}",
                bundle.arranged.keys().collect::<Vec<_>>()
            );
            panic!(
                "Arrangement alarmingly absent! id: {:?}, keys: {:?}",
                Id::Global(idx_id),
                &idx.key
            );
        }
    };
}

/// Dispatches the rearranging of an arrangement coming from an iterative scope
/// according to specialized key-value arrangement types.
fn dispatch_rearrange_iterative<'g, G, T>(
    oks: SpecializedArrangement<Child<'g, G, T>>,
    name: &str,
) -> SpecializedArrangement<G>
where
    G: Scope<Timestamp = mz_repr::Timestamp>,
    T: RenderTimestamp,
{
    match oks {
        SpecializedArrangement::RowUnit(inner) => {
            let name = format!("{} [val: empty]", name);
            let oks = rearrange_iterative(inner, &name);
            SpecializedArrangement::RowUnit(oks)
        }
        SpecializedArrangement::RowRow(inner) => {
            let oks = rearrange_iterative(inner, name);
            SpecializedArrangement::RowRow(oks)
        }
    }
}

/// Rearranges an arrangement coming from an iterative scope into an arrangement
/// in the outer timestamp scope.
fn rearrange_iterative<'g, G, T, Tr1, Tr2>(
    oks: Arranged<Child<'g, G, T>, TraceAgent<Tr1>>,
    name: &str,
) -> Arranged<G, TraceAgent<Tr2>>
where
    G: Scope<Timestamp = mz_repr::Timestamp>,
    T: RenderTimestamp,
    Tr1: TraceReader<Time = T, Diff = Diff>,
    Tr1::KeyOwned: Columnation + ExchangeData + Hashable,
    Tr1::ValOwned: Columnation + ExchangeData,
    Tr2: Trace + TraceReader<Time = G::Timestamp, Diff = Diff> + 'static,
    Tr2::Batch: Batch,
    Tr2::Batcher: Batcher<Item = ((Tr1::KeyOwned, Tr1::ValOwned), G::Timestamp, Diff)>,
    Arranged<G, TraceAgent<Tr2>>: ArrangementSize,
{
    use differential_dataflow::trace::cursor::MyTrait;
    oks.as_collection(|k, v| (k.into_owned(), v.into_owned()))
        .leave()
        .mz_arrange(name)
}

/// Renders a plan to a differential dataflow, producing the collection of results.
///
/// This method allows for `plan` to contain a `LetRec` variant at its root, and is planned
/// in the context of `level` pre-existing iteration coordinates.
///
/// This method recursively descends `LetRec` nodes, establishing nested scopes for each
/// and establishing the appropriate recursive dependencies among the bound variables.
/// Once non-`LetRec` nodes are reached it calls in to `render_plan` which will error if
/// further `LetRec` variants are found.
///
/// The method requires that all variables conclude with a physical representation that
/// contains a collection (i.e. a non-arrangement), and it will panic otherwise.
fn render_recursive_plan<C: Context>(
    ctx: &mut C,
    level: usize,
    plan: Plan,
) -> CollectionBundle<C::Scope>
where
    C: Context<Timestamp = Product<mz_repr::Timestamp, PointStamp<u64>>>,
{
    if let Plan::LetRec {
        ids,
        values,
        limits,
        body,
    } = plan
    {
        assert_eq!(ids.len(), values.len());
        assert_eq!(ids.len(), limits.len());
        // It is important that we only use the `Variable` until the object is bound.
        // At that point, all subsequent uses should have access to the object itself.
        let mut variables = BTreeMap::new();
        for id in ids.iter() {
            use differential_dataflow::dynamic::feedback_summary;
            use differential_dataflow::operators::iterate::Variable;
            let inner = feedback_summary::<u64>(level + 1, 1);
            let oks_v = Variable::new(
                ctx.scope_mut(),
                Product::new(Default::default(), inner.clone()),
            );
            let err_v = Variable::new(ctx.scope_mut(), Product::new(Default::default(), inner));

            ctx.insert_id(
                Id::Local(*id),
                CollectionBundle::from_collections(oks_v.clone(), err_v.clone()),
            );
            variables.insert(Id::Local(*id), (oks_v, err_v));
        }
        // Now render each of the bindings.
        for (id, value, limit) in izip!(ids.iter(), values.into_iter(), limits.into_iter()) {
            let bundle = render_recursive_plan(ctx, level + 1, value);
            // We need to ensure that the raw collection exists, but do not have enough information
            // here to cause that to happen.
            let (oks, mut err) = bundle.collection.clone().unwrap();
            ctx.insert_id(Id::Local(*id), bundle);
            let (oks_v, err_v) = variables.remove(&Id::Local(*id)).unwrap();

            // Set oks variable to `oks` but consolidated to ensure iteration ceases at fixed point.
            let mut oks = oks.consolidate_named::<KeySpine<_, _, _>>("LetRecConsolidation");
            if let Some(token) = ctx.shutdown_token().get_inner() {
                oks = oks.with_token(Weak::clone(token));
            }

            if let Some(limit) = limit {
                // We swallow the results of the `max_iter`th iteration, because
                // these results would go into the `max_iter + 1`th iteration.
                let (in_limit, over_limit) =
                    oks.inner.branch_when(move |Product { inner: ps, .. }| {
                        // We get None in the first iteration, because the `PointStamp` doesn't yet have
                        // the `level`th element. It will get created when applying the summary for the
                        // first time.
                        let iteration_index = *ps.vector.get(level).unwrap_or(&0);
                        // The pointstamp starts counting from 0, so we need to add 1.
                        iteration_index + 1 >= limit.max_iters.into()
                    });
                oks = Collection::new(in_limit);
                if !limit.return_at_limit {
                    err = err.concat(&Collection::new(over_limit).map(move |_data| {
                        DataflowError::EvalError(Box::new(EvalError::LetRecLimitExceeded(format!(
                            "{}",
                            limit.max_iters.get()
                        ))))
                    }));
                }
            }

            oks_v.set(&oks);

            // Set err variable to the distinct elements of `err`.
            // Distinctness is important, as we otherwise might add the same error each iteration,
            // say if the limit of `oks` has an error. This would result in non-terminating rather
            // than a clean report of the error. The trade-off is that we lose information about
            // multiplicities of errors, but .. this seems to be the better call.
            let err: KeyCollection<_, _, _> = err.into();
            let mut errs = err
                .mz_arrange::<ErrSpine<_, _>>("Arrange recursive err")
                .mz_reduce_abelian::<_, ErrSpine<_, _>>(
                    "Distinct recursive err",
                    move |_k, _s, t| t.push(((), 1)),
                )
                .as_collection(|k, _| k.clone());
            if let Some(token) = &ctx.shutdown_token().get_inner() {
                errs = errs.with_token(Weak::clone(token));
            }
            err_v.set(&errs);
        }
        // Now extract each of the bindings into the outer scope.
        for id in ids.into_iter() {
            let bundle = ctx.remove_id(Id::Local(id)).unwrap();
            let (oks, err) = bundle.collection.unwrap();
            ctx.insert_id(
                Id::Local(id),
                CollectionBundle::from_collections(
                    oks.leave_dynamic(level + 1),
                    err.leave_dynamic(level + 1),
                ),
            );
        }

        render_recursive_plan(ctx, level, *body)
    } else {
        render_plan(ctx, plan)
    }
}

/// Renders a plan to a differential dataflow, producing the collection of results.
///
/// The return type reflects the uncertainty about the data representation, perhaps
/// as a stream of data, perhaps as an arrangement, perhaps as a stream of batches.
fn render_plan<C: Context>(ctx: &mut C, plan: Plan) -> CollectionBundle<C::Scope> {
    match plan {
        Plan::Constant { rows } => {
            // Produce both rows and errs to avoid conditional dataflow construction.
            let (rows, errs) = match rows {
                Ok(rows) => (rows, Vec::new()),
                Err(e) => (Vec::new(), vec![e]),
            };

            // We should advance times in constant collections to start from `as_of`.
            let as_of_frontier = ctx.as_of().clone();
            let until = ctx.until().clone();
            let ok_collection = rows
                .into_iter()
                .filter_map(move |(row, mut time, diff)| {
                    time.advance_by(as_of_frontier.borrow());
                    if !until.less_equal(&time) {
                        Some((row, C::Timestamp::to_inner(time), diff))
                    } else {
                        None
                    }
                })
                .to_stream(ctx.scope_mut())
                .as_collection();

            let mut error_time: mz_repr::Timestamp = Timestamp::minimum();
            error_time.advance_by(ctx.as_of().borrow());
            let err_collection = errs
                .into_iter()
                .map(move |e| {
                    (
                        DataflowError::from(e),
                        C::Timestamp::to_inner(error_time),
                        1,
                    )
                })
                .to_stream(ctx.scope_mut())
                .as_collection();

            CollectionBundle::from_collections(ok_collection, err_collection)
        }
        Plan::Get { id, keys, plan } => {
            // Recover the collection from `self` and then apply `mfp` to it.
            // If `mfp` happens to be trivial, we can just return the collection.
            let mut collection = ctx
                .lookup_id(id)
                .unwrap_or_else(|| panic!("Get({:?}) not found at render time", id));
            match plan {
                mz_compute_types::plan::GetPlan::PassArrangements => {
                    // Assert that each of `keys` are present in `collection`.
                    assert!(keys
                        .arranged
                        .iter()
                        .all(|(key, _, _)| collection.arranged.contains_key(key)));
                    assert!(keys.raw <= collection.collection.is_some());
                    // Retain only those keys we want to import.
                    collection
                        .arranged
                        .retain(|key, _value| keys.arranged.iter().any(|(key2, _, _)| key2 == key));
                    collection
                }
                mz_compute_types::plan::GetPlan::Arrangement(key, row, mfp) => {
                    let (oks, errs) =
                        collection.as_collection_core(mfp, Some((key, row)), ctx.until().clone());
                    CollectionBundle::from_collections(oks, errs)
                }
                mz_compute_types::plan::GetPlan::Collection(mfp) => {
                    let (oks, errs) = collection.as_collection_core(mfp, None, ctx.until().clone());
                    CollectionBundle::from_collections(oks, errs)
                }
            }
        }
        Plan::Let { id, value, body } => {
            // Render `value` and bind it to `id`. Complain if this shadows an id.
            let value = render_plan(ctx, *value);
            let prebound = ctx.insert_id(Id::Local(id), value);
            assert!(prebound.is_none());

            let body = render_plan(ctx, *body);
            ctx.remove_id(Id::Local(id));
            body
        }
        Plan::LetRec { .. } => {
            unreachable!("LetRec should have been extracted and rendered");
        }
        Plan::Mfp {
            input,
            mfp,
            input_key_val,
        } => {
            let input = render_plan(ctx, *input);
            // If `mfp` is non-trivial, we should apply it and produce a collection.
            if mfp.is_identity() {
                input
            } else {
                let (oks, errs) = input.as_collection_core(mfp, input_key_val, ctx.until().clone());
                CollectionBundle::from_collections(oks, errs)
            }
        }
        Plan::FlatMap {
            input,
            func,
            exprs,
            mfp_after: mfp,
            input_key,
        } => {
            let input = render_plan(ctx, *input);
            render_flat_map(ctx, input, func, exprs, mfp, input_key)
        }
        Plan::Join { inputs, plan } => {
            let inputs = inputs
                .into_iter()
                .map(|input| render_plan(ctx, input))
                .collect();
            match plan {
                mz_compute_types::plan::join::JoinPlan::Linear(linear_plan) => {
                    render_join(ctx, inputs, linear_plan)
                }
                mz_compute_types::plan::join::JoinPlan::Delta(delta_plan) => {
                    render_delta_join(ctx, inputs, delta_plan)
                }
            }
        }
        Plan::Reduce {
            input,
            key_val_plan,
            plan,
            input_key,
            mfp_after,
        } => {
            let input = render_plan(ctx, *input);
            let mfp_option = (!mfp_after.is_identity()).then_some(mfp_after);
            render_reduce(ctx, input, key_val_plan, plan, input_key, mfp_option)
        }
        Plan::TopK { input, top_k_plan } => {
            let input = render_plan(ctx, *input);
            render_topk(ctx, input, top_k_plan)
        }
        Plan::Negate { input } => {
            let input = render_plan(ctx, *input);
            let (oks, errs) = input.as_specific_collection(None);
            CollectionBundle::from_collections(oks.negate(), errs)
        }
        Plan::Threshold {
            input,
            threshold_plan,
        } => {
            let input = render_plan(ctx, *input);
            render_threshold(input, threshold_plan)
        }
        Plan::Union {
            inputs,
            consolidate_output,
        } => {
            let mut oks = Vec::new();
            let mut errs = Vec::new();
            for input in inputs.into_iter() {
                let (os, es) = render_plan(ctx, input).as_specific_collection(None);
                oks.push(os);
                errs.push(es);
            }
            let mut oks = differential_dataflow::collection::concatenate(ctx.scope_mut(), oks);
            if consolidate_output {
                oks = oks.consolidate_named::<KeySpine<_, _, _>>("UnionConsolidation")
            }
            let errs = differential_dataflow::collection::concatenate(ctx.scope_mut(), errs);
            CollectionBundle::from_collections(oks, errs)
        }
        Plan::ArrangeBy {
            input,
            forms: keys,
            input_key,
            input_mfp,
        } => {
            let input = render_plan(ctx, *input);
            input.ensure_collections(
                keys,
                input_key,
                input_mfp,
                ctx.until().clone(),
                ctx.enable_specialized_arrangements(),
            )
        }
    }
}

/// A timestamp type that can be used for operations within MZ's dataflow layer.
pub trait RenderTimestamp: Timestamp + Lattice + Refines<mz_repr::Timestamp> + Columnation {
    /// The system timestamp component of the timestamp.
    ///
    /// This is useful for manipulating the system time, as when delaying
    /// updates for subsequent cancellation, as with monotonic reduction.
    fn system_time(&mut self) -> &mut mz_repr::Timestamp;
    /// Effects a system delay in terms of the timestamp summary.
    fn system_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary;
    /// The event timestamp component of the timestamp.
    fn event_time(&mut self) -> &mut mz_repr::Timestamp;
    /// Effects an event delay in terms of the timestamp summary.
    fn event_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary;
    /// Steps the timestamp back so that logical compaction to the output will
    /// not conflate `self` with any historical times.
    fn step_back(&self) -> Self;
}

impl RenderTimestamp for mz_repr::Timestamp {
    fn system_time(&mut self) -> &mut mz_repr::Timestamp {
        self
    }
    fn system_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary {
        delay
    }
    fn event_time(&mut self) -> &mut mz_repr::Timestamp {
        self
    }
    fn event_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary {
        delay
    }
    fn step_back(&self) -> Self {
        self.saturating_sub(1)
    }
}

impl<T: Timestamp + Lattice + Columnation> RenderTimestamp for Product<mz_repr::Timestamp, T> {
    fn system_time(&mut self) -> &mut mz_repr::Timestamp {
        &mut self.outer
    }
    fn system_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary {
        Product::new(delay, Default::default())
    }
    fn event_time(&mut self) -> &mut mz_repr::Timestamp {
        &mut self.outer
    }
    fn event_delay(delay: mz_repr::Timestamp) -> <Self as Timestamp>::Summary {
        Product::new(delay, Default::default())
    }
    fn step_back(&self) -> Self {
        Product::new(self.outer.saturating_sub(1), self.inner.clone())
    }
}

/// Suppress progress messages for times before the given `as_of`.
///
/// This operator exists specifically to work around a memory spike we'd otherwise see when
/// hydrating arrangements (#21165). The memory spike happens because when the `arrange_core`
/// operator observes a frontier advancement without data it inserts an empty batch into the spine.
/// When it later inserts the snapshot batch into the spine, an empty batch is already there and
/// the spine initiates a merge of these batches, which requires allocating a new batch the size of
/// the snapshot batch.
///
/// The strategy to avoid the spike is to prevent the insertion of that initial empty batch by
/// ensuring that the first frontier advancement downstream `arrange_core` operators observe is
/// beyond the `as_of`, so the snapshot data has already been collected.
///
/// To ensure this, this operator needs to take two measures:
///  * Keep around a minimum capability until the input announces progress beyond the `as_of`.
///  * Reclock all updates emitted at times not beyond the `as_of` to the minimum time.
///
/// The second measure requires elaboration: If we wouldn't reclock snapshot updates, they might
/// still be upstream of `arrange_core` operators when those get to know about us dropping the
/// minimum capability. The in-flight snapshot updates would hold back the input frontiers of
/// `arrange_core` operators to the `as_of`, which would cause them to insert empty batches.
fn suppress_early_progress<G, D>(
    stream: Stream<G, D>,
    as_of: Antichain<G::Timestamp>,
) -> Stream<G, D>
where
    G: Scope,
    D: Data,
{
    stream.unary_frontier(Pipeline, "SuppressEarlyProgress", |default_cap, _info| {
        let mut early_cap = Some(default_cap);

        let mut buffer = Default::default();
        move |input, output| {
            input.for_each(|data_cap, data| {
                data.swap(&mut buffer);
                let mut session = if as_of.less_than(data_cap.time()) {
                    output.session(&data_cap)
                } else {
                    let cap = early_cap.as_ref().expect("early_cap can't be dropped yet");
                    output.session(cap)
                };
                session.give_vec(&mut buffer);
            });

            let frontier = input.frontier().frontier();
            if !PartialOrder::less_equal(&frontier, &as_of.borrow()) {
                early_cap.take();
            }
        }
    })
}
