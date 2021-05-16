use crate::node::node::{ChannelId, ChannelSetup};

use crate::util::enforcing_trait_impls::EnforcementState;

pub struct NodeEntry {
    pub seed: Vec<u8>,
    pub key_derivation_style: u8,
    pub network: String,
}

#[derive(Debug)]
pub struct ChannelEntry {
    pub nonce: Vec<u8>,
    pub channel_value_satoshis: u64,
    pub channel_setup: Option<ChannelSetup>,
    // Permanent channel ID if different from the initial channel ID
    pub id: Option<ChannelId>,
    pub enforcement_state: EnforcementState,
}