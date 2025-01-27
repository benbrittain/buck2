/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use allocative::Allocative;
use dupe::Dupe;

use crate::api::cycles::DetectCycles;
use crate::api::data::DiceData;
use crate::api::user_data::UserComputationData;
use crate::impls::core::state::init_state;
use crate::impls::core::state::CoreStateHandle;
use crate::impls::core::state::StateRequest;
use crate::impls::key_index::DiceKeyIndex;
use crate::impls::transaction::TransactionUpdater;
use crate::introspection::graph::GraphIntrospectable;
use crate::metrics::Metrics;

#[derive(Allocative)]
pub(crate) struct DiceModern {
    pub(crate) key_index: DiceKeyIndex,
    pub(crate) state_handle: CoreStateHandle,
    pub(crate) global_data: DiceData,
}

impl Debug for DiceModern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiceModern").finish_non_exhaustive()
    }
}

pub(crate) struct DiceModernDataBuilder(DiceData);

impl DiceModernDataBuilder {
    pub(crate) fn new() -> Self {
        Self(DiceData::new())
    }

    pub fn set<K: Send + Sync + 'static>(&mut self, val: K) {
        self.0.set(val);
    }

    pub fn build(self, _detect_cycles: DetectCycles) -> Arc<DiceModern> {
        DiceModern::new(self.0)
    }
}

impl DiceModern {
    pub(crate) fn new(global_data: DiceData) -> Arc<Self> {
        let state_handle = init_state();

        Arc::new(DiceModern {
            key_index: Default::default(),
            state_handle,
            global_data,
        })
    }

    #[cfg(test)]
    pub(crate) fn builder() -> DiceModernDataBuilder {
        DiceModernDataBuilder::new()
    }

    pub fn updater(self: &Arc<Self>) -> TransactionUpdater {
        self.updater_with_data(UserComputationData::new())
    }

    pub fn updater_with_data(self: &Arc<Self>, extra: UserComputationData) -> TransactionUpdater {
        TransactionUpdater::new(self.dupe(), Arc::new(extra))
    }

    pub fn metrics(&self) -> Metrics {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.state_handle
            .request(StateRequest::Metrics { resp: tx });

        // Modern dice can just run on a blocking runtime and block waiting for the channel.
        // This is safe since the processing dice thread is dedicated, and never awaits any other tasks.
        tokio::task::block_in_place(|| rx.blocking_recv().unwrap())
    }

    pub fn to_introspectable(&self) -> GraphIntrospectable {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.state_handle.request(StateRequest::Introspection {
            resp: tx,
            key_map: self.key_index.introspect(),
        });

        rx.blocking_recv().unwrap()
    }

    /// Note: modern dice does not support cycle detection yet
    pub fn detect_cycles(&self) -> &DetectCycles {
        // TODO(bobyf) actually have cycles for dice modern
        const CYCLES: DetectCycles = DetectCycles::Disabled;
        &CYCLES
    }

    /// Wait until all active versions have exited.
    pub fn wait_for_idle(&self) -> impl Future<Output = ()> + 'static {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.state_handle
            .request(StateRequest::GetTasksPendingCancellation { resp: tx });

        async move {
            let tasks = rx.await.unwrap();
            futures::future::join_all(tasks).await;
        }
    }

    /// true when there are no tasks pending cancellation
    pub async fn is_idle(&self) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.state_handle
            .request(StateRequest::GetTasksPendingCancellation { resp: tx });

        let tasks = rx.await.unwrap();

        tasks.iter().all(|task| task.is_terminated())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use dupe::Dupe;

    use crate::impls::core::state::StateRequest;
    use crate::impls::ctx::SharedLiveTransactionCtx;
    use crate::impls::dice::DiceModern;
    use crate::impls::transaction::ActiveTransactionGuard;
    use crate::versions::VersionNumber;

    impl DiceModern {
        pub(crate) async fn testing_shared_ctx(
            &self,
            v: VersionNumber,
        ) -> (SharedLiveTransactionCtx, ActiveTransactionGuard) {
            let (tx, rx) = tokio::sync::oneshot::channel();

            let guard = ActiveTransactionGuard::new(v, self.state_handle.dupe());
            self.state_handle.request(StateRequest::CtxAtVersion {
                version: v,
                guard,
                resp: tx,
            });
            rx.await.unwrap()
        }
    }
}
