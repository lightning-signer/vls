use core::fmt;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::{Network, Transaction};
use bitcoin_hashes::core::fmt::{Error, Formatter};
use lightning::chain::keysinterface::{ChannelKeys, KeysInterface, KeysManager};
use lightning::ln::chan_utils::{ChannelPublicKeys, HTLCOutputInCommitment, TxCreationKeys};
use lightning::ln::msgs::UnsignedChannelAnnouncement;
use lightning::util::logger::Logger;
use rand::{Rng, thread_rng};
use secp256k1::{All, PublicKey, Secp256k1, SecretKey, Signature};

use crate::util::enforcing_trait_impls::EnforcingChannelKeys;
use crate::util::test_utils::TestLogger;

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct ChannelId([u8; 32]);

impl Debug for ChannelId {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.write_str(hex::encode(self.0).as_str())
    }
}

pub struct Channel {
    pub keys: EnforcingChannelKeys,
    pub secp_ctx: Secp256k1<All>,
}

impl Debug for Channel {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("channel")
    }
}

impl Channel {
    fn make_tx_keys(&self, per_commitment_point: &PublicKey) -> TxCreationKeys {
        let inner = &self.keys.inner;
        let revocation_base = PublicKey::from_secret_key(&self.secp_ctx, &inner.revocation_base_key);
        let payment_base = PublicKey::from_secret_key(&self.secp_ctx, &inner.payment_base_key);
        let htlc_base = PublicKey::from_secret_key(&self.secp_ctx, &inner.htlc_base_key);

        let remote_points = inner.remote_channel_pubkeys.as_ref().unwrap();

        TxCreationKeys::new(&self.secp_ctx,
                            &per_commitment_point,
                            &remote_points.delayed_payment_basepoint,
                            &remote_points.htlc_basepoint,
                            &revocation_base,
                            &payment_base,
                            &htlc_base).unwrap()
    }

    pub fn sign_remote_commitment(&self, feerate_per_kw: u64, commitment_tx: &Transaction,
                                  per_commitment_point: &PublicKey, htlcs: &[&HTLCOutputInCommitment],
                                  to_self_delay: u16) -> Result<(Signature, Vec<Signature>), ()> {
        let tx_keys = self.make_tx_keys(per_commitment_point);
        self.keys.sign_remote_commitment(feerate_per_kw, commitment_tx, &tx_keys, htlcs, to_self_delay, &self.secp_ctx)
    }

    pub fn sign_channel_announcement(&self, msg: &UnsignedChannelAnnouncement) -> Result<Signature, ()> {
        self.keys.sign_channel_announcement(msg, &self.secp_ctx)
    }

    pub fn accept(&mut self, channel_points: &ChannelPublicKeys) {
        self.keys.set_remote_channel_pubkeys(channel_points);
    }
}

pub struct Node {
    pub keys_manager: KeysManager,
    channels: Mutex<HashMap<ChannelId, Channel>>,
}

impl Node {
    pub fn get_node_secret(&self) -> SecretKey {
        self.keys_manager.get_node_secret()
    }
}

impl Debug for Node {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("node")
    }
}

pub struct MySigner {
    pub logger: Arc<Logger>,
    nodes: Mutex<HashMap<PublicKey, Node>>,
}

impl MySigner {
    pub fn new() -> MySigner {
        let test_logger = Arc::new(TestLogger::with_id("server".to_owned()));
        let logger = Arc::clone(&test_logger) as Arc<Logger>;
        let signer = MySigner {
            logger: test_logger,
            nodes: Mutex::new(HashMap::new()),
        };
        log_info!(signer, "new MySigner");
        signer
    }

    pub fn new_node(&self) -> PublicKey {
        let secp_ctx = Secp256k1::signing_only();
        let network = Network::Testnet;
        let mut rng = thread_rng();

        let mut seed = [0; 32];
        rng.fill_bytes(&mut seed);

        let logger = Arc::clone(&self.logger);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards");
        let node = Node {
            keys_manager: KeysManager::new(&seed, network, logger, now.as_secs(), now.subsec_nanos()),
            channels: Mutex::new(HashMap::new()),
        };
        let node_id = PublicKey::from_secret_key(&secp_ctx, &node.keys_manager.get_node_secret());
        let mut nodes = self.nodes.lock().unwrap();
        nodes.insert(node_id, node);
        node_id
    }

    pub fn new_channel(&self, node_id: &PublicKey, channel_value_satoshi: u64) -> Result<ChannelId, ()> {
        let nodes = self.nodes.lock().unwrap();
        let node = match nodes.get(node_id) {
            Some(n) => n,
            None => {
                log_error!(self, "no such node {}", node_id);
                return Err(());
            }
        };
        let mut channels = node.channels.lock().unwrap();
        let keys_manager = &node.keys_manager;
        let channel_id = ChannelId(keys_manager.get_channel_id());
        if channels.contains_key(&channel_id) {
            log_error!(self, "already have channel ID {:?}", channel_id);
            return Err(());
        }
        let unused_inbound_flag = false;
        let chan_keys =
            EnforcingChannelKeys::new(keys_manager.get_channel_keys(unused_inbound_flag, channel_value_satoshi));
        let channel = Channel {
            keys: chan_keys,
            secp_ctx: Secp256k1::new(),
        };
        channels.insert(channel_id, channel);
        Ok(channel_id)
    }

    pub fn with_node<F: Sized, T, E>(&self, node_id: &PublicKey, f: F) -> Result<T, E>
        where F: Fn(Option<&Node>) -> Result<T, E> {
        let nodes = self.nodes.lock().unwrap();
        let node = nodes.get(node_id);
        f(node)
    }

    pub fn with_channel<F: Sized, T, E>(&self, node_id: &PublicKey,
                                        channel_id: &ChannelId,
                                        f: F) -> Result<T, E>
        where F: Fn(Option<&mut Channel>) -> Result<T, E> {
        let nodes = self.nodes.lock().unwrap();
        let node = nodes.get(node_id);
        node.map_or_else(|| f(None), |n| {
            f(n.channels.lock().unwrap().get_mut(channel_id))
        })
    }

    pub fn with_channel_do<F: Sized, T>(&self, node_id: &PublicKey,
                                        channel_id: &ChannelId,
                                        f: F) -> T
        where F: Fn(Option<&mut Channel>) -> T {
        let nodes = self.nodes.lock().unwrap();
        let node = nodes.get(node_id);
        node.map_or_else(|| f(None), |n| {
            f(n.channels.lock().unwrap().get_mut(channel_id))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::util::test_utils::*;

    use super::*;

    #[test]
    fn new_channel_test() -> Result<(), ()> {
        let signer = MySigner::new();
        let node_id = signer.new_node();
        let channel_id = signer.new_channel(&node_id, 1000)?;
        signer.with_node(&node_id, |node| {
            assert!(node.is_some());
            Ok(())
        })?;
        signer.with_channel(&node_id, &channel_id, |chan| {
            assert!(chan.is_some());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn bad_channel_lookup_test() -> Result<(), ()> {
        let signer = MySigner::new();
        let node_id = signer.new_node();
        let channel_id = ChannelId([1; 32]);
        signer.with_channel(&node_id, &channel_id, |chan| {
            assert!(chan.is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn bad_node_lookup_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let signer = MySigner::new();
        let node_id = pubkey_from_secret_hex("0101010101010101010101010101010101010101010101010101010101010101", &secp_ctx);

        let channel_id = ChannelId([1; 32]);
        signer.with_channel(&node_id, &channel_id, |chan| {
            assert!(chan.is_none());
            Ok(())
        })?;

        signer.with_node(&node_id, |node| {
            assert!(node.is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn new_channel_bad_node_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let signer = MySigner::new();
        let node_id = pubkey_from_secret_hex("0101010101010101010101010101010101010101010101010101010101010101", &secp_ctx);
        assert!(signer.new_channel(&node_id, 1000).is_err());
        Ok(())
    }
}