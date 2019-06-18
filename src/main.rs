pub mod json_rpc_client;
pub mod mempool;
pub mod messages;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
    io::Error
};

use bitcoin::{util::psbt::serialize::{Serialize, Deserialize}, Transaction};
use futures::{future::ok, lazy, Future, Stream};
use futures_zmq::{prelude::*, Sub};
use log::{error, info};
use mempool::Mempool;
use tokio::{codec::Framed, net::TcpListener, prelude::*, timer::Interval};
use serde_json::json;

use crate::{
    json_rpc_client::JsonClient,
    messages::{Message, MessageCodec},
};

fn main() {
    // Logging
    std::env::set_var("RUST_LOG", "INFO");
    pretty_env_logger::init_timed();

    info!("starting...");

    // Mempool
    let mempool_shared = Arc::new(Mutex::new(Mempool::default()));

    // Bitcoin client
    let json_client = Arc::new(JsonClient::new(
        "http://127.0.0.1:8333".to_string(),
        "0hlb".to_string(),
        "heychris".to_string(),
    ));

    // ZeroMQ
    // Transaction subscription
    let context = Arc::new(zmq::Context::new());
    let mempool_shared_inner = mempool_shared.clone();
    let tx_sub = Sub::builder(context.clone())
        .connect("tcp:///127.0.0.1:28332")
        .filter("rawtx".as_bytes())
        .build();
    let tx_runner = tx_sub
        .and_then(move |tx_sub| {
            tx_sub.stream().for_each(move |multipart| {
                // Add new transactions to mempool
                let tx_raw: &[u8] = &multipart.get(0).unwrap();
                info!("new tx");
                let new_tx = Transaction::deserialize(tx_raw).unwrap();
                mempool_shared_inner.lock().unwrap().insert(new_tx);
                ok(())
            })
        })
        .map(|_| ())
        .map_err(|e| {
            error!("tx subscription error = {}", e);
        });

    // Block subscription
    let mempool_shared_inner = mempool_shared.clone();
    let block_sub = Sub::builder(context.clone())
        .connect("tcp:///127.0.0.1:28332")
        .filter("hashblock".as_bytes())
        .build();
    let block_runner = block_sub
        .and_then(move |block_sub| {
            block_sub.stream().for_each(move |multipart| {
                let block_hash: &[u8] = &multipart.get(0).unwrap();
                info!("new block = {:?}", block_hash);

                // Reset mempool
                *mempool_shared_inner.lock().unwrap() = Mempool::default();

                // TODO: Repopulate

                ok(())
            })
        })
        .map(|_| ())
        .map_err(|e| {
            error!("block subscription error = {}", e);
        });

    // Server
    let incoming = TcpListener::bind(&"0.0.0.0:8885".parse().unwrap())
        .unwrap()
        .incoming();

    let mempool_shared_inner = mempool_shared.clone();
    let server = incoming.map_err(|e| error!("{}", e)).for_each(move |socket| {
        let mempool_shared_inner = mempool_shared_inner.clone();
        let framed_sock = Framed::new(socket, MessageCodec);
        let (send_stream, received_stream) = framed_sock.split();
        let json_client_inner = json_client.clone();
        let responses = received_stream.filter_map(move |msg| {
            match msg {
                Message::Minisketch(mut peer_minisketch) => {
                    let minisketch = mempool_shared_inner.lock().unwrap().minisketch()  ;
                    peer_minisketch.merge(&minisketch).unwrap();

                    let mut decoded_ids = [0u64; 512]; // Overestimation here
                    peer_minisketch.decode(&mut decoded_ids).unwrap();

                    Some(Message::GetTxs(
                        decoded_ids.iter().filter(|id| **id != 0).cloned().collect(),
                    ))
                }
                Message::Oddsketch(peer_oddsketch) => {
                    let mempool_guard = mempool_shared_inner.lock().unwrap();
                    let oddsketch = mempool_guard.oddsketch();
                    let estimated_size = (oddsketch ^ peer_oddsketch).size();
                    let out_minisketch = mempool_guard.minisketch_slice(estimated_size as usize);
                    Some(Message::Minisketch(out_minisketch))
                }
                Message::GetTxs(vec_ids) => {
                    let mempool_guard = mempool_shared_inner.lock().unwrap();
                    Some(Message::Txs(
                        vec_ids
                            .iter()
                            .filter_map(|id| mempool_guard.tx().get(id))
                            .cloned()
                            .collect(),
                    ))
                }
                Message::Txs(vec_txs) => {
                    let mut mempool_guard = mempool_shared_inner.lock().unwrap();
                    for tx in vec_txs {
                        let raw = tx.serialize();
                        mempool_guard.insert(tx);
                        let req = json_client_inner.build_request("sendrawtransaction".to_string(), vec![json!(raw)]);
                        tokio::spawn(json_client_inner.send_request(&req).map(|_| {}).map_err(|e| error!("{:?}", e)));
                    }

                    None
                }
            }
        });

        // Heartbeat
        let mempool_shared_inner = mempool_shared.clone();
        let interval = Interval::new_interval(Duration::from_millis(1000)).map_err(|e| {error!("{}", e); Error::from_raw_os_error(0)});
        let heartbeat = interval.map(move |_| {
            Message::Oddsketch(mempool_shared_inner.lock().unwrap().oddsketch())
        });

        let out = responses.select(heartbeat);
        let send = send_stream.send_all(out).map(|_| ()).or_else(move |e| {
            error!("{}", e);
            Ok(())
        });
        tokio::spawn(send)
    });

    tokio::run(lazy(|| {
        tokio::spawn(tx_runner);
        tokio::spawn(block_runner);
        tokio::spawn(server);
        ok(())
    }));
}
