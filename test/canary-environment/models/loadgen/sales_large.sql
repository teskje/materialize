-- Copyright Materialize, Inc. and contributors. All rights reserved.
--
-- Use of this software is governed by the Business Source License
-- included in the LICENSE file at the root of this repository.
--
-- As of the Change Date specified in that file, in accordance with
-- the Business Source License, use of this software will be governed
-- by the Apache License, Version 2.0.

{{ config(materialized='source', cluster='qa_canary_environment_storage', indexes=[{'default': True, 'cluster': 'qa_canary_environment_compute'}]) }}
{{ create_large_loadgen_source('sales') }}
