// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

use std::env;

fn main() {
    env::set_var("PROTOC", protobuf_src::protoc());

    let mut config = prost_build::Config::new();
    config.btree_map(["."]);

    tonic_build::configure()
        // Enabling `emit_rerun_if_changed` will rerun the build script when
        // anything in the include directory (..) changes. This causes quite a
        // bit of spurious recompilation, so we disable it. The default behavior
        // is to re-run if any file in the crate changes; that's still a bit too
        // broad, but it's better.
        .emit_rerun_if_changed(false)
        .extern_path(".mz_ccsr.config", "::mz_ccsr")
        .extern_path(".mz_expr.id", "::mz_expr")
        .extern_path(".mz_expr.linear", "::mz_expr")
        .extern_path(".mz_expr.relation", "::mz_expr")
        .extern_path(".mz_expr.scalar", "::mz_expr")
        .extern_path(".mz_kafka_util.addr", "::mz_kafka_util")
        .extern_path(".mz_postgres_util.desc", "::mz_postgres_util::desc")
        .extern_path(".mz_repr.adt.regex", "::mz_repr::adt::regex")
        .extern_path(".mz_repr.chrono", "::mz_repr::chrono")
        .extern_path(".mz_repr.antichain", "::mz_repr::antichain")
        .extern_path(".mz_repr.global_id", "::mz_repr::global_id")
        .extern_path(".mz_orchestrator", "::mz_orchestrator")
        .extern_path(".mz_persist_client", "::mz_persist_client")
        .extern_path(".mz_proto", "::mz_proto")
        .extern_path(".mz_repr.relation_and_scalar", "::mz_repr")
        .extern_path(".mz_repr.row", "::mz_repr")
        .extern_path(".mz_repr.url", "::mz_repr::url")
        .extern_path(".mz_rocksdb_types", "::mz_rocksdb_types")
        .extern_path(".mz_cluster_client", "::mz_cluster_client")
        .extern_path(".mz_tracing", "::mz_tracing")
        .extern_path(".mz_service", "::mz_service")
        .compile_with_config(
            config,
            &[
                "storage-client/src/controller.proto",
                "storage-client/src/client.proto",
                "storage-client/src/types/errors.proto",
                "storage-client/src/types/connections/aws.proto",
                "storage-client/src/types/instances.proto",
                "storage-client/src/types/parameters.proto",
                "storage-client/src/types/sinks.proto",
                "storage-client/src/types/sources.proto",
                "storage-client/src/types/sources/encoding.proto",
            ],
            &[".."],
        )
        .unwrap_or_else(|e| panic!("{e}"))
}
