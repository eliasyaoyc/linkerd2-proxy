use bytes::{
    buf::{Buf, BufMut},
    BytesMut,
};
use linkerd2_dns_name::Name;
use linkerd2_error::Error;
use linkerd2_io::{self as io, AsyncReadExt};
use linkerd2_proxy_transport::Detect;
use prost::Message;
use std::str::FromStr;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/header.proxy.l5d.io.rs"));
}

#[derive(Clone, Debug)]
pub struct Header {
    /// The target port.
    port: u16,

    /// The logical name of the target (service), if one is known.
    pub name: Option<Name>,
}

#[derive(Clone, Debug, Default)]
pub struct DetectHeader(());

const PREFACE: &[u8] = b"proxy.l5d.io/connect\r\n\r\n";
const PREFACE_LEN: usize = PREFACE.len() + 4;

#[async_trait::async_trait]
impl Detect for DetectHeader {
    type Protocol = Header;

    #[inline]
    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Option<Header>, Error> {
        let header = Header::read_prefaced(io, buf).await?;
        Ok(header)
    }
}

impl Header {
    /// Encodes the connection header to a byte buffer.
    #[inline]
    pub fn encode_prefaced(&self, buf: &mut BytesMut) -> Result<(), Error> {
        buf.reserve(PREFACE_LEN);
        buf.put(PREFACE);

        debug_assert!(buf.capacity() >= 4);
        // Safety: These bytes must be initialized below once the message has
        // been encoded.
        unsafe {
            buf.advance_mut(4);
        }

        self.encode(buf)?;

        // Once the message length is known, we back-fill the length at the
        // start of the buffer.
        let len = buf.len() - PREFACE_LEN;
        assert!(len <= std::u32::MAX as usize);
        {
            let mut buf = &mut buf[PREFACE.len()..PREFACE_LEN];
            buf.put_u32(len as u32);
        }

        Ok(())
    }

    #[inline]
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), Error> {
        let header = proto::Header {
            port: self.port as i32,
            name: self
                .name
                .as_ref()
                .map(|n| n.to_string())
                .unwrap_or_default(),
        };
        header.encode(buf)?;
        Ok(())
    }

    /// Attempts to decode a connection header from an I/O stream.
    ///
    /// If the header is not present, the non-header bytes that were read are
    /// returned.
    ///
    /// An I/O error is returned if the connection header is invalid.
    #[inline]
    async fn read_prefaced<I: io::AsyncRead + Unpin + 'static>(
        io: &mut I,
        buf: &mut BytesMut,
    ) -> io::Result<Option<Self>> {
        // Read at least enough data to determine whether a connection header is
        // present and, if so, how long it is.
        while buf.len() < PREFACE_LEN {
            if io.read_buf(buf).await? == 0 {
                return Ok(None);
            }
        }

        // Advance the buffer past the preface if it matches.
        if &buf.bytes()[..PREFACE.len()] != PREFACE {
            return Ok(None);
        }
        buf.advance(PREFACE.len());

        // Read the message length. If it is larger than our allowed buffer
        // capacity, fail the connection.
        let msg_len = buf.get_u32() as usize;
        if msg_len > buf.capacity() + PREFACE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message length exceeds capacity",
            ));
        }

        // Free up parsed preface data and ensure there's enough capacity for
        // the message.
        buf.reserve(msg_len);
        while buf.len() < msg_len {
            if io.read_buf(buf).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Full header message not provided",
                ));
            }
        }

        // Take the bytes needed to parse the message and leave the remaining
        // bytes in the caller-provided buffer.
        let msg = buf.split_to(msg_len);
        Self::decode(msg.freeze())
    }

    // Decodes a protobuf message from the buffer.
    #[inline]
    fn decode<B: Buf>(buf: B) -> io::Result<Option<Self>> {
        let h = proto::Header::decode(buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid header message"))?;

        let name = if h.name.is_empty() {
            None
        } else {
            let n = Name::from_str(&h.name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid name"))?;
            Some(n)
        };

        Ok(Some(Self {
            name,
            port: h.port as u16,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_prefaced() {
        let header = Header {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
        };
        let mut rx = {
            let mut buf = BytesMut::new();
            header.encode_prefaced(&mut buf).expect("must encode");
            buf.put_slice(b"12345");
            std::io::Cursor::new(buf.freeze())
        };
        let mut buf = BytesMut::new();
        let h = Header::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("decodes")
            .expect("decodes");
        assert_eq!(header.port, h.port);
        assert_eq!(header.name, h.name);
        assert_eq!(buf.as_ref(), b"12345");
    }

    #[tokio::test]
    async fn detect_prefaced() {
        let header = Header {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
        };
        let mut rx = {
            let mut buf = BytesMut::new();
            header.encode_prefaced(&mut buf).expect("must encode");
            buf.put_slice(b"12345");
            std::io::Cursor::new(buf.freeze())
        };
        let mut buf = BytesMut::new();
        let h = DetectHeader::default()
            .detect(&mut rx, &mut buf)
            .await
            .expect("must decode")
            .expect("must decode");
        assert_eq!(header.port, h.port);
        assert_eq!(header.name, h.name);
        assert_eq!(&buf[..], b"12345");
    }

    #[tokio::test]
    async fn detect_no_header() {
        const MSG: &'static [u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (mut rx, _tx) = tokio_test::io::Builder::new().read(MSG).build_with_handle();
        let mut buf = BytesMut::new();
        let h = DetectHeader::default()
            .detect(&mut rx, &mut buf)
            .await
            .expect("must not fail");
        assert!(h.is_none(), "must not decode");
        assert_eq!(&buf[..], MSG);
    }

    #[tokio::test]
    async fn many_reads() {
        let header = Header {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
        };
        let mut rx = {
            let msg = {
                let mut buf = BytesMut::new();
                header.encode(&mut buf).expect("must encode");
                buf.freeze()
            };
            let len = {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(msg.len() as u32);
                buf.freeze()
            };
            tokio_test::io::Builder::new()
                .read(b"proxy.l5d")
                .read(b".io/connect")
                .read(b"\r\n\r\n")
                .read(len.as_ref())
                .read(msg.as_ref())
                .read(b"12345")
                .build()
        };
        let mut buf = BytesMut::new();
        let h = Header::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("I/O must not error")
            .expect("header must be present");
        assert_eq!(header.port, h.port);
        assert_eq!(header.name, h.name);

        let mut buf = [0u8; 5];
        rx.read_exact(&mut buf)
            .await
            .expect("I/O must still have data");
        assert_eq!(&buf, b"12345");
    }
}