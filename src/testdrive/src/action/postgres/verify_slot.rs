// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::cmp;
use std::time::Duration;

use anyhow::{Context, bail};
use mz_ore::retry::Retry;

use crate::action::{ControlFlow, State};
use crate::parser::BuiltinCommand;
use crate::util::postgres::postgres_client;

pub async fn run_verify_slot(
    mut cmd: BuiltinCommand,
    state: &State,
) -> Result<ControlFlow, anyhow::Error> {
    let connection = cmd.args.string("connection")?;
    let slot = cmd.args.string("slot")?;
    let expect_active: bool = cmd.args.parse("active")?;
    cmd.args.done()?;

    let (client, conn_handle) = postgres_client(&connection, state.default_timeout).await?;

    Retry::default()
        .initial_backoff(Duration::from_millis(50))
        .max_duration(cmp::max(state.default_timeout, Duration::from_secs(60)))
        .retry_async_canceling(|_| async {
            println!(">> checking for postgres replication slot {}", &slot);
            let rows = client
                .query(
                    "SELECT active_pid FROM pg_replication_slots WHERE slot_name LIKE $1::TEXT",
                    &[&slot],
                )
                .await
                .context("querying postgres for replication slot")?;

            if rows.len() != 1 {
                bail!(
                    "expected entry for slot {} in pg_replication slots, found {}",
                    &slot,
                    rows.len()
                );
            }
            let active_pid: Option<i32> = rows[0].get(0);
            match (expect_active, active_pid) {
                (true, None) => bail!("expected slot {slot} to be active, is inactive"),
                (false, Some(pid)) => {
                    bail!("expected slot {slot} to be inactive, is active for pid {pid}")
                }
                _ => {}
            };
            Ok(())
        })
        .await?;

    drop(client);
    conn_handle
        .await
        .unwrap()
        .context("postgres connection error")?;

    Ok(ControlFlow::Continue)
}
