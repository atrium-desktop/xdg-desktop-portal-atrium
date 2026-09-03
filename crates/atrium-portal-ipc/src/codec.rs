//! Length-prefixed JSON framing used by Tessera IPC v24.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};
use zeroize::Zeroize as _;

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) fn write_msg<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(json_error)?;
    if bytes.len() > MAX_FRAME {
        bytes.zeroize();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds {MAX_FRAME}", bytes.len()),
        ));
    }
    let length = bytes.len() as u32;
    let result = writer
        .write_all(&length.to_le_bytes())
        .and_then(|()| writer.write_all(&bytes))
        .and_then(|()| writer.flush());
    bytes.zeroize();
    result
}

pub(crate) fn read_msg<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {length} exceeds {MAX_FRAME}"),
        ));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    let result = serde_json::from_slice(&bytes).map_err(json_error);
    bytes.zeroize();
    result
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_round_trips_and_bounds_allocations() {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &serde_json::json!({"type": "Subscribe"})).unwrap();
        let decoded: serde_json::Value = read_msg(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded["type"], "Subscribe");

        let mut oversized = Cursor::new(((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(read_msg::<_, serde_json::Value>(&mut oversized).is_err());
    }

    #[test]
    fn back_to_back_frames_decode_independently() {
        let mut stream = Vec::new();
        write_msg(
            &mut stream,
            &serde_json::json!({"type": "Subscribe", "seq": 1}),
        )
        .unwrap();
        write_msg(&mut stream, &serde_json::json!({"type": "Event", "seq": 2})).unwrap();
        let mut cursor = Cursor::new(stream);
        let first: serde_json::Value = read_msg(&mut cursor).unwrap();
        let second: serde_json::Value = read_msg(&mut cursor).unwrap();
        assert_eq!(first["type"], "Subscribe");
        assert_eq!(first["seq"], 1);
        assert_eq!(second["type"], "Event");
        assert_eq!(second["seq"], 2);
        // The stream is exhausted exactly: no trailing bytes are consumed
        // or left over by the framing.
        assert_eq!(cursor.position() as usize, cursor.get_ref().len());
    }

    #[test]
    fn frame_at_exactly_max_frame_is_accepted() {
        // `{"d":"` + payload + `"}` is 8 bytes of framing around the string.
        let payload = "x".repeat(MAX_FRAME - 8);
        let message = serde_json::json!({"d": payload});
        assert_eq!(serde_json::to_vec(&message).unwrap().len(), MAX_FRAME);

        let mut bytes = Vec::new();
        write_msg(&mut bytes, &message).expect("a MAX_FRAME frame is accepted");
        assert_eq!(&bytes[..4], (MAX_FRAME as u32).to_le_bytes().as_slice());
        assert_eq!(bytes.len(), 4 + MAX_FRAME);
        let decoded: serde_json::Value = read_msg(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn frame_above_max_frame_is_rejected_without_writing() {
        let payload = "x".repeat(MAX_FRAME - 7);
        let message = serde_json::json!({"d": payload});
        assert_eq!(serde_json::to_vec(&message).unwrap().len(), MAX_FRAME + 1);

        let mut bytes = Vec::new();
        let error = write_msg(&mut bytes, &message).expect_err("oversized frames are refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The refused frame leaves the stream untouched: no length prefix,
        // no partial payload.
        assert!(bytes.is_empty());
    }

    #[test]
    fn read_rejects_length_prefix_above_max_frame() {
        let mut oversized = Cursor::new(((MAX_FRAME as u32) + 1).to_le_bytes());
        let error = read_msg::<_, serde_json::Value>(&mut oversized).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The allocation cap fires before any payload is read.
        assert_eq!(oversized.position(), 4);
    }

    #[test]
    fn truncated_length_prefix_is_unexpected_eof() {
        let mut truncated = Cursor::new([0x2a_u8, 0x00]);
        let error = read_msg::<_, serde_json::Value>(&mut truncated).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_payload_is_unexpected_eof() {
        let mut frame = b"\x0a\x00\x00\x00".to_vec();
        frame.extend_from_slice(b"{\"a");
        let mut cursor = Cursor::new(frame);
        let error = read_msg::<_, serde_json::Value>(&mut cursor).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn malformed_json_payload_is_invalid_data() {
        let mut frame = b"\x03\x00\x00\x00".to_vec();
        frame.extend_from_slice(b"nope");
        let mut cursor = Cursor::new(frame);
        let error = read_msg::<_, serde_json::Value>(&mut cursor).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
