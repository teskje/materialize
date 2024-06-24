// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Preflight checks for deployments.

use std::sync::Arc;

use anyhow::anyhow;
use mz_catalog::durable::{BootstrapArgs, CatalogError, Metrics, OpenableDurableCatalogState};
use mz_persist_client::PersistClient;
use mz_sql::catalog::EnvironmentId;

use crate::deployment::state::DeploymentState;
use crate::BUILD_INFO;

/// The necessary input for preflight checks.
pub struct PreflightInput {
    pub boot_ts: u64,
    pub environment_id: EnvironmentId,
    pub persist_client: PersistClient,
    pub bootstrap_default_cluster_replica_size: String,
    pub bootstrap_role: Option<String>,
    pub deploy_generation: u64,
    pub deployment_state: DeploymentState,
    pub openable_adapter_storage: Box<dyn OpenableDurableCatalogState>,
    pub catalog_metrics: Arc<Metrics>,
}

/// Perform a legacy (non-0dt) preflight check.
pub async fn preflight_legacy(
    PreflightInput {
        boot_ts,
        environment_id,
        persist_client,
        bootstrap_default_cluster_replica_size,
        bootstrap_role,
        deploy_generation,
        deployment_state,
        mut openable_adapter_storage,
        catalog_metrics,
    }: PreflightInput,
) -> Result<Box<dyn OpenableDurableCatalogState>, anyhow::Error> {
    tracing::info!("Requested deploy generation {deploy_generation}");

    if !openable_adapter_storage.is_initialized().await? {
        tracing::info!("Catalog storage doesn't exist so there's no current deploy generation. We won't wait to be leader");
        deployment_state.set_is_leader();
        return Ok(openable_adapter_storage);
    }
    let catalog_generation = openable_adapter_storage.get_deployment_generation().await?;
    tracing::info!("Found catalog generation {catalog_generation:?}");
    if catalog_generation < deploy_generation {
        tracing::info!("Catalog generation {catalog_generation:?} is less than deploy generation {deploy_generation}. Performing pre-flight checks");
        match openable_adapter_storage
            .open_savepoint(
                boot_ts.clone(),
                &BootstrapArgs {
                    default_cluster_replica_size: bootstrap_default_cluster_replica_size,
                    bootstrap_role,
                },
                deploy_generation,
                None,
            )
            .await
        {
            Ok(adapter_storage) => Box::new(adapter_storage).expire().await,
            Err(CatalogError::Durable(e)) if e.can_recover_with_write_mode() => {
                // This is theoretically possible if catalog implementation A is
                // initialized, implementation B is uninitialized, and we are going to
                // migrate from A to B. The current code avoids this by always
                // initializing all implementations, regardless of the target
                // implementation. Still it's easy to protect against this and worth it in
                // case things change in the future.
                tracing::warn!("Unable to perform upgrade test because the target implementation is uninitialized");
                return Ok(mz_catalog::durable::persist_backed_catalog_state(
                    persist_client,
                    environment_id.organization_id(),
                    BUILD_INFO.semver_version(),
                    Arc::clone(&catalog_metrics),
                )
                .await?);
            }
            Err(e) => {
                return Err(anyhow!(e).context("Catalog upgrade would have failed with this error"))
            }
        }

        let promoted = deployment_state.set_ready_to_promote();

        tracing::info!("Waiting for user to promote this envd to leader");
        promoted.await;

        Ok(mz_catalog::durable::persist_backed_catalog_state(
            persist_client,
            environment_id.organization_id(),
            BUILD_INFO.semver_version(),
            Arc::clone(&catalog_metrics),
        )
        .await?)
    } else if catalog_generation == deploy_generation {
        tracing::info!("Server requested generation {deploy_generation} which is equal to catalog's generation");
        deployment_state.set_is_leader();
        Ok(openable_adapter_storage)
    } else {
        mz_ore::halt!("Server started with requested generation {deploy_generation} but catalog was already at {catalog_generation:?}. Deploy generations must increase monotonically");
    }
}
