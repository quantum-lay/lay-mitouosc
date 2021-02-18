use std::convert::TryFrom;
use std::net::SocketAddr;

use tokio::task::{self, JoinHandle};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use anyhow::{anyhow, bail, ensure};

#[allow(unused_imports)]
use log::{LevelFilter, info, warn};

use message::{Response, Request};
use rosc::{OscMessage, OscPacket};

use lay::{
    Layer,
    Measured,
    operations::{opid, OpArgs},
    gates::{PauliGate, HGate, SGate, TGate, CXGate}
};

pub mod message;

const SEND_QUEUE_LEN: usize = 1000;
const RECV_QUEUE_LEN: usize = 1000;
const OSC_BUF_LEN: usize = 1000;

async fn device_comm_loop(tx_addr: SocketAddr,
                          rx_addr: SocketAddr,
                          mut req_rx: mpsc::Receiver<Option<Request>>,
                          meas_tx: mpsc::Sender<Option<((u32, u32), bool)>>) -> anyhow::Result<()> {
    let rx_sock = UdpSocket::bind(rx_addr).await?;
    let mut buf = vec![0; OSC_BUF_LEN];
    while let Some(msg) = req_rx.recv().await {
        info!("device_sender_loop: Received from channel: {:?}", msg);
        match msg {
            Some(msg) => {
                let packet = rosc::encoder::encode(&OscPacket::Message(OscMessage::from(&msg))
                        ).map_err(|e| anyhow!("{:?}", e))?;
                rx_sock.send_to(&packet, tx_addr).await?;
                if let Request::Mz(x, y) = msg {
                    let res = receive_response(&mut buf, &rx_sock).await?;
                    info!("Received from device: {:?}", res);
                    let measured = match res { Response::Mz(_, f) => (f as u32) == 1 };
                    meas_tx.send(Some(((x as u32, y as u32), measured))).await?;
                }
            },
            None => {
                meas_tx.send(None).await?;
            },
        }
    }
    bail!("device_sender_loop unexpected finished");
}

async fn receive_response(buf: &mut Vec<u8>, sock: &UdpSocket) -> anyhow::Result<Response> {
    let len = sock.recv(buf).await?;
    let packet = rosc::decoder::decode(&buf[..len]).map_err(|e| anyhow!("{:?}", e))?;
    let msg = Response::try_from(match packet {
        OscPacket::Message(msg) => {
            warn!("Message without Bundle");
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
    Ok(msg)
}

#[derive(Debug)]
pub struct MitouOscLayer {
    handle: JoinHandle<anyhow::Result<()>>,
    size: (u32, u32),
    sender: mpsc::Sender<Option<Request>>,
    receiver: mpsc::Receiver<Option<((u32, u32), bool)>>,
}

impl MitouOscLayer {
    pub fn exec(size: (u32, u32), device_tx: SocketAddr, device_rx: SocketAddr)
            -> anyhow::Result<MitouOscLayer> {
        exec(size, device_tx, device_rx)
    }
}

impl Drop for MitouOscLayer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Layer for MitouOscLayer {
    type Operation = OpArgs<Self>;
    type Qubit = (u32, u32);
    type Slot = (u32, u32);
    type Buffer = MitouOscBuffer;
    type Requested = anyhow::Result<()>;
    type Response = anyhow::Result<()>;

    fn send(&mut self, ops: &[Self::Operation]) -> Self::Requested {
        for op in ops {
            match op {
                OpArgs::Empty(id) if *id == opid::INIT => {
                    for y in 0..(self.size.1 as i32) {
                        for x in 0..(self.size.0 as i32) {
                            self.sender.blocking_send(Some(Request::InitZero(x, y)))?;
                        }
                    }
                }
                OpArgs::Q(id, q) => {
                    let x = q.0 as i32;
                    let y = q.1 as i32;
                    match *id {
                        opid::X => {
                            self.sender.blocking_send(Some(Request::X(x, y)))?;
                        },
                        opid::Y => {
                            self.sender.blocking_send(Some(Request::Y(x, y)))?;
                        },
                        opid::Z => {
                            self.sender.blocking_send(Some(Request::Z(x, y)))?;
                        },
                        opid::S => {
                            self.sender.blocking_send(Some(Request::S(x, y)))?;
                        },
                        opid::SDG => {
                            self.sender.blocking_send(Some(Request::Sdg(x, y)))?;
                        },
                        opid::T => {
                            self.sender.blocking_send(Some(Request::T(x, y)))?;
                        },
                        opid::TDG => {
                            self.sender.blocking_send(Some(Request::Tdg(x, y)))?;
                        },
                        _ => {
                            bail!("Unexpected single qubit gate");
                        }
                    }
                },
                OpArgs::QS(id, q, s) if *id == opid::MEAS => {
                    ensure!(q == s, "Qubit and slot must be same.");
                    let x = q.0 as i32;
                    let y = q.1 as i32;
                    self.sender.blocking_send(Some(Request::Mz(x, y)))?;
                },
                OpArgs::QQ(id, c, t) if *id == opid::CX => {
                    self.sender
                        .blocking_send(Some(Request::CX(c.0 as i32, c.1 as i32, t.0 as i32, t.1 as i32)))?;
                },
                _ => {
                    bail!("Unexpected operation");
                }
            }
        }
        self.sender.blocking_send(None)?;
        Ok(())
    }

    fn receive(&mut self, buf: &mut Self::Buffer) -> Self::Response {
        loop {
            match self.receiver.blocking_recv() {
                Some(Some(((x, y), m))) => {
                    (buf.0)[x as usize + (y as usize * buf.1)] = m;
                },
                Some(None) => {
                    return Ok(());
                }
                _ => {
                    bail!("Unexpected response");
                }
            }
        }
    }

    fn make_buffer(&self) -> Self::Buffer {
        let v = vec![false; (self.size.0 * self.size.1) as usize];
        MitouOscBuffer(v, self.size.0 as usize)
    }
}

impl PauliGate for MitouOscLayer {}
impl HGate for MitouOscLayer {}
impl SGate for MitouOscLayer {}
impl TGate for MitouOscLayer {}
impl CXGate for MitouOscLayer {}

#[derive(Debug, PartialEq, Eq)]
pub struct MitouOscBuffer(Vec<bool>, usize);

impl Measured for MitouOscBuffer {
    type Slot = (u32, u32);
    fn get(&self, pos: (u32, u32)) -> bool {
        let (x, y) = pos;
        (self.0)[self.1 * (y as usize) + (x as usize)]
    }
}

fn exec(size: (u32, u32), device_tx: SocketAddr, device_rx: SocketAddr) -> anyhow::Result<MitouOscLayer>
{
    let (req_tx, req_rx) = mpsc::channel(SEND_QUEUE_LEN);
    let (meas_tx, meas_rx) = mpsc::channel(RECV_QUEUE_LEN);
    Ok(MitouOscLayer {
        handle: task::spawn(async move {
            let device_comm = task::spawn(device_comm_loop(device_tx, device_rx, req_rx, meas_tx));

            device_comm.await??;
            Ok(())
        }),
        size,
        sender: req_tx,
        receiver: meas_rx,})
}
