/// state persistence helper;
mod state;

use aws_nitro_enclaves_nsm_api::api::{Request, Response};
use aws_nitro_enclaves_nsm_api::driver::{nsm_exit, nsm_init, nsm_process_request};
use ed25519_consensus as ed25519;
use ed25519_consensus::SigningKey;
use rand_core::OsRng;
use serde_bytes::ByteBuf;
use std::io;
use std::os::unix::io::AsRawFd;
use std::thread;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tendermint::node::Id;
use tendermint_p2p::secret_connection::{self, PublicKey, SecretConnection};
use tmkms_light::chain::state::PersistStateSync;
use tmkms_light::config::validator::ValidatorConfig;
use tmkms_light::connection::{Connection, PlainConnection};
use tmkms_light::error::{io_error_wrap, Error};
use tmkms_light::utils::{read_u16_payload, write_u16_payload};
use tmkms_nitro_helper::{
    NitroConfig, NitroKeygenResponse, NitroRequest, NitroResponse, VSOCK_HOST_CID,
};
use tracing::{error, info, trace, warn};
use vsock::{VsockAddr, VsockStream};
use zeroize::{Zeroize, Zeroizing};

fn get_secret_connection(
    vsock_port: u32,
    identity_key: &ed25519::SigningKey,
    peer_id: Option<Id>,
) -> io::Result<Box<dyn Connection>> {
    let addr = VsockAddr::new(VSOCK_HOST_CID, vsock_port);
    let socket = vsock::VsockStream::connect(&addr)?;
    info!("KMS node ID: {}", PublicKey::from(identity_key));
    let identity_key = identity_key.clone();
    let connection = SecretConnection::new(socket, identity_key, secret_connection::Version::V0_34)
        .map_err(|e| {
            error!("secret connection failed: {}", e);
            io::Error::from(io::ErrorKind::Other)
        })?;
    let actual_peer_id = connection.remote_pubkey().peer_id();

    // TODO: https://github.com/informalsystems/tendermint-rs/issues/786
    if let Some(expected_peer_id) = peer_id {
        if expected_peer_id.ct_eq(&actual_peer_id).unwrap_u8() == 0 {
            error!(
                "(vsock port {}): validator peer ID mismatch! (expected {}, got {})",
                vsock_port, expected_peer_id, actual_peer_id
            );
            return Err(io::Error::from(io::ErrorKind::Other));
        }
    }
    info!("connected to validator successfully");

    if peer_id.is_none() {
        // TODO: https://github.com/informalsystems/tendermint-rs/issues/786
        warn!(
            "unverified validator peer ID! ({})",
            connection.remote_pubkey().peer_id()
        );
    }

    Ok(Box::new(connection))
}

/// keeps retrying with approx. 1 sec sleep until it manages to connect to tendermint privval endpoint
pub fn get_connection(
    config: &NitroConfig,
    id_keypair: Option<&ed25519::SigningKey>,
) -> Box<dyn Connection> {
    loop {
        let conn: io::Result<Box<dyn Connection>> = if let Some(ikp) = id_keypair {
            get_secret_connection(config.enclave_tendermint_conn, ikp, config.peer_id)
        } else {
            let addr = VsockAddr::new(VSOCK_HOST_CID, config.enclave_tendermint_conn);
            if let Ok(socket) = vsock::VsockStream::connect(&addr) {
                trace!("tendermint vsock port: {}", config.enclave_tendermint_conn);
                trace!("tendermint peer addr: {:?}", socket.peer_addr());
                trace!("tendermint local addr: {:?}", socket.local_addr());
                trace!("tendermint fd: {}", socket.as_raw_fd());
                info!("connected to validator successfully");
                let plain_conn = PlainConnection::new(socket);
                Ok(Box::new(plain_conn))
            } else {
                warn!("vsock failed to connect to validator");
                Err(io::ErrorKind::Other.into())
            }
        };
        if let Err(e) = conn {
            error!("tendermint connection error {:?}", e);
            thread::sleep(Duration::new(1, 0));
        } else {
            return conn.unwrap();
        }
    }
}

/// a simple req-rep handling loop
pub fn entry(mut stream: VsockStream) -> Result<(), Error> {
    let nsm_fd = nsm_init();
    let json_raw = read_u16_payload(&mut stream)?;
    let request: Result<NitroRequest, _> = serde_json::from_slice(&json_raw);
    match request {
        Ok(NitroRequest::Start(config)) => {
            let key_bytes = Zeroizing::new(
                aws_ne_sys::kms_decrypt(
                    config.aws_region.as_bytes(),
                    config.credentials.aws_key_id.as_bytes(),
                    config.credentials.aws_secret_key.as_bytes(),
                    config.credentials.aws_session_token.as_bytes(),
                    config.sealed_consensus_key.as_ref(),
                )
                .map_err(|_e| Error::access_error())?,
            );
            let secret = ed25519::SigningKey::try_from(key_bytes.as_slice())
                .map_err(|_e| Error::invalid_key_error())?;
            let id_keypair = if let Some(ref ciphertext) = config.sealed_id_key {
                let id_key_bytes = Zeroizing::new(
                    aws_ne_sys::kms_decrypt(
                        config.aws_region.as_bytes(),
                        config.credentials.aws_key_id.as_bytes(),
                        config.credentials.aws_secret_key.as_bytes(),
                        config.credentials.aws_session_token.as_bytes(),
                        ciphertext.as_ref(),
                    )
                    .map_err(|_e| Error::access_error())?,
                );
                let id_secret = ed25519::SigningKey::try_from(id_key_bytes.as_slice())
                    .map_err(|_e| Error::invalid_key_error())?;
                Some(id_secret)
            } else {
                None
            };
            let mut state_holder = state::StateHolder::new(config.enclave_state_port)
                .map_err(|e| Error::io_error("failed get state connection".into(), e))?;
            let state = state_holder
                .load_state()
                .map_err(|e| io_error_wrap("failed to load initial state".into(), e))?;
            let conn: Box<dyn Connection> = get_connection(&config, id_keypair.as_ref());
            let mut session = tmkms_light::session::Session::new(
                ValidatorConfig {
                    chain_id: config.chain_id.clone(),
                    max_height: config.max_height,
                },
                conn,
                secret,
                state,
                state_holder,
            );
            loop {
                if let Err(e) = session.request_loop() {
                    error!("request error: {}", e);
                }
                let conn: Box<dyn Connection> = get_connection(&config, id_keypair.as_ref());
                session.reset_connection(conn);
            }
        }
        Ok(NitroRequest::Keygen(keygen_config)) => {
            let csprng = OsRng {};
            let mut keypair = SigningKey::new(csprng);
            let public = keypair.verification_key();
            let pubkeyb64 = String::from_utf8(subtle_encoding::base64::encode(public))
                .map_err(|e| io_error_wrap("base64 encoding error".into(), e))?;
            let keyidb64 =
                String::from_utf8(subtle_encoding::base64::encode(&keygen_config.kms_key_id))
                    .map_err(|e| io_error_wrap("base64 encoding error".into(), e))?;

            let claim = format!(
                "{{\"pubkey\":\"{}\",\"key_id\":\"{}\"}}",
                pubkeyb64, keyidb64
            );
            let user_data = Some(ByteBuf::from(claim));
            let response: NitroResponse = match aws_ne_sys::kms_encrypt(
                keygen_config.aws_region.as_bytes(),
                keygen_config.credentials.aws_key_id.as_bytes(),
                keygen_config.credentials.aws_secret_key.as_bytes(),
                keygen_config.credentials.aws_session_token.as_bytes(),
                keygen_config.kms_key_id.as_bytes(),
                keypair.as_bytes(),
            ) {
                Ok(encrypted_secret) => {
                    let req = Request::Attestation {
                        user_data,
                        // as this is one-off attestation on generation,
                        // no need here (this may useful in other scenarios)
                        nonce: None,
                        // this field is meant for encryptions (e.g. when AWS KMS
                        // sends a response to the enclave),
                        // so it's used in `aws_ne_sys`, but not here
                        public_key: None,
                    };
                    let att = nsm_process_request(nsm_fd, req);
                    match att {
                        Response::Attestation { document } => Ok(NitroKeygenResponse {
                            encrypted_secret,
                            public_key: public.as_bytes().to_vec(),
                            attestation_doc: document,
                        }),
                        _ => Err("failed to obtain an attestation document".to_owned()),
                    }
                }
                Err(e) => Err(format!("{:?}", e)),
            };
            keypair.zeroize();
            let json = serde_json::to_string(&response).map_err(Error::serialization_error)?;
            write_u16_payload(&mut stream, json.as_bytes())
                .map_err(|e| Error::io_error("failed to send keypair response".into(), e))?;
        }
        Err(e) => {
            error!("config error: {}", e);
        }
    }
    nsm_exit(nsm_fd);

    Ok(())
}
