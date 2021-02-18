use std::convert::TryFrom;
use std::env;
use std::fmt::Debug;
use std::net::SocketAddr;

use lay::{
    Layer,
    Measured,
    gates::{PauliGate, HGate, CXGate},
    operations::{Operation, PauliOperation, HOperation, CXOperation}
};
use lay_steane::SteaneLayer;

use tokio::task;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::signal::ctrl_c;

use anyhow::{anyhow, bail, ensure};

#[allow(unused_imports)]
use log::{LevelFilter, info, warn};

use lay_mitouosc::message::{Response, Request};
use rosc::{OscMessage, OscPacket};

const OSC_BUF_LEN: usize = 1000;
const QUEUE_LEN: usize = 100;

/// Loop for sending response to client.
async fn sender_loop(tx_addr: SocketAddr, mut chan_rx: mpsc::Receiver<Response>) -> anyhow::Result<()> {
    let tx = std::net::UdpSocket::bind("0.0.0.0:9999")?;
    while let Some(msg) = chan_rx.recv().await {
        info!("sender_loop: Received from channel: {:?}", msg);
        let packet = rosc::encoder::encode(&OscPacket::Message(OscMessage::from(&msg)))
            .map_err(|e| anyhow!("{:?}", e))?;
        info!("sender_loop: Encoded packet (len={}): {:?}", packet.len(), packet);
        info!("sender_loop: Sending to {}...", tx_addr);
        //tx.send(&packet).await?;
        tx.send_to(&packet, tx_addr)?;
        info!("sender_loop: Sent.");
    }
    bail!("sender_loop: unexpected finished");
}

/// Loop for receiving request from client.
async fn receiver_loop(host_rx_addr: SocketAddr, chan_tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
    let mut buf = vec![0; OSC_BUF_LEN];
    let rx = UdpSocket::bind(host_rx_addr).await?;
    loop {
        info!("receiver_loop: Receiving from {}...", host_rx_addr);
        let len = rx.recv(&mut buf).await?;
        info!("receiver_loop: Received. len={}, bytes={:?}", len, &buf[..len]);
        let packet = rosc::decoder::decode(&buf[..len]);
        let packet = match packet {
            Ok(inner) => inner,
            Err(e) => {
                warn!("receiver_loop: OSC Error {:?}", e);
                continue;
            }
        };
        info!("receiver_loop: OSC Message: {:?}", packet);
        let msg = Request::try_from(match packet {
            OscPacket::Message(msg) => {
                warn!("receiver_loop: Message without Bundle");
                msg
            },
            OscPacket::Bundle(mut bundle) => {
                ensure!(bundle.content.len() != 0, "Received empty bundle.");
                ensure!(bundle.content.len() == 1, "Multiple messages in same bundle.");
                match bundle.content.pop().unwrap() {
                    OscPacket::Message(msg) => msg,
                    OscPacket::Bundle(_bundle) => bail!("Received nested bundle.")
                }
            }
        })?;
        info!("receiver_loop: Message: {:?}", msg);
        chan_tx.send(msg).await?;
    }
}

async fn runner_loop<L>(
        mut backend: L,
        mut ops_rx: mpsc::Receiver<Request>,
        result_tx: mpsc::Sender<Response>,
        cast_q: impl Fn(i32, i32) -> L::Qubit + Send + 'static,
        cast_s: impl Fn(i32, i32) -> L::Slot + Send + 'static) -> anyhow::Result<()>
where L: Layer + PauliGate + HGate + CXGate + Debug + Send + 'static,
      <L as Layer>::Operation: Operation<L> + PauliOperation<L> + HOperation<L> + CXOperation<L> + Debug + Send,
      <L as Layer>::Buffer: Send,
{
    info!("runner_loop: Start");
    let mut ops = backend.opsvec();
    let mut buf = backend.make_buffer();
    ops.initialize();
    while let Some(msg) = ops_rx.recv().await {
        info!("runner_loop: Message received from channel. {:?}", msg);
        match msg {
            Request::X(x, y) => ops.x(cast_q(x, y)),
            Request::Y(x, y) => ops.y(cast_q(x, y)),
            Request::Z(x, y) => ops.z(cast_q(x, y)),
            Request::H(x, y) => ops.h(cast_q(x, y)),
            Request::CX(x1, y1, x2, y2) => ops.cx(cast_q(x1, y1), cast_q(x2, y2)),
            Request::Mz(x, y) => {
                info!("runner_loop: Received Mz inst.");
                ops.measure(cast_q(x, y), cast_s(x, y));
                info!("runner_loop: send_receive...");
                info!("ops: {:?}", ops);
                backend.send_receive(ops.as_ref(), &mut buf);
                let bit = buf.get(cast_s(x, y));
                info!("runner_loop: measurement: {}", bit);
                result_tx.send(Response::Mz(0, bit as i32 as f32)).await?;
                ops.clear();
            },
            _ => unimplemented!()
        }
    }
    bail!("runner_loop unexpected exit");
}

pub async fn exec<L>(tx: SocketAddr,
                 rx: SocketAddr,
                 backend: L,
                 cast_q: impl Fn(i32, i32) -> L::Qubit + Send + 'static,
                 cast_s: impl Fn(i32, i32) -> L::Slot + Send + 'static) -> anyhow::Result<()>
where L: Layer + PauliGate + HGate + CXGate + Debug + Send + 'static,
      <L as Layer>::Operation: Operation<L> + PauliOperation<L> + HOperation<L> + CXOperation<L> + Debug + Send,
      <L as Layer>::Buffer: Send,
{
    let (ops_tx, ops_rx) = mpsc::channel(QUEUE_LEN);
    let (result_tx, result_rx) = mpsc::channel(QUEUE_LEN);
    let sender = task::spawn(sender_loop(tx, result_rx));
    let runner = task::spawn(runner_loop(backend, ops_rx, result_tx, cast_q, cast_s));
    let receiver = task::spawn(receiver_loop(rx, ops_tx));

    ctrl_c().await?;
    receiver.abort();
    runner.abort();
    sender.abort();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_default_env().filter_level(LevelFilter::Info).init();
    let n_qubits = 10;
    let tx = env::args().nth(1)
                        .ok_or(anyhow!("tx address expected"))?
                        .parse::<SocketAddr>()?;
    let rx = env::args().nth(2)
                        .ok_or(anyhow!("rx address expected"))?
                        .parse::<SocketAddr>()?;
    let backend = SteaneLayer::from_seed_with_gk(n_qubits, 123);

    exec(tx, rx, backend, |_, y| y as u32, |_, y| y as u32).await
}
