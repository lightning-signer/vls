#[cfg(feature = "redb-kvv")]
pub mod redb;

use crate::model::*;
use lightning_signer::bitcoin::secp256k1::PublicKey;
use lightning_signer::chain::tracker::ChainTracker;
use lightning_signer::channel::{Channel, ChannelId, ChannelStub};
use lightning_signer::monitor::ChainMonitor;
use lightning_signer::node::{NodeConfig, NodeState};
use lightning_signer::persist::model::{
    ChannelEntry as CoreChannelEntry, NodeEntry as CoreNodeEntry,
};
use lightning_signer::persist::{Error, Persist};
use lightning_signer::policy::validator::{EnforcementState, ValidatorFactory};
use lightning_signer::SendSync;
use serde_json::{from_slice, to_vec};
use std::ops::Deref;
use std::sync::Arc;

const NODE_ENTRY_PREFIX: &str = "node/entry";
const NODE_STATE_PREFIX: &str = "node/state";
const NODE_TRACKER_PREFIX: &str = "node/tracker";
const ALLOWLIST_PREFIX: &str = "node/allowlist";
const CHANNEL_PREFIX: &str = "channel";
const SEPARATOR: &str = "/";

/// A key-version-value store
pub trait KVVStore: SendSync {
    /// Put a key-value pair into the store
    fn put(&self, key: &str, value: &[u8]) -> Result<(), Error>;
    /// If the key already exists, the version must be one greater than the existing version,
    /// and if it does not exist, the version must be 0.
    fn put_with_version(&self, key: &str, version: u64, value: &[u8]) -> Result<(), Error>;
    /// Get a key-value pair from the store
    /// Returns Ok(None) if the key does not exist.
    fn get(&self, key: &str) -> Result<Option<(u64, Vec<u8>)>, Error>;
    /// Get all key-value pairs with the given prefix
    fn get_prefix(&self, prefix: &str) -> Result<Vec<(String, (u64, Vec<u8>))>, Error>;
    /// Delete a key-value pair from the store
    fn delete(&self, key: &str) -> Result<(), Error>;
    /// Clear the database
    fn clear_database(&self) -> Result<(), Error>;
}

pub struct KVVPersister(pub Box<dyn KVVStore>);

impl Deref for KVVPersister {
    type Target = dyn KVVStore;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl SendSync for KVVPersister {}

impl Persist for KVVPersister {
    fn new_node(
        &self,
        node_id: &PublicKey,
        config: &NodeConfig,
        state: &NodeState,
    ) -> Result<(), Error> {
        self.update_node(node_id, state).unwrap();
        let key = make_key(NODE_ENTRY_PREFIX, &node_id.serialize());
        let entry = NodeEntry {
            key_derivation_style: config.key_derivation_style as u8,
            network: config.network.to_string(),
        };
        let value = to_vec(&entry).unwrap();
        self.put(&key, &value)
    }

    fn update_node(&self, node_id: &PublicKey, state: &NodeState) -> Result<(), Error> {
        let key = make_key(NODE_STATE_PREFIX, &node_id.serialize());
        let entry: NodeStateEntry = state.into();
        let value = to_vec(&entry).unwrap();
        self.put(&key, &value)
    }

    fn delete_node(&self, node_id: &PublicKey) -> Result<(), Error> {
        let id = node_id.serialize();
        self.delete(&make_key(NODE_ENTRY_PREFIX, &id))?;
        self.delete(&make_key(NODE_STATE_PREFIX, &id))
    }

    fn new_channel(&self, node_id: &PublicKey, stub: &ChannelStub) -> Result<(), Error> {
        let key = make_key2(CHANNEL_PREFIX, &node_id.serialize(), stub.id0.as_slice());
        let channel_value_satoshis = 0;

        let entry = ChannelEntry {
            channel_value_satoshis,
            channel_setup: None,
            id: None,
            enforcement_state: EnforcementState::new(0),
            blockheight: Some(stub.blockheight),
        };
        let value = to_vec(&entry).unwrap();
        self.put(&key, &value)
    }

    fn delete_channel(&self, node_id: &PublicKey, channel_id: &ChannelId) -> Result<(), Error> {
        let key = make_key2(CHANNEL_PREFIX, &node_id.serialize(), channel_id.as_slice());
        self.delete(&key)
    }

    fn new_chain_tracker(
        &self,
        node_id: &PublicKey,
        tracker: &ChainTracker<ChainMonitor>,
    ) -> Result<(), Error> {
        self.update_tracker(node_id, tracker)
    }

    fn update_tracker(
        &self,
        node_id: &PublicKey,
        tracker: &ChainTracker<ChainMonitor>,
    ) -> Result<(), Error> {
        let key = make_key(NODE_TRACKER_PREFIX, &node_id.serialize());
        let model: ChainTrackerEntry = tracker.into();
        let value = to_vec(&model).unwrap();
        self.put(&key, &value)
    }

    fn get_tracker(
        &self,
        node_id: PublicKey,
        validator_factory: Arc<dyn ValidatorFactory>,
    ) -> Result<ChainTracker<ChainMonitor>, Error> {
        let key = make_key(NODE_TRACKER_PREFIX, &node_id.serialize());
        let value = self.get(&key)?.expect("tracker not found").1;
        let model: ChainTrackerEntry = from_slice(&value).unwrap();
        Ok(model.into_tracker(node_id.clone(), validator_factory))
    }

    fn update_channel(&self, node_id: &PublicKey, channel: &Channel) -> Result<(), Error> {
        let key = make_key2(CHANNEL_PREFIX, &node_id.serialize(), channel.id0.as_slice());

        let channel_value_satoshis = channel.setup.channel_value_sat;
        let entry = ChannelEntry {
            channel_value_satoshis,
            channel_setup: Some(channel.setup.clone()),
            id: channel.id.clone(),
            enforcement_state: channel.enforcement_state.clone(),
            blockheight: None,
        };
        let value = to_vec(&entry).unwrap();
        self.put(&key, &value)
    }

    fn get_channel(
        &self,
        node_id: &PublicKey,
        channel_id: &ChannelId,
    ) -> Result<CoreChannelEntry, Error> {
        let key = make_key2(CHANNEL_PREFIX, &node_id.serialize(), channel_id.as_slice());
        let value = self.get(&key)?.expect("channel not found").1;
        let entry: ChannelEntry = from_slice(&value).unwrap();
        Ok(entry.into())
    }

    fn get_node_channels(
        &self,
        node_id: &PublicKey,
    ) -> Result<Vec<(ChannelId, CoreChannelEntry)>, Error> {
        let prefix = make_key(CHANNEL_PREFIX, &node_id.serialize()) + SEPARATOR;
        let mut res = Vec::new();
        for (key, (_version, value)) in self.get_prefix(&prefix)? {
            let suffix = hex::decode(&key[prefix.len()..]).expect("invalid hex in key suffix");
            let channel_id = ChannelId::new(&suffix);
            let entry: ChannelEntry = from_slice(&value).unwrap();
            res.push((channel_id, entry.into()));
        }
        Ok(res)
    }

    fn update_node_allowlist(
        &self,
        node_id: &PublicKey,
        allowlist: Vec<String>,
    ) -> Result<(), Error> {
        let key = make_key(ALLOWLIST_PREFIX, &node_id.serialize());
        let entry = AllowlistItemEntry { allowlist };
        let value = to_vec(&entry).unwrap();
        self.put(&key, &value)
    }

    fn get_node_allowlist(&self, node_id: &PublicKey) -> Result<Vec<String>, Error> {
        let key = make_key(ALLOWLIST_PREFIX, &node_id.serialize());
        let value = self.get(&key)?.expect("allowlist not found").1;
        let entry: AllowlistItemEntry = from_slice(&value).unwrap();
        Ok(entry.allowlist)
    }

    fn get_nodes(&self) -> Result<Vec<(PublicKey, CoreNodeEntry)>, Error> {
        let prefix = NODE_ENTRY_PREFIX.to_string() + SEPARATOR;
        let mut res = Vec::new();
        self.get_prefix(&prefix)?
            .into_iter()
            .filter(|(_k, (_r, value))| !value.is_empty())
            .for_each(|(key, (_r, value))| {
                let suffix = hex::decode(&key[prefix.len()..]).expect("invalid hex in key suffix");
                let node_id = PublicKey::from_slice(&suffix).unwrap();
                let entry: NodeEntry = from_slice(&value).unwrap();
                let state_value = self
                    .get(&make_key(NODE_STATE_PREFIX, &node_id.serialize()))
                    .unwrap()
                    .unwrap()
                    .1;
                let state_entry: NodeStateEntry = from_slice(&state_value).unwrap();
                let state = NodeState::restore(
                    state_entry.invoices,
                    state_entry.issued_invoices,
                    state_entry.preimages,
                    0,
                    state_entry.velocity_control.into(),
                    state_entry.fee_velocity_control.into(),
                );
                let node_entry = CoreNodeEntry {
                    key_derivation_style: entry.key_derivation_style as u8,
                    network: entry.network,
                    state,
                };
                res.push((node_id, node_entry));
            });
        Ok(res)
    }

    fn clear_database(&self) -> Result<(), Error> {
        // delegate to the underlying store
        self.0.clear_database()
    }
}

fn make_key(prefix: impl Into<String>, key: &[u8]) -> String {
    format!("{}/{}", prefix.into(), hex::encode(key))
}

fn make_key2(prefix: impl Into<String>, key1: &[u8], key2: &[u8]) -> String {
    format!("{}/{}/{}", prefix.into(), hex::encode(key1), hex::encode(key2))
}
