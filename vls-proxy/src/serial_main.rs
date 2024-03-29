//! A single-binary hsmd drop-in replacement for CLN, connecting to an embedded
//! VLS over a USB / serial connection.

use std::env;
use std::sync::{Arc, Mutex};

use clap::{arg, App};
use url::Url;

#[allow(unused_imports)]
use log::{error, info};

use lightning_signer::bitcoin::Network;

use serial::{connect, SerialSignerPort, SignerLoop};

use vls_frontend::frontend::SourceFactory;
use vls_frontend::Frontend;
use vls_protocol::msgs::{self, Message, SerialRequestHeader};
use vls_protocol::serde_bolt::WireString;
use vls_protocol_signer::vls_protocol;

use client::UnixClient;
use connection::{open_parent_fd, UnixConnection};
use portfront::SignerPortFront;
use util::{
    abort_on_panic, add_hsmd_args, bitcoind_rpc_url, create_runtime, setup_logging, vls_network,
};
use vls_proxy::*;

mod serial;

fn run_test(serial_port: String) -> anyhow::Result<()> {
    let mut serial = connect(serial_port)?;
    let mut id = 0u16;
    let mut sequence = 1;

    loop {
        let peer_id = [0u8; 33];
        let dbid = 0;
        msgs::write_serial_request_header(
            &mut serial,
            &SerialRequestHeader { sequence, peer_id, dbid },
        )?;
        let ping = msgs::Ping { id, message: WireString("ping".as_bytes().to_vec()) };
        msgs::write(&mut serial, ping)?;
        msgs::read_serial_response_header(&mut serial, sequence)?;
        sequence = sequence.wrapping_add(1);
        let reply = msgs::read(&mut serial)?;
        match reply {
            Message::Pong(p) => {
                info!("got reply {} {}", p.id, String::from_utf8(p.message.0).unwrap());
                assert_eq!(p.id, id);
            }
            _ => {
                panic!("unknown response");
            }
        }
        id += 1;
    }
}

pub fn main() -> anyhow::Result<()> {
    abort_on_panic();
    let parent_fd = open_parent_fd();

    setup_logging(".", "remote_hsmd_serial", "info");

    // Why does this interfere w/ the serial communication?
    // info!("remote_hsmd_serial git_desc={} starting", GIT_DESC);

    let app = make_clap_app();
    let matches = app.get_matches();
    if matches.is_present("git-desc") {
        println!("remote_hsmd_serial git_desc={}", GIT_DESC);
        return Ok(());
    }
    if matches.is_present("version") {
        // Pretend to be the right version, given to us by an env var
        let version =
            env::var("GREENLIGHT_VERSION").expect("set GREENLIGHT_VERSION to match c-lightning");
        println!("{}", version);
        return Ok(());
    }

    let serial_port = env::var("VLS_SERIAL_PORT").unwrap_or("/dev/ttyACM1".to_string());

    if matches.is_present("test") {
        run_test(serial_port)?;
    } else {
        let conn = UnixConnection::new(parent_fd);
        let client = UnixClient::new(conn);
        let serial = Arc::new(Mutex::new(connect(serial_port)?));

        let network = vls_network().parse::<Network>().expect("malformed vls network");
        let signer_port = SerialSignerPort::new(serial.clone());
        let signer_front = Arc::new(SignerPortFront::new(Arc::new(signer_port), network));
        let source_factory = Arc::new(SourceFactory::new(".", network));
        let frontend = Frontend::new(
            signer_front,
            source_factory,
            Url::parse(&bitcoind_rpc_url()).expect("malformed rpc url"),
        );

        let runtime = create_runtime("serial-frontend");
        runtime.block_on(async {
            frontend.start();
        });

        let mut signer_loop = SignerLoop::new(client, serial);
        signer_loop.start();
    }

    Ok(())
}

fn make_clap_app() -> App<'static> {
    let app = App::new("signer")
        .about("CLN:serial - connects to an embedded VLS over a USB / serial connection")
        .arg(arg!(--test "run a test against the embedded device"));
    add_hsmd_args(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_clap_app() {
        make_clap_app();
    }
}
