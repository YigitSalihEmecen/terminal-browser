//! Wire codec: postcard payloads, optionally zstd-compressed.
//!
//! One WebSocket binary message = `[flag][payload]`. `flag 0`: raw postcard. `flag 1`: a chunk of
//! a *streaming* zstd context that lives as long as the connection. Streaming matters: a single
//! diff is a few hundred bytes (no ratio to speak of alone), but consecutive diffs repeat colours,
//! escape-free text and run headers, which the shared window picks up.
//!
//! Messages under `MIN_COMPRESS` bytes are sent raw and are not fed to the compressor, so both
//! sides' contexts stay in step: the decoder only ever sees what the encoder compressed.

use std::io::Write;

use serde::{de::DeserializeOwned, Serialize};
use zstd::stream::write::{Decoder as ZDecoder, Encoder as ZEncoder};

const RAW: u8 = 0;
const ZSTD: u8 = 1;
pub const MIN_COMPRESS: usize = 96;
/// Largest decoded message we accept (a full 500×200 frame is ~1 MB).
pub const MAX_DECODED: usize = 32 << 20;
/// Compression window: 2^17 = 128 KiB. Bounds memory on both ends and what a peer can make us hold.
const WINDOW_LOG: u32 = 17;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("empty message")]
    Empty,
    #[error("unknown compression flag {0}")]
    BadFlag(u8),
    #[error("message too large after decompression")]
    TooLarge,
    #[error("decompression failed: {0}")]
    Zstd(String),
    #[error("malformed message: {0}")]
    Postcard(#[from] postcard::Error),
}

pub struct Encoder {
    z: ZEncoder<'static, Vec<u8>>,
}

impl Encoder {
    /// `level`: zstd level (1 = fastest, 3 = default, 9+ = small but slower).
    pub fn new(level: i32) -> Result<Self, CodecError> {
        let mut z =
            ZEncoder::new(Vec::new(), level).map_err(|e| CodecError::Zstd(e.to_string()))?;
        z.set_parameter(zstd::zstd_safe::CParameter::WindowLog(WINDOW_LOG))
            .map_err(|e| CodecError::Zstd(e.to_string()))?;
        Ok(Self { z })
    }

    pub fn encode<T: Serialize>(&mut self, msg: &T) -> Result<Vec<u8>, CodecError> {
        let payload = postcard::to_stdvec(msg)?;
        if payload.len() < MIN_COMPRESS {
            let mut out = Vec::with_capacity(payload.len() + 1);
            out.push(RAW);
            out.extend_from_slice(&payload);
            return Ok(out);
        }
        self.z
            .write_all(&payload)
            .map_err(|e| CodecError::Zstd(e.to_string()))?;
        self.z
            .flush()
            .map_err(|e| CodecError::Zstd(e.to_string()))?;
        let chunk = std::mem::take(self.z.get_mut());
        let mut out = Vec::with_capacity(chunk.len() + 1);
        out.push(ZSTD);
        out.extend_from_slice(&chunk);
        Ok(out)
    }
}

/// Output sink that refuses to grow past [`MAX_DECODED`]: the cap holds *while* zstd expands,
/// not after the fact (a few KB of input can legitimately inflate to gigabytes).
#[derive(Default)]
struct Capped(Vec<u8>);

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + buf.len() > MAX_DECODED {
            return Err(std::io::Error::other("decoded message too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Decoder {
    z: ZDecoder<'static, Capped>,
}

impl Decoder {
    pub fn new() -> Result<Self, CodecError> {
        let mut z =
            ZDecoder::new(Capped::default()).map_err(|e| CodecError::Zstd(e.to_string()))?;
        // refuse frames that declare a bigger window than we are willing to allocate
        z.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(WINDOW_LOG))
            .map_err(|e| CodecError::Zstd(e.to_string()))?;
        Ok(Self { z })
    }

    /// Decode one message. After an `Err` from a compressed message the stream state is lost:
    /// the caller must drop the connection.
    pub fn decode<T: DeserializeOwned>(&mut self, bytes: &[u8]) -> Result<T, CodecError> {
        let (&flag, body) = bytes.split_first().ok_or(CodecError::Empty)?;
        match flag {
            RAW => Ok(postcard::from_bytes(body)?),
            ZSTD => {
                let too_large = |e: std::io::Error| {
                    if e.to_string().contains("too large") {
                        CodecError::TooLarge
                    } else {
                        CodecError::Zstd(e.to_string())
                    }
                };
                self.z.write_all(body).map_err(too_large)?;
                self.z.flush().map_err(too_large)?;
                let plain = std::mem::take(&mut self.z.get_mut().0);
                Ok(postcard::from_bytes(&plain)?)
            }
            f => Err(CodecError::BadFlag(f)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diff::full_runs, Grid, ServerMsg, Style};

    fn frame(text: &str) -> ServerMsg {
        let mut g = Grid::new(120, 40, Style::default());
        for y in 0..40 {
            g.put_str(
                2,
                y,
                &format!("{text} line {y} with some words in it to compress"),
                Style::default(),
                120,
            );
        }
        ServerMsg::FullFrame {
            tab: 1,
            seq: 1,
            cols: 120,
            rows: 40,
            runs: full_runs(&g),
        }
    }

    #[test]
    fn small_messages_are_raw_and_big_ones_compress() {
        let mut e = Encoder::new(3).unwrap();
        let small = e.encode(&ServerMsg::Error("x".into())).unwrap();
        assert_eq!(small[0], RAW);
        let f = frame("hello");
        let raw = postcard::to_stdvec(&f).unwrap();
        let big = e.encode(&f).unwrap();
        assert_eq!(big[0], ZSTD);
        assert!(big.len() * 3 < raw.len(), "{} vs {}", big.len(), raw.len());
    }

    #[test]
    fn stream_stays_in_sync_through_mixed_traffic() {
        let (mut e, mut d) = (Encoder::new(3).unwrap(), Decoder::new().unwrap());
        for i in 0..50 {
            let m = if i % 3 == 0 {
                ServerMsg::Error(format!("e{i}"))
            } else {
                frame(&format!("f{i}"))
            };
            let bytes = e.encode(&m).unwrap();
            assert_eq!(d.decode::<ServerMsg>(&bytes).unwrap(), m, "message {i}");
        }
    }

    #[test]
    fn later_frames_compress_better_than_the_first_thanks_to_the_shared_window() {
        let mut e = Encoder::new(3).unwrap();
        let first = e.encode(&frame("same")).unwrap().len();
        let second = e.encode(&frame("same")).unwrap().len();
        assert!(second * 4 < first, "first {first}, repeat {second}");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let mut d = Decoder::new().unwrap();
        assert!(matches!(d.decode::<ServerMsg>(&[]), Err(CodecError::Empty)));
        assert!(matches!(
            d.decode::<ServerMsg>(&[9, 1, 2]),
            Err(CodecError::BadFlag(9))
        ));
        assert!(d.decode::<ServerMsg>(&[RAW, 0xff, 0xff, 0xff]).is_err());
        assert!(d.decode::<ServerMsg>(&[ZSTD, 1, 2, 3, 4]).is_err());
    }

    #[test]
    fn decompression_bombs_are_refused() {
        // a legitimate zstd stream that expands enormously
        let mut z = zstd::stream::write::Encoder::new(Vec::new(), 19).unwrap();
        z.set_parameter(zstd::zstd_safe::CParameter::WindowLog(WINDOW_LOG))
            .unwrap();
        z.write_all(&vec![0u8; MAX_DECODED + (1 << 20)]).unwrap();
        z.flush().unwrap();
        let mut msg = vec![ZSTD];
        msg.extend_from_slice(&std::mem::take(z.get_mut()));
        let mut d = Decoder::new().unwrap();
        assert!(
            matches!(d.decode::<ServerMsg>(&msg), Err(CodecError::TooLarge)),
            "bomb accepted"
        );
    }

    #[test]
    fn client_messages_round_trip_too() {
        use crate::{ClientMsg, KeyCode, KeyEvent, Mods};
        let (mut e, mut d) = (Encoder::new(3).unwrap(), Decoder::new().unwrap());
        for m in [
            ClientMsg::Key(KeyEvent {
                code: KeyCode::Char('é'),
                mods: Mods::CTRL,
            }),
            ClientMsg::Paste("x".repeat(5000)),
            ClientMsg::Navigate {
                url: "https://example.org/?q=日本".into(),
            },
        ] {
            let b = e.encode(&m).unwrap();
            assert_eq!(d.decode::<ClientMsg>(&b).unwrap(), m);
        }
    }
}
