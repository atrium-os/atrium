use crate::command::protocol::{Command, Completion};
use crate::input::capture::InputEvent;

use std::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write};
use std::collections::VecDeque;

const CMD_SIZE: usize = 128;

pub struct NetworkLink {
    listener: TcpListener,
    client: Option<TcpStream>,
    recv_buf: Vec<u8>,
    display_w: u32,
    display_h: u32,
    refresh_hz: u32,
}

impl NetworkLink {
    pub fn bind(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", port))?;
        listener.set_nonblocking(true)?;
        log::info!("GPU server listening on TCP port {}", port);
        Ok(Self {
            listener,
            client: None,
            recv_buf: Vec::new(),
            display_w: 0,
            display_h: 0,
            refresh_hz: 0,
        })
    }

    pub fn set_display_info(&mut self, w: u32, h: u32, hz: u32) {
        self.display_w = w;
        self.display_h = h;
        self.refresh_hz = hz;
    }

    fn try_accept(&mut self) {
        if self.client.is_some() { return; }
        match self.listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(true).ok();
                log::info!("GPU client connected from {}", addr);
                self.client = Some(stream);
                self.recv_buf.clear();
                // send display info immediately
                self.send_display_info();
            }
            Err(_) => {}
        }
    }

    pub fn recv_command(&mut self) -> Option<Command> {
        self.try_accept();

        let client = self.client.as_mut()?;

        // read available data
        let mut tmp = [0u8; 4096];
        match client.read(&mut tmp) {
            Ok(0) => {
                log::info!("GPU client disconnected");
                self.client = None;
                return None;
            }
            Ok(n) => {
                self.recv_buf.extend_from_slice(&tmp[..n]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                self.client = None;
                return None;
            }
        }

        // need at least CMD_SIZE bytes for a command
        if self.recv_buf.len() < CMD_SIZE { return None; }

        let buf: Vec<u8> = self.recv_buf.drain(..CMD_SIZE).collect();

        // check for handshake (opcode 0xFFFF)
        let opcode = u16::from_le_bytes([buf[0], buf[1]]);
        if opcode == 0xFFFF {
            self.send_display_info();
            return None;
        }

        let cmd: Command = *bytemuck::from_bytes(&buf);
        Some(cmd)
    }

    pub fn send_completion(&mut self, comp: &Completion) {
        if let Some(ref mut client) = self.client {
            let bytes = bytemuck::bytes_of(comp);
            let _ = client.write_all(bytes);
        }
    }

    pub fn send_input_event(&mut self, evt: &InputEvent) {
        if let Some(ref mut client) = self.client {
            let bytes = bytemuck::bytes_of(evt);
            let _ = client.write_all(bytes);
        }
    }

    fn send_display_info(&mut self) {
        if let Some(ref mut client) = self.client {
            let mut buf = [0u8; CMD_SIZE];
            buf[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
            buf[4..8].copy_from_slice(&self.display_w.to_le_bytes());
            buf[8..12].copy_from_slice(&self.display_h.to_le_bytes());
            buf[12..16].copy_from_slice(&self.refresh_hz.to_le_bytes());
            let _ = client.write_all(&buf);
        }
    }

    pub fn has_guest(&self) -> bool {
        self.client.is_some()
    }
}
