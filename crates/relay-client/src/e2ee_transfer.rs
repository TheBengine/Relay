//! Recovered history never participates in MLS ratchets, unread or reaction ordering.
use super::*;
#[cfg(test)]
use crate::history_transfer::Snapshot;
use crate::network::CommandOrigin;
use std::sync::atomic::AtomicBool;
impl CryptoSession {
    pub fn transfer_source(
        &self,
        origin: &CommandOrigin,
    ) -> Result<history::TransferSource, String> {
        if self.state.identity.server_id != origin.server_id
            || self.state.identity.user_id != origin.user_id
        {
            return Err("History account changed.".into());
        }
        Ok(history::transfer_source(
            &self.state,
            &self.path,
            self.binding(),
        ))
    }
    #[cfg(test)]
    pub fn transfer_snapshot(&self, origin: &CommandOrigin) -> Result<Snapshot, String> {
        self.transfer_source(origin)?
            .snapshot(origin, &AtomicBool::new(false))
    }
    #[cfg(test)]
    pub fn import_transferred_history(
        &mut self,
        snapshot: Snapshot,
        origin: &CommandOrigin,
    ) -> Result<usize, String> {
        let cancel = AtomicBool::new(false);
        let prepared = self
            .transfer_source(origin)?
            .stage(snapshot, origin, &cancel)?;
        self.commit_transferred_history(prepared, &cancel)
    }
    pub fn commit_transferred_history(
        &mut self,
        mut prepared: history::PreparedTransfer,
        cancel: &AtomicBool,
    ) -> Result<usize, String> {
        let count = prepared.count;
        if count > 0 {
            self.mutate(|_, state| prepared.attach(state, cancel))?;
        } else {
            let mut state = self.state.clone();
            prepared.attach(&mut state, cancel)?;
        }
        prepared.committed();
        Ok(count)
    }
    pub fn recovered_content(
        &self,
        scope: &str,
        wire: &str,
    ) -> Result<Option<CachedEnvelope>, String> {
        history::recovered_lookup(&self.state, &self.path, &self.binding(), &cipher_hash(wire))?
            .map(|entry| checked_cached(&entry, scope))
            .transpose()
    }
    pub fn stored_content(
        &self,
        scope: &str,
        wire: &str,
    ) -> Result<Option<(CachedEnvelope, bool)>, String> {
        if let Some(row) =
            history::lookup(&self.state, &self.path, &self.binding(), &cipher_hash(wire))?
        {
            return Ok(Some((checked_cached(&row, scope)?, false)));
        }
        Ok(self.recovered_content(scope, wire)?.map(|row| (row, true)))
    }
}
