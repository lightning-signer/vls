use hex::FromHex;
use lightning_signer::node::{Node, NodeConfig, NodeServices};
use lightning_signer::persist::Persist;
use lightning_signer::policy::simple_validator::SimpleValidatorFactory;
use lightning_signer::util::clock::StandardClock;
use lightning_signer::util::test_utils::{
    init_channel, make_current_test_invoice, make_genesis_starting_time_factory,
    make_test_channel_setup, TEST_NODE_CONFIG, TEST_SEED,
};
use std::env::args;
use std::sync::Arc;
use vls_persist::kv_json::KVJsonPersister;

pub fn init_node(
    node_config: NodeConfig,
    seedstr: &str,
    path: &str,
) -> (Arc<Node>, Arc<KVJsonPersister<'static>>) {
    let mut seed = [0; 32];
    seed.copy_from_slice(Vec::from_hex(seedstr).unwrap().as_slice());

    let persister = Arc::new(KVJsonPersister::new(path));
    let validator_factory = Arc::new(SimpleValidatorFactory::new());
    let starting_time_factory = make_genesis_starting_time_factory(node_config.network);
    let clock = Arc::new(StandardClock());
    let services = NodeServices {
        validator_factory,
        starting_time_factory,
        persister: persister.clone(),
        clock,
    };

    let node = Node::new(node_config, &seed, vec![], services);
    persister.new_node(&node.get_id(), &node_config, &node.get_state()).unwrap();
    (Arc::new(node), persister)
}

fn main() {
    let path = args().nth(1).unwrap();
    let (node, _persister) = init_node(TEST_NODE_CONFIG, TEST_SEED[0], &path);
    node.add_allowlist(&["address:mv4rnyY3Su5gjcDNzbMLKBQkBicCtHUtFB".to_string()]).unwrap();
    init_channel(make_test_channel_setup(), node.clone());
    // add an invoice
    let invoice = make_current_test_invoice(1, 123);
    node.add_invoice(invoice).unwrap();
}
