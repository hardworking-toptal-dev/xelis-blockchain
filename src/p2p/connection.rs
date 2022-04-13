use crate::core::serializer::Serializer;
use crate::globals::get_current_time;
use crate::core::reader::Reader;
use super::error::P2pError;
use super::packet::PacketIn;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::net::{TcpStream, SocketAddr, Shutdown};
use std::fmt::{Display, Error, Formatter};
use std::sync::mpsc::Sender;
use std::io::{Write, Read};
use std::convert::TryInto;
use std::sync::Mutex;
use bytes::Bytes;
use log::warn;

pub type Tx = Sender<Bytes>; // TODO Use Tokio unbounded channel
type P2pResult<T> = std::result::Result<T, P2pError>;

pub enum State {
    Pending, // connection is new, no handshake received
    Handshake, // handshake received, not checked
    Success // handshake is valid
}

pub struct Connection {
    state: State,
    stream: Mutex<TcpStream>, // Stream for read & write
    addr: SocketAddr, // TCP Address
    tx: Mutex<Tx>, // Tx to send bytes
    bytes_in: AtomicUsize, // total bytes read
    bytes_out: AtomicUsize, // total bytes sent
    connected_on: u64,
    closed: AtomicBool, // if Connection#close() is called, close is set to true
    blocking: AtomicBool // blocking until something is sent or not
}

impl Connection {
    pub fn new(stream: TcpStream, addr: SocketAddr, tx: Tx) -> Self {
        Self {
            state: State::Pending,
            stream: Mutex::new(stream),
            addr,
            tx: Mutex::new(tx),
            connected_on: get_current_time(),
            bytes_in: AtomicUsize::new(0),
            bytes_out: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            blocking: AtomicBool::new(true)
        }
    }

    // Set the connection thread blocking or not
    pub fn set_blocking(&self, blocking: bool) -> P2pResult<()> {
        let stream = self.stream.lock()?;
        stream.set_nonblocking(!blocking)?;
        self.blocking.store(blocking, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_stream(&self) -> &Mutex<TcpStream> {
        &self.stream
    }

    pub fn send_bytes(&self, buf: &[u8]) -> P2pResult<()> {
        let mut stream = self.stream.lock()?;
        stream.write(buf)?;
        self.bytes_out.fetch_add(buf.len(), Ordering::Relaxed);
        stream.flush()?;
        Ok(())
    }

    pub fn read_packet(&self, buf: &mut [u8], max_size: u32) -> P2pResult<PacketIn> {
        let size = self.read_packet_size(buf)?;
        if size == 0 || size > max_size {
            warn!("Received invalid packet size: {} bytes (max: {} bytes) from peer {}", size, max_size, self.get_address());
            return Err(P2pError::InvalidPacketSize)
        }

        let bytes = self.read_all_bytes(buf, size)?;
        let mut reader = Reader::new(&bytes);
        let packet = PacketIn::read(&mut reader)?;
        if reader.total_read() != bytes.len() {
            warn!("read only {}/{} on bytes available", reader.total_read(), bytes.len());
            return Err(P2pError::InvalidPacketNotFullRead)
        }
        Ok(packet)
    }

    fn read_packet_size(&self, buf: &mut [u8]) -> P2pResult<u32> {
        let read = self.read_bytes(&mut buf[0..4])?;
        if read != 4 {
            warn!("Received invalid packet size: expected to read 4 bytes but read only {} bytes from peer {}", read, self.get_address());
            return Err(P2pError::InvalidPacketSize)
        }
        let array: [u8; 4] = buf[0..4].try_into()?;
        let size = u32::from_be_bytes(array);
        Ok(size)
    }

    fn read_all_bytes(&self, buf: &mut [u8], mut left: u32) -> P2pResult<Vec<u8>> {
        let buf_size = buf.len() as u32;
        let mut bytes = Vec::new();
        let mut stream = self.stream.lock()?;
        while left > 0 {
            let max = if buf_size > left {
                left as usize
            } else {
                buf_size as usize
            };
            let read = self.read_bytes_from_stream(&mut stream, &mut buf[0..max])?;
            left -= read as u32;
            bytes.extend(&buf[0..read]);
        }
        Ok(bytes)
    }

    // this function will wait until something is sent to the socket if it's in blocking mode
    // this return the size of data read & set in the buffer.
    // used to only lock one time the stream and read on it
    fn read_bytes_from_stream(&self, stream: &mut TcpStream, buf: &mut [u8]) -> P2pResult<usize> {
        let result = stream.read(buf)?;
        match result {
            0 => {
                self.close()?;
                Err(P2pError::Disconnected)
            }
            n => {
                self.bytes_in.fetch_add(n, Ordering::Relaxed);
                Ok(n)
            }
        }
    }

    pub fn read_bytes(&self, buf: &mut [u8]) -> P2pResult<usize> {
        let mut stream = self.stream.lock()?;
        self.read_bytes_from_stream(&mut stream, buf)
    }

    pub fn close(&self) -> P2pResult<()> {
        self.closed.store(true, Ordering::Relaxed);
        self.stream.lock()?.shutdown(Shutdown::Both)?;
        Ok(())
    }

    pub fn set_state(&mut self, state: State) {
        self.state = state;
    }

    pub fn get_state(&self) -> &State {
        &self.state
    }

    pub fn get_address(&self) -> &SocketAddr {
        &self.addr
    }

    pub fn bytes_out(&self) -> usize {
        self.bytes_out.load(Ordering::Relaxed)
    }

    pub fn bytes_in(&self) -> usize {
        self.bytes_in.load(Ordering::Relaxed)
    }

    pub fn connected_on(&self) -> u64 {
        self.connected_on
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn is_blocking(&self) -> bool {
        self.blocking.load(Ordering::Relaxed)
    }
}

impl Display for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), Error> {
        write!(f, "Connection[peer: {}, read: {} kB, sent: {} kB, connected on: {}, closed: {},  blocking: {}]", self.get_address(), self.bytes_in() / 1024, self.bytes_out() / 1024, self.connected_on(), self.is_closed(), self.is_blocking())
    }
}