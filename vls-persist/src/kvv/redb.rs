use crate::kvv::KVVStore;
use lightning_signer::persist::Error;
use lightning_signer::SendSync;
use redb::{Database, ReadableTable, TableDefinition};
use std::collections::BTreeMap;
use std::convert::TryInto;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");

pub struct RedbKVVStore {
    db: Database,
    versions: BTreeMap<String, u64>,
}

impl SendSync for RedbKVVStore {}

impl RedbKVVStore {
    pub fn new(path: &str) -> Self {
        let mut db = Database::create(path).unwrap();
        db.check_integrity().expect("database integrity check failed");
        let mut versions = BTreeMap::new();
        {
            // create the table if it doesn't exist
            let tx = db.begin_write().unwrap();
            tx.open_table(TABLE).unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.begin_read().unwrap();
            let table = tx.open_table(TABLE).unwrap();
            for item in table.iter().unwrap() {
                let (key, vv) = item.expect("failed to iterate");
                let (version, _) = Self::from_vv(vv.value());
                versions.insert(key.value().to_string(), version);
            }
        }

        Self { db, versions }
    }

    fn from_vv(vv: &[u8]) -> (u64, Vec<u8>) {
        let version = u64::from_be_bytes(vv[..8].try_into().unwrap());
        let value = vv[8..].to_vec();
        (version, value)
    }
}

impl KVVStore for RedbKVVStore {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        let version = self.versions.get(key).map(|v| v + 1).unwrap_or(0);
        self.put_with_version(key, version, value)
    }

    fn put_with_version(&self, key: &str, version: u64, value: &[u8]) -> Result<(), Error> {
        if let Some(v) = self.versions.get(key) {
            if version != v + 1 {
                return Err(Error::VersionMismatch);
            }
        } else if version != 0 {
            return Err(Error::VersionMismatch);
        }
        let tx = self.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(TABLE).unwrap();
            let mut vv = Vec::with_capacity(value.len() + 8);
            vv.extend_from_slice(&version.to_be_bytes());
            vv.extend_from_slice(value);
            table.insert(key, vv.as_slice()).expect("failed to insert");
        }
        tx.commit().unwrap();
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<(u64, Vec<u8>)>, Error> {
        let tx = self.db.begin_read().unwrap();
        let table = tx.open_table(TABLE).unwrap();
        let result = table.get(key).expect("failed to get");
        if let Some(vv) = result {
            let (version, value) = Self::from_vv(vv.value());
            Ok(Some((version, value)))
        } else {
            Ok(None)
        }
    }

    fn get_prefix(&self, prefix: &str) -> Result<Vec<(String, (u64, Vec<u8>))>, Error> {
        let tx = self.db.begin_read().unwrap();
        let table = tx.open_table(TABLE).unwrap();
        let mut result = Vec::new();
        for item in table.range(prefix..).unwrap() {
            let (key, vv) = item.expect("failed to iterate");
            if key.value().starts_with(prefix) {
                let (version, value) = Self::from_vv(vv.value());
                result.push((key.value().to_string(), (version, value)));
            } else {
                break;
            }
        }
        Ok(result)
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        self.put(key, &[])
    }

    fn clear_database(&self) -> Result<(), Error> {
        let tx = self.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(TABLE).unwrap();
            for _ in table.drain(""..).unwrap() {}
        }
        tx.commit().unwrap();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::kvv::redb::RedbKVVStore;
    use crate::kvv::KVVPersister;
    use alloc::sync::Arc;
    use lightning_signer::node::{Node, NodeServices};
    use lightning_signer::persist::MemorySeedPersister;
    use lightning_signer::policy::simple_validator::SimpleValidatorFactory;
    use lightning_signer::util::clock::StandardClock;
    use hex::FromHex;
    use lightning_signer::util::test_utils::*;
    use std::env;
    use std::path::Path;

    #[test]
    fn restore_0_9_test() {
        // running inside kcov doesn't set CARGO_MANIFEST_DIR, so we have a fallback
        let fixture_path = if let Ok(module_path) = env::var("CARGO_MANIFEST_DIR") {
            println!("module_path: {}", module_path);
            format!("{}/../data/samples/0_9_redb", module_path)
        } else if let Ok(fixtures_path) = env::var("FIXTURES_DIR") {
            println!("fixtures_path: {}", fixtures_path);
            format!("{}/samples/0_9_redb", fixtures_path)
        } else {
            panic!("Missing CARGO_MANIFEST_DIR / FIXTURES_DIR");
        };
        if !Path::new(&fixture_path).exists() {
            panic!("Fixture path does not exist: {}", fixture_path);
        }
        let persister = KVVPersister(Box::new(RedbKVVStore::new(&fixture_path)));
        let mut seed = [0; 32];
        seed.copy_from_slice(Vec::from_hex(TEST_SEED[0]).unwrap().as_slice());

        let seed_persister = Arc::new(MemorySeedPersister::new(seed.to_vec()));
        let node_services = NodeServices {
            validator_factory: Arc::new(SimpleValidatorFactory::new()),
            starting_time_factory: make_genesis_starting_time_factory(TEST_NODE_CONFIG.network),
            persister: Arc::new(persister),
            clock: Arc::new(StandardClock()),
        };
        let nodes = Node::restore_nodes(node_services, seed_persister).unwrap();
        assert_eq!(nodes.len(), 1);
        let node = nodes.values().next().unwrap();
        assert_eq!(node.channels().len(), 1);
    }
}
