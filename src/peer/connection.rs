use anyhow::{bail, Context};
use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;

use crate::peer::HandshakePacket;
use crate::PEER_ID;

#[derive(Debug, PartialEq)]
pub enum Frame {
    Choke,
    Unchoke,
    Interested,
    NotInterested,

    /// Single number, the index which that downloader just completed and
    /// checked the hash of.
    Have(u32),

    /// Only ever sent as the first message. Its payload is a bitfield with each
    /// index that downloader has sent set to one and the rest set to zero.
    /// Downloaders which don't have anything yet may skip the 'bitfield' message.
    /// The first byte of the bitfield corresponds to indices 0 - 7 from high bit
    /// to low bit, respectively. The next one 8-15, etc. Spare bits at the end
    /// are set to zero.
    Bitfield(Bytes),

    /// Request a piece chunk.
    Request {
        // The zero-based piece index.
        index: u32,
        /// The zero-based byte offset within the piece
        /// This'll be 0 for the first block, 2^14 for the second block, 2*2^14
        /// for the third block etc.
        begin: u32,
        /// Generally a power of two unless it gets truncated by the end of the file.
        /// All current implementations use 2^14 (16 kiB), and close connections
        /// which request an amount greater than that
        length: u32,
    },

    /// Correlated with request messages implicitly. It is possible for an unexpected
    /// piece to arrive if choke and unchoke messages are sent in quick succession
    /// and/or transfer is going very slowly.
    Piece {
        // The zero-based piece index.
        index: u32,
        /// The zero-based byte offset within the piece.
        begin: u32,
        /// The data for the piece, usually 2^14 bytes long.
        chunk: Bytes,
    },

    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
}

/// A wrapper around the `TcpStream` to send and receive framed messages.
#[derive(Debug)]
pub struct Connection {
    stream: BufWriter<TcpStream>,
    buf: BytesMut,
}

/// 4B
const U32_SIZE: usize = std::mem::size_of::<u32>();

/// 65536B (64KiB)
const FRAME_MAX: usize = 1 << 16;

impl Connection {
    pub fn new(stream: TcpStream) -> Connection {
        Connection {
            stream: BufWriter::new(stream),
            buf: BytesMut::with_capacity(32 * 1024),
        }
    }

    pub async fn handshake(&mut self, info_hash: [u8; 20]) -> crate::Result<HandshakePacket> {
        let mut packet = HandshakePacket::new(info_hash, *PEER_ID);
        self.stream
            .write_all(packet.as_bytes())
            .await
            .context("send handshake packet")?;
        self.stream.flush().await?;
        self.stream
            .read_exact(packet.as_bytes_mut())
            .await
            .context("read handshake packet")?;
        Ok(packet)
    }

    pub async fn read_frame(&mut self) -> crate::Result<Option<Frame>> {
        loop {
            if let Some(frame) = self.parse_frame()? {
                return Ok(Some(frame));
            }

            if 0 == self.stream.read_buf(&mut self.buf).await? {
                if self.buf.is_empty() {
                    return Ok(None);
                } else {
                    bail!("connection reset by peer")
                }
            }
        }
    }

    fn parse_frame(&mut self) -> crate::Result<Option<Frame>> {
        if self.buf.len() < U32_SIZE {
            // Not enough data to read length marker.
            return Ok(None);
        }

        // Read length marker, this should not fail since we know we have 4 bytes in the buffer.
        let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len == 0 {
            // `KeepAlive` messsage, skip length marker and continue parsing since
            // we may still have bytes left in the buffer.
            let _ = self.buf.get_u32(); // self.buf.advance(4);
            return self.parse_frame();
        }

        // Check that the length is not too large to avoid a denial of
        // service attack where the server runs out of memory.
        if len > FRAME_MAX {
            bail!("protocol error; frame of length {len} is too large.")
            /* return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Frame of length {} is too large.", len),
            )); */
        }

        if self.buf.len() < U32_SIZE + len {
            // The full data has not yet arrived.
            //
            // We reserve more space in the buffer. This is not strictly
            // necessary, but is a good idea performance-wise.
            self.buf.reserve(U32_SIZE + len - self.buf.len());

            // We need more bytes to form the next frame.
            return Ok(None);
        }

        // Skip length marker, it has already been parsed.
        self.buf.advance(U32_SIZE);

        let frame = match self.buf.get_u8() {
            0 => Frame::Choke,
            1 => Frame::Unchoke,
            2 => Frame::Interested,
            3 => Frame::NotInterested,
            4 => {
                let index = self.buf.get_u32();
                Frame::Have(index)
            }
            5 => {
                let bitfield = self.buf.split_to(len - 1).freeze();
                Frame::Bitfield(bitfield)
            }
            6 => Frame::Request {
                index: self.buf.get_u32(),
                begin: self.buf.get_u32(),
                length: self.buf.get_u32(),
            },
            7 => Frame::Piece {
                index: self.buf.get_u32(),
                begin: self.buf.get_u32(),
                chunk: self.buf.split_to(len - 9).freeze(),
            },
            8 => Frame::Cancel {
                index: self.buf.get_u32(),
                begin: self.buf.get_u32(),
                length: self.buf.get_u32(),
            },
            // TODO: Implemenet custom protocol error.
            n => bail!("protocol error; invalid message kind {n}"),
        };

        Ok(Some(frame))
    }

    pub async fn write_frame(&mut self, frame: &Frame) -> crate::Result<()> {
        match frame {
            Frame::Have(index) => {
                self.stream.write_u32(5).await?;
                self.stream.write_u8(4).await?;
                self.stream.write_u32(*index).await?;
            }
            Frame::Bitfield(bitfield) => {
                self.stream.write_u32((1 + bitfield.len()) as u32).await?;
                self.stream.write_u8(u8::from(frame)).await?;
                self.stream.write_all(bitfield).await?;
            }
            Frame::Request {
                index,
                begin,
                length,
            } => {
                self.stream.write_u32(13).await?;
                self.stream.write_u8(u8::from(frame)).await?;
                self.stream.write_u32(*index).await?;
                self.stream.write_u32(*begin).await?;
                self.stream.write_u32(*length).await?;
            }
            Frame::Piece {
                index,
                begin,
                chunk,
            } => {
                self.stream.write_u32((9 + chunk.len()) as u32).await?;
                self.stream.write_u8(u8::from(frame)).await?;
                self.stream.write_u32(*index).await?;
                self.stream.write_u32(*begin).await?;
                self.stream.write_all(chunk).await?;
            }
            Frame::Cancel {
                index,
                begin,
                length,
            } => {
                self.stream.write_u32(13).await?;
                self.stream.write_u8(u8::from(frame)).await?;
                self.stream.write_u32(*index).await?;
                self.stream.write_u32(*begin).await?;
                self.stream.write_u32(*length).await?;
            }
            // `Choke`, `Unchoke`, `Interested`, and 'NotInterested' have no payload.
            frame => {
                self.stream.write_u32(1).await?;
                self.stream.write_u8(u8::from(frame)).await?;
            }
        };

        self.stream.flush().await?;
        Ok(())
    }
}

impl From<&Frame> for u8 {
    fn from(value: &Frame) -> Self {
        use Frame::*;
        match value {
            Choke => 0,
            Unchoke => 1,
            Interested => 2,
            NotInterested => 3,
            Have(_) => 4,
            Bitfield(_) => 5,
            Request { .. } => 6,
            Piece { .. } => 7,
            Cancel { .. } => 8,
        }
    }
}
