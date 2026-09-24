//! A hand-written RFB client, built out of [`bolide_rfb::proto`]'s message builders and
//! nothing else.
//!
//! Deliberately *not* `bolide_rfb::Session`: the fake's job is to agree with the written
//! protocol, and testing it against bolide's own client would only prove the two agree
//! with each other. Every byte this client reads is read against the shape RFC 6143
//! describes, so a fake that drifted would fail here before it reached bolide.
//!
//! Every read has a timeout. A hung test on CI is indistinguishable from a dead runner.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use bolide_rfb::proto::{self, PixelFormat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Generous enough that a loaded box does not flake, short enough that a genuinely
/// stuck server fails the test rather than the job.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Client {
    pub stream: TcpStream,
}

/// What ServerInit told us.
#[derive(Debug, PartialEq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub format: PixelFormat,
    pub name: String,
}

/// One rect of a FramebufferUpdate: its header, and the bytes that followed.
#[derive(Debug, Clone, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub encoding: i32,
    /// Everything after the rect header — for ZRLE that includes the `u32` length.
    pub payload: Vec<u8>,
}

impl Rect {
    /// The ZRLE stream slice, past the `u32` byte count.
    pub fn zrle_stream(&self) -> &[u8] {
        let len = u32::from_be_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]) as usize;
        assert_eq!(
            self.payload.len(),
            4 + len,
            "a ZRLE payload's u32 must describe it"
        );
        &self.payload[4..]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Update(Vec<Rect>),
    Bell,
    CutText(String),
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Client {
        let stream = tokio::time::timeout(READ_TIMEOUT, TcpStream::connect(addr))
            .await
            .expect("connecting to loopback should not take five seconds")
            .expect("the fake should be listening");
        Client { stream }
    }

    pub async fn read_n(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(READ_TIMEOUT, self.stream.read_exact(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("the fake never sent the {n} bytes it owed"))
            .expect("the connection should still be open");
        buf
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("the fake is up");
    }

    /// Read the 12-byte banner and answer 3.8.
    pub async fn version(&mut self) -> Vec<u8> {
        let banner = self.read_n(12).await;
        self.send(proto::VERSION_3_8).await;
        banner
    }

    /// The security types on offer.
    pub async fn security_types(&mut self) -> Vec<u8> {
        let count = self.read_n(1).await[0] as usize;
        self.read_n(count).await
    }

    pub async fn security_result(&mut self) -> u32 {
        let r = self.read_n(4).await;
        u32::from_be_bytes([r[0], r[1], r[2], r[3]])
    }

    /// The 3.8 reason string that follows a failed SecurityResult.
    pub async fn failure_reason(&mut self) -> String {
        let len = self.read_n(4).await;
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        String::from_utf8(self.read_n(len).await).expect("a reason string is utf-8")
    }

    /// ClientInit, then ServerInit.
    pub async fn init(&mut self) -> ServerInit {
        self.send(&[1]).await; // shared flag
        let head = self.read_n(20).await;
        let mut block = [0u8; 16];
        block.copy_from_slice(&head[4..20]);
        let len = self.read_n(4).await;
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let name = String::from_utf8(self.read_n(len).await).expect("a desktop name is utf-8");
        ServerInit {
            width: u16::from_be_bytes([head[0], head[1]]),
            height: u16::from_be_bytes([head[2], head[3]]),
            format: PixelFormat::decode(&block),
            name,
        }
    }

    /// Version → security `None` → ServerInit, the whole opening in one call.
    pub async fn open(&mut self) -> ServerInit {
        self.version().await;
        let types = self.security_types().await;
        assert_eq!(types, vec![proto::SECURITY_NONE]);
        self.send(&[proto::SECURITY_NONE]).await;
        assert_eq!(self.security_result().await, proto::SECURITY_RESULT_OK);
        self.init().await
    }

    /// Read one server → client message, sizing rect payloads with `fmt` — which is
    /// exactly what a real client has to do, since no rect says how long it is.
    pub async fn message(&mut self, fmt: &PixelFormat) -> ServerMessage {
        let kind = self.read_n(1).await[0];
        match kind {
            0 => {
                let head = self.read_n(3).await;
                let count = u16::from_be_bytes([head[1], head[2]]);
                let mut rects = Vec::new();
                for _ in 0..count {
                    rects.push(self.rect(fmt).await);
                }
                ServerMessage::Update(rects)
            }
            2 => ServerMessage::Bell,
            3 => {
                let head = self.read_n(7).await;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
                ServerMessage::CutText(proto::from_latin1(&self.read_n(len).await))
            }
            other => panic!("the fake sent server message {other}"),
        }
    }

    async fn rect(&mut self, fmt: &PixelFormat) -> Rect {
        let head = self.read_n(12).await;
        let width = u16::from_be_bytes([head[4], head[5]]);
        let height = u16::from_be_bytes([head[6], head[7]]);
        let encoding = i32::from_be_bytes([head[8], head[9], head[10], head[11]]);
        let payload = match encoding {
            proto::ENC_RAW => {
                self.read_n(width as usize * height as usize * fmt.bytes_per_pixel())
                    .await
            }
            proto::ENC_COPY_RECT => self.read_n(4).await,
            proto::ENC_ZRLE => {
                let len = self.read_n(4).await;
                let n = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
                let mut payload = len;
                payload.extend_from_slice(&self.read_n(n).await);
                payload
            }
            proto::ENC_DESKTOP_SIZE => Vec::new(),
            other => panic!("the fake sent encoding {other}"),
        };
        Rect {
            x: u16::from_be_bytes([head[0], head[1]]),
            y: u16::from_be_bytes([head[2], head[3]]),
            width,
            height,
            encoding,
            payload,
        }
    }

    /// Is the socket closed? Reads once and expects EOF.
    pub async fn at_eof(&mut self) -> bool {
        let mut buf = [0u8; 1];
        match tokio::time::timeout(READ_TIMEOUT, self.stream.read(&mut buf)).await {
            Ok(Ok(0)) => true,
            Ok(Ok(_)) => false,
            Ok(Err(_)) => true, // a reset is a close too
            Err(_) => panic!("the socket neither closed nor sent anything"),
        }
    }
}

/// Inflate a ZRLE stream slice the way a client's single per-connection inflater does.
pub fn inflate(inf: &mut flate2::Decompress, stream: &[u8]) -> Vec<u8> {
    let mut input = stream;
    let mut out = Vec::new();
    loop {
        let before_in = inf.total_in();
        let before_out = inf.total_out();
        out.reserve(4096);
        inf.decompress_vec(input, &mut out, flate2::FlushDecompress::Sync)
            .expect("the fake's zlib stream must stay a stream");
        let consumed = (inf.total_in() - before_in) as usize;
        input = &input[consumed..];
        if input.is_empty() && inf.total_out() == before_out {
            return out;
        }
    }
}
