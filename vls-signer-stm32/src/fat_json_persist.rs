use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::str::FromStr;

use serde_json::json;

use fatfs::{Read, Write};

use log::*;

use vls_protocol_signer::lightning_signer;

use lightning_signer::{
    bitcoin::secp256k1::PublicKey,
    chain::tracker::ChainTracker,
    channel::{Channel, ChannelId, ChannelStub},
    monitor::ChainMonitor,
    node::{NodeConfig, NodeState as CoreNodeState},
    policy::validator::EnforcementState,
    prelude::*,
};

use lightning_signer::persist::{
    self,
    model::{ChannelEntry as CoreChannelEntry, NodeEntry as CoreNodeEntry},
    Persist,
};
use vls_persist::model::{
    AllowlistItemEntry, ChainTrackerEntry, ChannelEntry, NodeEntry, NodeStateEntry,
};

use crate::setup::SetupFS;

const STATE_PATH: &str = "STATE";
const NODE_BUCKET_PATH: &str = "NODE";
const NODESTATE_BUCKET_PATH: &str = "NODESTATE";
const CHANNEL_BUCKET_PATH: &str = "CHANNEL";
const ALLOWLIST_BUCKET_PATH: &str = "ALLOWLIST";
const CHAINTRACKER_BUCKET_PATH: &str = "CHAINTRACKER";

const BUFSZ: usize = 128;

struct Error(persist::Error);

impl From<fatfs::Error<()>> for Error {
    fn from(err: fatfs::Error<()>) -> Self {
        match err {
            fatfs::Error::NotFound => Error(persist::Error::NotFound(format!("{:?}", err))),
            fatfs::Error::AlreadyExists =>
                Error(persist::Error::AlreadyExists(format!("{:?}", err))),
            _ => Error(persist::Error::Internal(format!("{:?}", err))),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error(persist::Error::Internal(format!("serde_json::Error: {:?}", err)))
    }
}

impl Into<persist::Error> for Error {
    fn into(self) -> persist::Error {
        self.0
    }
}

pub struct FatJsonPersister {
    setupfs: Arc<RefCell<SetupFS>>,
    node_id: PublicKey,
}

impl SendSync for FatJsonPersister {}

impl FatJsonPersister {
    pub fn new(setupfs: Arc<RefCell<SetupFS>>) -> Self {
        let node_id = PublicKey::from_str(&setupfs.borrow().runpath()).expect("node_id string");
        let ss = Self { setupfs, node_id };
        ss.ensure_path(&Self::state_path());
        ss.ensure_path(&Self::node_bucket_path());
        ss.ensure_path(&Self::nodestate_bucket_path());
        ss.ensure_path(&Self::channel_bucket_path());
        ss.ensure_path(&Self::allowlist_bucket_path());
        ss.ensure_path(&Self::chaintracker_bucket_path());
        ss
    }

    fn ensure_path(&self, path: &String) {
        self.setupfs
            .borrow()
            .rundir()
            .create_dir(&path.as_str())
            .unwrap_or_else(|err| panic!("create_dir {} failed: {:#?}", path, err));
    }

    fn state_path() -> String {
        STATE_PATH.to_string()
    }
    fn node_bucket_path() -> String {
        format!("{}/{}", STATE_PATH, NODE_BUCKET_PATH)
    }
    fn nodestate_bucket_path() -> String {
        format!("{}/{}", STATE_PATH, NODESTATE_BUCKET_PATH)
    }
    fn channel_bucket_path() -> String {
        format!("{}/{}", STATE_PATH, CHANNEL_BUCKET_PATH)
    }
    fn allowlist_bucket_path() -> String {
        format!("{}/{}", STATE_PATH, ALLOWLIST_BUCKET_PATH)
    }
    fn chaintracker_bucket_path() -> String {
        format!("{}/{}", STATE_PATH, CHAINTRACKER_BUCKET_PATH)
    }

    fn node_key() -> String {
        "node".to_string()
    }

    fn nodestate_key() -> String {
        "nodestate".to_string()
    }

    fn channel_key(chanid: &[u8]) -> String {
        hex::encode(chanid)
    }

    fn allowlist_key() -> String {
        "allowlist".to_string()
    }

    fn chaintracker_key() -> String {
        "chaintracker".to_string()
    }

    // Update or create a value in a bucket
    fn upsert_value(&self, bucket: &str, key: &str, value: &str) -> Result<(), Error> {
        self.write_new_file(bucket, key, value)?;
        self.retire_cur_file(bucket, key).ok(); // ok if it's not there
        self.replace_file(bucket, key)?;
        self.expunge_old_file(bucket, key).ok(); // ok if it's not there
        Ok(())
    }

    // Create a value in a bucket, fail if AlreadyExists
    fn insert_value(&self, bucket: &str, key: &str, value: &str) -> Result<(), Error> {
        self.write_new_file(bucket, key, value)?;
        self.replace_file(bucket, key)?;
        Ok(())
    }

    // Update a value in a bucket, fail if NotFound
    fn update_value(&self, bucket: &str, key: &str, value: &str) -> Result<(), Error> {
        self.write_new_file(bucket, key, value)?;
        self.retire_cur_file(bucket, key)?;
        self.replace_file(bucket, key)?;
        self.expunge_old_file(bucket, key)?;
        Ok(())
    }

    // Read a value from a bucket
    fn read_value(&self, bucket: &str, key: &str) -> Result<String, Error> {
        let setupfs = self.setupfs.borrow();
        let file_path = format!("{}/{}", bucket, key);
        let mut file = setupfs.rundir().open_file(&file_path)?;
        let mut value: Vec<u8> = vec![];
        let mut buffer = [0u8; BUFSZ];
        loop {
            let len = file.read(&mut buffer)?;
            if len == 0 {
                break;
            }
            value.extend_from_slice(&buffer[..len]);
        }
        Ok(String::from_utf8_lossy(&value).to_string())
    }

    // List all possible keys in a bucket
    fn list_keys(&self, bucket: &str) -> Result<Vec<String>, Error> {
        let setupfs = self.setupfs.borrow();
        let bucket_dir = setupfs.rundir().open_dir(bucket)?;
        Ok(bucket_dir
            .iter()
            .filter_map(|de| {
                let name = de.unwrap().file_name();
                if name != "." && name != ".." {
                    Some(name)
                } else {
                    None
                }
            })
            .collect())
    }

    fn write_new_file(&self, bucket: &str, key: &str, value: &str) -> Result<(), Error> {
        let setupfs = self.setupfs.borrow();
        let new_file_path = format!("{}/{}.new", bucket, key);
        let mut new_file = setupfs.rundir().create_file(&new_file_path)?;
        new_file.write(value.as_bytes())?;
        new_file.flush()?;
        Ok(())
    }

    fn retire_cur_file(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let setupfs = self.setupfs.borrow();
        let bucket_dir = setupfs.rundir().open_dir(bucket)?;
        let cur_file_path = format!("{}", key);
        let old_file_path = format!("{}.old", cur_file_path);
        bucket_dir.rename(&cur_file_path, &bucket_dir, &old_file_path)?;
        Ok(())
    }

    fn replace_file(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let setupfs = self.setupfs.borrow();
        let bucket_dir = setupfs.rundir().open_dir(bucket)?;
        let cur_file_path = format!("{}", key);
        let new_file_path = format!("{}.new", cur_file_path);
        bucket_dir.rename(&new_file_path, &bucket_dir, &cur_file_path)?;
        Ok(())
    }

    fn expunge_old_file(&self, bucket: &str, key: &str) -> Result<(), Error> {
        let old_file_path = format!("{}/{}.old", bucket, key);
        self.setupfs.borrow().rundir().remove(&old_file_path)?;
        Ok(())
    }
}

impl Persist for FatJsonPersister {
    fn new_node(
        &self,
        _node_id: &PublicKey,
        config: &NodeConfig,
        state: &CoreNodeState,
    ) -> Result<(), persist::Error> {
        {
            let key = Self::node_key();
            let entry = NodeEntry {
                key_derivation_style: config.key_derivation_style as u8,
                network: config.network.to_string(),
            };
            let value = json!(entry).to_string();
            self.insert_value(&Self::node_bucket_path(), &key, &value).map_err(|err| err.into())?;
        }

        {
            let key = Self::nodestate_key();
            let state_entry: NodeStateEntry = state.into();
            let state_value = json!(state_entry).to_string();
            self.upsert_value(&Self::nodestate_bucket_path(), &key, &state_value)
                .map_err(|err| err.into())?;
        }

        Ok(())
    }

    #[allow(unused)]
    fn update_node(
        &self,
        node_id: &PublicKey,
        state: &CoreNodeState,
    ) -> Result<(), persist::Error> {
        let key = Self::nodestate_key();
        let state_entry: NodeStateEntry = state.into();
        let state_value = json!(state_entry).to_string();
        self.update_value(&Self::nodestate_bucket_path(), &key, &state_value)
            .map_err(|err| err.into())?;
        Ok(())
    }

    #[allow(unused)]
    fn delete_node(&self, node_id: &PublicKey) -> Result<(), persist::Error> {
        unimplemented!();
    }

    fn new_chain_tracker(
        &self,
        _node_id: &PublicKey,
        tracker: &ChainTracker<ChainMonitor>,
    ) -> Result<(), persist::Error> {
        let key = Self::chaintracker_key();
        let entry: ChainTrackerEntry = tracker.into();
        let value = json!(entry).to_string();
        self.upsert_value(&Self::chaintracker_bucket_path(), &key, &value).map_err(|err| err.into())
    }

    fn update_tracker(
        &self,
        _node_id: &PublicKey,
        tracker: &ChainTracker<ChainMonitor>,
    ) -> Result<(), persist::Error> {
        let key = Self::chaintracker_key();
        let entry: ChainTrackerEntry = tracker.into();
        let value = json!(entry).to_string();
        self.update_value(&Self::chaintracker_bucket_path(), &key, &value).map_err(|err| err.into())
    }

    fn get_tracker(
        &self,
        _node_id: &PublicKey,
    ) -> Result<ChainTracker<ChainMonitor>, persist::Error> {
        let key = Self::chaintracker_key();
        let entry: ChainTrackerEntry = serde_json::from_str(
            &self.read_value(&Self::chaintracker_bucket_path(), &key).map_err(|err| err.into())?,
        )
        .map_err(|err| persist::Error::Internal(format!("serde_json failed: {:?}", err)))?;
        Ok(entry.into())
    }

    fn new_channel(&self, _node_id: &PublicKey, stub: &ChannelStub) -> Result<(), persist::Error> {
        let key = Self::channel_key(&stub.id0.inner());
        info!("new_channel: {}", key);
        let channel_value_satoshis = 0; // TODO not known yet
        let entry = ChannelEntry {
            channel_value_satoshis,
            channel_setup: None,
            id: None,
            enforcement_state: EnforcementState::new(0),
        };
        let value = json!(entry).to_string();
        self.insert_value(&Self::channel_bucket_path(), &key, &value).map_err(|err| err.into())
    }

    fn update_channel(
        &self,
        _node_id: &PublicKey,
        channel: &Channel,
    ) -> Result<(), persist::Error> {
        let key = Self::channel_key(&channel.id0.inner());
        info!("update_channel: {}", key);
        let channel_value_satoshis = channel.setup.channel_value_sat;
        let entry = ChannelEntry {
            channel_value_satoshis,
            channel_setup: Some(channel.setup.clone()),
            id: channel.id.clone(),
            enforcement_state: channel.enforcement_state.clone(),
        };
        let value = json!(entry).to_string();
        self.update_value(&Self::channel_bucket_path(), &key, &value).map_err(|err| err.into())
    }

    #[allow(unused)]
    fn get_channel(
        &self,
        node_id: &PublicKey,
        channel_id: &ChannelId,
    ) -> Result<CoreChannelEntry, persist::Error> {
        unimplemented!();
    }

    fn get_node_channels(
        &self,
        _node_id: &PublicKey,
    ) -> Result<Vec<(ChannelId, CoreChannelEntry)>, persist::Error> {
        info!("get_node_channels");
        let mut res = vec![];
        for key in self.list_keys(&Self::channel_bucket_path()).map_err(|err| err.into())? {
            // skip entries which are not valid hex
            if let Ok(bytes) = hex::decode(&key) {
                let entry: ChannelEntry = serde_json::from_str(
                    &self
                        .read_value(&Self::channel_bucket_path(), &key)
                        .map_err(|err| err.into())?,
                )
                .map_err(|err| persist::Error::Internal(format!("serde_json failed: {:?}", err)))?;
                res.push((ChannelId::new(&bytes[..]), entry.into()))
            }
        }
        Ok(res)
    }

    fn update_node_allowlist(
        &self,
        _node_id: &PublicKey,
        allowlist: Vec<String>,
    ) -> Result<(), persist::Error> {
        let key = Self::allowlist_key();
        let entry = AllowlistItemEntry { allowlist };
        let value = json!(entry).to_string();
        self.upsert_value(&Self::allowlist_bucket_path(), &key, &value).map_err(|err| err.into())
    }

    fn get_node_allowlist(&self, _node_id: &PublicKey) -> Result<Vec<String>, persist::Error> {
        let key = Self::allowlist_key();
        let entry: AllowlistItemEntry = serde_json::from_str(
            &self.read_value(&Self::allowlist_bucket_path(), &key).map_err(|err| err.into())?,
        )
        .map_err(|err| persist::Error::Internal(format!("serde_json failed: {:?}", err)))?;
        Ok(entry.allowlist)
    }

    fn get_nodes(&self) -> Result<Vec<(PublicKey, CoreNodeEntry)>, persist::Error> {
        info!("get_nodes");
        let mut res = vec![];
        let key = Self::node_key();
        if let Ok(nodevalue) = &self.read_value(&Self::node_bucket_path(), &key) {
            let e: NodeEntry = serde_json::from_str(&nodevalue)
                .map_err(|err| persist::Error::Internal(format!("serde_json failed: {:?}", err)))?;
            let skey = Self::nodestate_key();
            let state_e: NodeStateEntry = serde_json::from_str(
                &self
                    .read_value(&Self::nodestate_bucket_path(), &skey)
                    .map_err(|err| err.into())?,
            )
            .map_err(|err| persist::Error::Internal(format!("serde_json failed: {:?}", err)))?;
            let state = CoreNodeState {
                invoices: Default::default(),
                issued_invoices: Default::default(),
                payments: Default::default(),
                excess_amount: 0,
                log_prefix: "".to_string(),
                velocity_control: state_e.velocity_control.into(),
            };
            let entry = CoreNodeEntry {
                key_derivation_style: e.key_derivation_style,
                network: e.network,
                state,
            };
            res.push((self.node_id.clone(), entry));
        }
        Ok(res)
    }

    fn clear_database(&self) -> Result<(), persist::Error> {
        unimplemented!();
    }
}
