use std::ffi::{c_char, CStr};
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

pub const PACKET_SIZE: usize = 1400;
pub const HEADER_LEN: usize = 24;
pub const MAX_PAYLOAD: usize = PACKET_SIZE - HEADER_LEN;
pub const MAGIC: u16 = 0x5244;
pub const VERSION: u8 = 1;
pub const KIND_VIDEO: u8 = 0;
const HELLO: &[u8] = b"VEW-HELLO";

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VewHeader {
    pub frame_id: u32,
    pub packet_index: u16,
    pub packet_count: u16,
    pub kind: u8,
    pub keyframe: u8,
    pub capture_us: u64,
}

impl VewHeader {
    pub fn parse(packet: &[u8]) -> Option<VewHeader> {
        if packet.len() < HEADER_LEN
            || u16::from_le_bytes([packet[0], packet[1]]) != MAGIC
            || packet[2] != VERSION
        {
            return None;
        }
        let h = VewHeader {
            kind: packet[3],
            frame_id: u32::from_le_bytes(packet[4..8].try_into().unwrap()),
            packet_index: u16::from_le_bytes([packet[8], packet[9]]),
            packet_count: u16::from_le_bytes([packet[10], packet[11]]),
            keyframe: packet[12] & 1,
            capture_us: u64::from_le_bytes(packet[16..24].try_into().unwrap()),
        };
        if h.packet_count == 0 || h.packet_index >= h.packet_count {
            return None;
        }
        Some(h)
    }

    pub fn write(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        out[2] = VERSION;
        out[3] = self.kind;
        out[4..8].copy_from_slice(&self.frame_id.to_le_bytes());
        out[8..10].copy_from_slice(&self.packet_index.to_le_bytes());
        out[10..12].copy_from_slice(&self.packet_count.to_le_bytes());
        out[12] = self.keyframe;
        out[13..16].fill(0);
        out[16..24].copy_from_slice(&self.capture_us.to_le_bytes());
    }
}

pub struct Frame<'a> {
    pub data: &'a [u8],
    pub frame_id: u32,
    pub capture_us: u64,
    pub keyframe: bool,
}

pub struct VewClient {
    sock: UdpSocket,
    buf: Vec<u8>,
    received: Vec<bool>,
    remaining: u16,
    len: usize,
    current: Option<VewHeader>,
    timeout: Option<Duration>,
    got_any: bool,
}

impl VewClient {
    pub fn connect<A: ToSocketAddrs>(host: A) -> io::Result<VewClient> {
        let host: SocketAddr = host
            .to_socket_addrs()?
            .find(|a| a.is_ipv4())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no IPv4 address"))?;
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_recv_buffer_size(32 * 1024 * 1024)?;
        let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
        socket.bind(&bind.into())?;
        let sock: UdpSocket = socket.into();
        sock.connect(host)?;
        sock.send(HELLO)?;
        Ok(VewClient {
            sock,
            buf: Vec::new(),
            received: Vec::new(),
            remaining: 0,
            len: 0,
            current: None,
            timeout: None,
            got_any: false,
        })
    }

    pub fn next_frame(&mut self, timeout: Option<Duration>) -> io::Result<Option<Frame<'_>>> {
        let timeout = timeout.filter(|t| !t.is_zero());
        if timeout != self.timeout {
            self.sock.set_read_timeout(timeout)?;
            self.timeout = timeout;
        }
        let mut packet = [0u8; 2048];
        loop {
            let n = match self.sock.recv(&mut packet) {
                Ok(n) => n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if !self.got_any {
                        self.sock.send(HELLO)?;
                    }
                    return Ok(None);
                }
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            };
            self.got_any = true;
            if let Some(h) = VewHeader::parse(&packet[..n]) {
                if self.push(h, &packet[HEADER_LEN..n]) {
                    return Ok(Some(self.frame()));
                }
            }
        }
    }

    fn push(&mut self, h: VewHeader, payload: &[u8]) -> bool {
        if h.kind != KIND_VIDEO || payload.len() > MAX_PAYLOAD {
            return false;
        }
        match self.current {
            Some(c) if c.frame_id == h.frame_id => {
                if c.packet_count != h.packet_count {
                    return false;
                }
            }
            Some(c) if (1..64).contains(&c.frame_id.wrapping_sub(h.frame_id)) => return false,
            _ => self.start(h),
        }
        let idx = h.packet_index as usize;
        if self.received[idx] || (idx + 1 < self.received.len() && payload.len() != MAX_PAYLOAD) {
            return false;
        }
        let off = idx * MAX_PAYLOAD;
        self.buf[off..off + payload.len()].copy_from_slice(payload);
        self.received[idx] = true;
        self.len = self.len.max(off + payload.len());
        self.remaining -= 1;
        self.remaining == 0
    }

    fn start(&mut self, h: VewHeader) {
        let count = h.packet_count as usize;
        self.buf.resize(count * MAX_PAYLOAD, 0);
        self.received.clear();
        self.received.resize(count, false);
        self.remaining = h.packet_count;
        self.len = 0;
        self.current = Some(h);
    }

    fn frame(&self) -> Frame<'_> {
        let h = self.current.unwrap();
        Frame {
            data: &self.buf[..self.len],
            frame_id: h.frame_id,
            capture_us: h.capture_us,
            keyframe: h.keyframe != 0,
        }
    }
}

#[repr(C)]
pub struct VewFrame {
    pub data: *const u8,
    pub len: usize,
    pub frame_id: u32,
    pub keyframe: u8,
    pub capture_us: u64,
}

#[no_mangle]
pub extern "C" fn vew_parse_header(packet: *const u8, len: usize, out: *mut VewHeader) -> i32 {
    if packet.is_null() || out.is_null() {
        return 0;
    }
    let packet = unsafe { std::slice::from_raw_parts(packet, len) };
    match VewHeader::parse(packet) {
        Some(h) => {
            unsafe { *out = h };
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn vew_client_connect(host: *const c_char) -> *mut VewClient {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
        return std::ptr::null_mut();
    };
    match VewClient::connect(host) {
        Ok(c) => Box::into_raw(Box::new(c)),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn vew_client_next_frame(client: *mut VewClient, out: *mut VewFrame, timeout_ms: u32) -> i32 {
    if client.is_null() || out.is_null() {
        return -1;
    }
    let client = unsafe { &mut *client };
    let timeout = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms as u64));
    match client.next_frame(timeout) {
        Ok(Some(f)) => {
            unsafe {
                *out = VewFrame {
                    data: f.data.as_ptr(),
                    len: f.data.len(),
                    frame_id: f.frame_id,
                    keyframe: f.keyframe as u8,
                    capture_us: f.capture_us,
                };
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub extern "C" fn vew_client_destroy(client: *mut VewClient) {
    if !client.is_null() {
        unsafe { drop(Box::from_raw(client)) };
    }
}