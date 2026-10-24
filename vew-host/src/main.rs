use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::io::{self, Write};
use std::net::UdpSocket;

use win_desktop_duplication::devices::AdapterFactory;
use win_desktop_duplication::tex_reader::TextureReader;
use win_desktop_duplication::{co_init, set_process_dpi_awareness, DesktopDuplicationApi};

const PACKET_SIZE: usize = 1400;
const HEADER_LEN: usize = 24;
const MAX_PAYLOAD: usize = PACKET_SIZE - HEADER_LEN;

struct PacketHeader {}

impl PacketHeader {
    pub fn send(sock: &UdpSocket, frame_id: u32, data: &[u8], keyframe: bool, capture_us: u64) -> io::Result<()> {
        let count = data.len().div_ceil(MAX_PAYLOAD) as u16;
        let mut buf = [0u8; PACKET_SIZE];
        for (i, chunk) in data.chunks(MAX_PAYLOAD).enumerate() {
            Self::write(&mut buf[..HEADER_LEN], frame_id, i as u16, count, keyframe, capture_us);
            buf[HEADER_LEN..HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            sock.send(&buf[..HEADER_LEN + chunk.len()])?;
        }
        Ok(())
    }

    pub fn write(h: &mut [u8], frame_id: u32, idx: u16, count: u16, keyframe: bool, capture_us: u64) {
        h[0..2].copy_from_slice(&0x5244u16.to_le_bytes()); // magic
        h[2] = 1;                                          // version
        h[3] = 0;
        h[4..8].copy_from_slice(&frame_id.to_le_bytes());
        h[8..10].copy_from_slice(&idx.to_le_bytes());
        h[10..12].copy_from_slice(&count.to_le_bytes());
        h[12] = keyframe as u8;                            // flags
        h[13..16].fill(0);                                 // reserved
        h[16..24].copy_from_slice(&capture_us.to_le_bytes());
    }
}

fn main() {
    println!("Welcome to the Vew host client. Please pick a rendering unit from the options below.\n\n");

    set_process_dpi_awareness();
    co_init();

    let mut selected = None;
    let mut choices: u8 = 0;
    for (i, adapter) in AdapterFactory::new().enumerate() {
        let count = adapter.iter_displays().count();
        println!("GPU {}: {} ({} display(s))", i + 1, adapter.name(), count);
        choices += 1;
    }
    loop {
        let mut input = 0;
        let mut i = String::new();
        print!("Choice: GPU ");
        io::stdout().flush().expect("Failed to flush stdout");

        io::stdin()
            .read_line(&mut i)
            .expect("Failed to read line");
        let parsed = i.trim().parse::<u8>();
        match parsed {
            Err(_) => {
                println!("Invalid choice.");
                println!("{:?}", i);
                continue;
            },
            Ok(int) => input = int
        }
        if input > choices || input <= 0 {
            println!("Invalid choice.");
            continue;
        }
        for (i, adapter) in AdapterFactory::new().enumerate() {
            if let Some(display) = adapter.get_display_by_idx(0) {
                if input as usize != i + 1 {
                    continue;
                }
                selected = Some((adapter.clone(), display));
            }
        }
        if selected.is_none() {
            println!("That GPU has no display attached.");
            continue;
        }
        break;
    }
    let (adapter, output) = selected.expect("selection has no display attached");

    let mut dupl = DesktopDuplicationApi::new(adapter, output.clone())
        .expect("failed to start desktop duplication");

    let (device, ctx) = dupl.get_device_and_ctx();
    let mut reader = TextureReader::new(device, ctx);

    let mut pixels: Vec<u8> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);

    let mut frames = 0u32;
    let mut misses = 0u32;
    let mut acquire_total = Duration::ZERO;
    let mut read_total = Duration::ZERO;

    let sock = UdpSocket::bind("0.0.0.0:6767").expect("failed to bind UDP socket");
    println!("UDP socket binded to 0.0.0.0:6767");

    println!("Waiting for client...");
    let (_, client) = sock.recv_from(&mut [0u8; 2048]).expect("failed to receive from client");
    sock.connect(client).expect("failed to connect to client");
    println!("Client connected: {}", client);

    loop {
        output.wait_for_vsync().expect("vsync wait failed");

        let t0 = Instant::now();
        let tex = match dupl.acquire_next_frame_now() {
            Ok(tex) => tex,
            Err(_) => {
                misses += 1;
                continue;
            }
        };
        let t1 = Instant::now();
        let capture_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;

        reader.get_data(&mut pixels, &tex).expect("readback failed");
        let t2 = Instant::now();

        if let Err(e) = PacketHeader::send(&sock, frames, &pixels, true, capture_us) {
            eprintln!("send failed: {}", e);
        }

        let desc = tex.desc();
        width = desc.width as usize;
        height = desc.height as usize;

        acquire_total += t1 - t0;
        read_total += t2 - t1;
        frames += 1;
    }
}