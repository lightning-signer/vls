// Largely cribbed from rust-lightning/lightning/src/offers/invoice.rs

use core::convert::Infallible;

use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::KeyPair;
use bitcoin::secp256k1::{schnorr, Message, PublicKey, Secp256k1, SecretKey};
use lightning::blinded_path::BlindedPath;

use lightning::chain::keysinterface::EntropySource;
use lightning::ln::features::BlindedHopFeatures;
use lightning::ln::PaymentHash;
use lightning::offers::invoice::BlindedPayInfo;
use lightning::offers::offer::OfferBuilder;

use crate::invoice::Invoice;

struct TestEntropySource {}
impl EntropySource for TestEntropySource {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        let mut rng = OsRng;
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        bytes
    }
}

fn payer_keys() -> KeyPair {
    let secp_ctx = Secp256k1::new();
    KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap())
}

fn payer_sign(digest: &Message) -> Result<schnorr::Signature, Infallible> {
    let secp_ctx = Secp256k1::new();
    let keys = KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
    Ok(secp_ctx.sign_schnorr_no_aux_rand(digest, &keys))
}

fn payer_pubkey() -> PublicKey {
    payer_keys().public_key()
}

fn recipient_keys() -> KeyPair {
    let secp_ctx = Secp256k1::new();
    KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[43; 32]).unwrap())
}

fn recipient_sign(digest: &Message) -> Result<schnorr::Signature, Infallible> {
    let secp_ctx = Secp256k1::new();
    let keys = KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[43; 32]).unwrap());
    Ok(secp_ctx.sign_schnorr_no_aux_rand(digest, &keys))
}

fn recipient_pubkey() -> PublicKey {
    recipient_keys().public_key()
}

fn pubkey(byte: u8) -> PublicKey {
    let secp_ctx = Secp256k1::new();
    PublicKey::from_secret_key(&secp_ctx, &privkey(byte))
}

fn privkey(byte: u8) -> SecretKey {
    SecretKey::from_slice(&[byte; 32]).unwrap()
}

fn payment_paths() -> Vec<(BlindedPath, BlindedPayInfo)> {
    let secp_ctx = Secp256k1::new();
    let entropy_source = TestEntropySource {};

    let paths = vec![
        BlindedPath::new_for_message(&[pubkey(43), pubkey(44)], &entropy_source, &secp_ctx)
            .expect("blinded path"),
        BlindedPath::new_for_message(&[pubkey(45), pubkey(46)], &entropy_source, &secp_ctx)
            .expect("blinded path"),
    ];

    let payinfo = vec![
        BlindedPayInfo {
            fee_base_msat: 1,
            fee_proportional_millionths: 1_000,
            cltv_expiry_delta: 42,
            htlc_minimum_msat: 100,
            htlc_maximum_msat: 1_000_000_000_000,
            features: BlindedHopFeatures::empty(),
        },
        BlindedPayInfo {
            fee_base_msat: 1,
            fee_proportional_millionths: 1_000,
            cltv_expiry_delta: 42,
            htlc_minimum_msat: 100,
            htlc_maximum_msat: 1_000_000_000_000,
            features: BlindedHopFeatures::empty(),
        },
    ];

    paths.into_iter().zip(payinfo.into_iter()).collect()
}

// Make a BOLT-12 invoice via Offer -> InvoiceRequest -> Invoice
pub fn make_test_bolt12_invoice(description: &str, payment_hash: PaymentHash) -> Invoice {
    let metadata = vec![1; 32];
    Invoice::Bolt12(
        OfferBuilder::new(description.into(), recipient_pubkey())
            .amount_msats(200_000)
            .build()
            .unwrap()
            .request_invoice(metadata, payer_pubkey())
            .unwrap()
            .build()
            .unwrap()
            .sign(payer_sign)
            .unwrap()
            .respond_with(payment_paths(), payment_hash)
            .unwrap()
            .build()
            .unwrap()
            .sign(recipient_sign)
            .unwrap(),
    )
}
