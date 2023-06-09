use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bitcoind_client::{BitcoindClient, BlockSource, BlockchainInfo};
use core::result::Result as CoreResult;
use lightning_signer::bitcoin::consensus::deserialize;
use lightning_signer::bitcoin::hashes::hex::FromHex;
use lightning_signer::bitcoin::hashes::Hash;
use lightning_signer::bitcoin::secp256k1::{All, PublicKey, Secp256k1, SecretKey};
use lightning_signer::bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey};
use lightning_signer::bitcoin::{Block, BlockHash, BlockHeader, FilterHeader, KeyPair, Network};
use lightning_signer::chain::tracker::{ChainTracker, Headers};
use lightning_signer::node::{Heartbeat, SignedHeartbeat};
use lightning_signer::policy::simple_validator::SimpleValidatorFactory;
use lightning_signer::txoo::filter::BlockSpendFilter;
use lightning_signer::txoo::proof::UnspentProof;
use lightning_signer::util::crypto_utils::sighash_from_heartbeat;
use lightning_signer::util::test_utils::MockListener;
use log::{error, info};
use serde_json::{json, Value};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;
use url::Url;
use vls_frontend::frontend::SourceFactory;
use vls_frontend::Frontend;
use vls_protocol::model;
use vls_protocol::msgs;
use vls_protocol::msgs::{
    AddBlockReply, ForwardWatchesReply, GetHeartbeatReply, Message, NodeInfoReply, SerBolt,
    TipInfoReply,
};
use vls_protocol::serde_bolt::{to_vec, Octets, WireString};
use vls_protocol_client::{ClientResult, SignerPort};
use vls_proxy::portfront::SignerPortFront;
use vls_proxy::util::setup_logging;

struct State {
    height: u32,
    block_hash: BlockHash,
    tracker: ChainTracker<MockListener>,
}

#[derive(Clone)]
struct DummySignerPort {
    secp: Secp256k1<All>,
    node_id: PublicKey,
    xpriv: ExtendedPrivKey,
    xpub: ExtendedPubKey,
    state: Arc<Mutex<State>>,
}

const NODE_SECRET: [u8; 32] = [3u8; 32];

impl DummySignerPort {
    fn new(height: u32, block: Block) -> Self {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&NODE_SECRET).unwrap();
        let node_id = PublicKey::from_secret_key(&secp, &secret_key);
        let network = Network::Regtest;
        let xpriv = ExtendedPrivKey::new_master(network, &[0; 32]).unwrap();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);
        let block_hash = block.block_hash();
        let filter = BlockSpendFilter::from_block(&block);
        let filter_header = filter.filter_header(&FilterHeader::all_zeros());
        let tip = Headers(block.header.clone(), filter_header);
        let validator_factory = SimpleValidatorFactory::new();
        let tracker =
            ChainTracker::new(network, height, tip, node_id, Arc::new(validator_factory)).unwrap();

        let state = State { height, block_hash, tracker };

        Self { secp, node_id, xpriv, xpub, state: Arc::new(Mutex::new(state)) }
    }

    fn assert_state(&self, height: u32, block_hash: BlockHash) -> bool {
        let state = self.state.lock().unwrap();
        state.height == height && state.block_hash == block_hash
    }

    fn height(&self) -> u32 {
        let state = self.state.lock().unwrap();
        state.height
    }
}

#[async_trait]
impl SignerPort for DummySignerPort {
    async fn handle_message(&self, message_bytes: Vec<u8>) -> ClientResult<Vec<u8>> {
        let message = msgs::from_vec(message_bytes).unwrap();
        match message {
            Message::NodeInfo(_) => {
                let reply = NodeInfoReply {
                    network_name: WireString("regtest".as_bytes().to_vec()),
                    node_id: model::PubKey(self.node_id.serialize()),
                    bip32: model::ExtKey(self.xpub.encode()),
                };
                Ok(reply.as_vec())
            }
            Message::TipInfo(_) => {
                let state = self.state.lock().unwrap();
                let reply = TipInfoReply {
                    height: state.height,
                    block_hash: model::BlockHash(state.block_hash.as_hash().into_inner()),
                };
                Ok(reply.as_vec())
            }
            Message::ForwardWatches(_) => {
                let reply = ForwardWatchesReply { txids: vec![], outpoints: vec![] };
                Ok(reply.as_vec())
            }
            Message::AddBlock(add) => {
                let mut state = self.state.lock().unwrap();
                state.height += 1;
                let header: BlockHeader = deserialize(&add.header.0).unwrap();
                let proof: UnspentProof = deserialize(&add.unspent_proof.unwrap().0).unwrap();
                state.tracker.add_block(header, proof).expect("add block failed");
                state.block_hash = header.block_hash();
                let reply = AddBlockReply {};
                Ok(reply.as_vec())
            }
            Message::GetHeartbeat(_) => {
                let state = self.state.lock().unwrap();
                let heartbeat = Heartbeat {
                    chain_tip: state.block_hash,
                    chain_height: state.height,
                    chain_timestamp: 0,
                    current_timestamp: 0,
                };
                let kp = KeyPair::from_secret_key(&self.secp, &self.xpriv.private_key);
                let ser_heartbeat = heartbeat.encode();
                let msg = sighash_from_heartbeat(&ser_heartbeat);
                let sig = self.secp.sign_schnorr_no_aux_rand(&msg, &kp);
                let signed_heartbeat = SignedHeartbeat { signature: sig[..].to_vec(), heartbeat };

                let reply =
                    GetHeartbeatReply { heartbeat: Octets(to_vec(&signed_heartbeat).unwrap()) };
                Ok(reply.as_vec())
            }
            m => {
                panic!("unhandled {:?}", m);
            }
        }
    }

    fn clone(&self) -> Box<dyn SignerPort> {
        Box::new(Clone::clone(self))
    }
}

async fn await_until<EF>(mut f: impl FnMut() -> bool, err_f: EF) -> Result<()>
where
    EF: FnOnce(),
{
    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();
    while !f() {
        if start.elapsed() > timeout {
            err_f();
            return Err(anyhow!("await_until timeout"));
        }
        sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    env::set_var("VLS_CHAINFOLLOWER_ENABLE", "1");
    setup_logging(tmpdir.path().to_str().unwrap(), "system-test", "debug");
    let network = Network::Regtest;
    let url: Url = "http://user:pass@127.0.0.1:18443".parse()?;
    let client = BitcoindClient::new(url.clone()).await;
    let info = get_info(&client).await?;
    let mut height = info.latest_height as u32;

    // ignore error, might already exist
    let _: CoreResult<Value, _> = client.call("createwallet", &[json!("default")]).await;

    let address: String = client.call("getnewaddress", &[]).await?;

    info!("mine to {}", address);
    let block_hash = mine(&client, &address, 1).await?;
    height += 1;

    let genesis_hash = client.get_block_hash(0).await?.unwrap();
    let genesis = client.get_block(&genesis_hash).await.unwrap();

    let signer_port = DummySignerPort::new(0, genesis);
    let source_factory = Arc::new(SourceFactory::new(tmpdir.path(), network));
    let frontend = Frontend::new(
        Arc::new(SignerPortFront::new(SignerPort::clone(&signer_port), network)),
        source_factory,
        url,
    );
    frontend.start();

    await_until(
        || signer_port.assert_state(height, block_hash),
        || error!("signer at height {} vs {}", signer_port.height(), height),
    )
    .await?;

    height += 1;
    info!("mine height {}", height);
    let block_hash = mine(&client, &address, 1).await?;

    await_until(
        || signer_port.assert_state(height, block_hash),
        || error!("signer at height {} vs {}", signer_port.height(), height),
    )
    .await?;
    Ok(())
}

async fn mine(client: &BitcoindClient, address: &str, blocks: u32) -> Result<BlockHash> {
    let hashes: Value = client.call("generatetoaddress", &[json!(blocks), json!(address)]).await?;
    Ok(BlockHash::from_hex(&hashes[0].as_str().unwrap())?)
}

async fn get_info(client: &BitcoindClient) -> Result<BlockchainInfo> {
    let info = client.get_blockchain_info().await?;
    println!("{:?}", info);
    Ok(info)
}
