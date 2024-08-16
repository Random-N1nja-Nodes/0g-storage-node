use super::{batcher::Batcher, sync_store::SyncStore};
use crate::{
    auto_sync::{batcher::SyncResult, metrics},
    Config, SyncSender,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use storage_async::Store;
use tokio::time::sleep;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RandomBatcherState {
    pub tasks: Vec<u64>,
    pub pending_txs: usize,
    pub ready_txs: usize,
}

#[derive(Clone)]
pub struct RandomBatcher {
    batcher: Batcher,
    sync_store: Arc<SyncStore>,
}

impl RandomBatcher {
    pub fn new(
        config: Config,
        store: Store,
        sync_send: SyncSender,
        sync_store: Arc<SyncStore>,
    ) -> Self {
        Self {
            batcher: Batcher::new(config, config.max_random_workers, store, sync_send),
            sync_store,
        }
    }

    pub async fn get_state(&self) -> Result<RandomBatcherState> {
        let (pending_txs, ready_txs) = self.sync_store.stat().await?;

        Ok(RandomBatcherState {
            tasks: self.batcher.tasks().await,
            pending_txs,
            ready_txs,
        })
    }

    pub async fn start(mut self, catched_up: Arc<AtomicBool>) {
        info!("Start to sync files");

        loop {
            // disable file sync until catched up
            if !catched_up.load(Ordering::Relaxed) {
                trace!("Cannot sync file in catch-up phase");
                sleep(self.batcher.config.auto_sync_idle_interval).await;
                continue;
            }

            if let Ok(state) = self.get_state().await {
                metrics::RANDOM_STATE_TXS_SYNCING.update(state.tasks.len() as u64);
                metrics::RANDOM_STATE_TXS_READY.update(state.ready_txs as u64);
                metrics::RANDOM_STATE_TXS_PENDING.update(state.pending_txs as u64);
            }

            match self.sync_once().await {
                Ok(true) => {}
                Ok(false) => {
                    trace!(
                        "File sync still in progress or idle, state = {:?}",
                        self.get_state().await
                    );
                    sleep(self.batcher.config.auto_sync_idle_interval).await;
                }
                Err(err) => {
                    warn!(%err, "Failed to sync file once, state = {:?}", self.get_state().await);
                    sleep(self.batcher.config.auto_sync_error_interval).await;
                }
            }
        }
    }

    async fn sync_once(&mut self) -> Result<bool> {
        if self.schedule().await? {
            return Ok(true);
        }

        // poll any completed file sync
        let (tx_seq, sync_result) = match self.batcher.poll().await? {
            Some(v) => v,
            None => return Ok(false),
        };

        debug!(%tx_seq, ?sync_result, "Completed to sync file, state = {:?}", self.get_state().await);
        match sync_result {
            SyncResult::Completed => metrics::RANDOM_SYNC_RESULT_COMPLETED.inc(1),
            SyncResult::Failed => metrics::RANDOM_SYNC_RESULT_FAILED.inc(1),
            SyncResult::Timeout => metrics::RANDOM_SYNC_RESULT_TIMEOUT.inc(1),
        }

        match sync_result {
            SyncResult::Completed => self.sync_store.remove_tx(tx_seq).await?,
            _ => self.sync_store.downgrade_tx_to_pending(tx_seq).await?,
        };

        Ok(true)
    }

    async fn schedule(&mut self) -> Result<bool> {
        let tx_seq = match self.sync_store.random_tx().await? {
            Some(v) => v,
            None => return Ok(false),
        };

        if !self.batcher.add(tx_seq).await? {
            return Ok(false);
        }

        debug!("Pick a file to sync, state = {:?}", self.get_state().await);

        Ok(true)
    }
}
