//! Fixed, allocation-free control framing. One host transaction at a time.
use heapless::Vec;

pub const MAX_REQUEST: usize = 64;
pub const MAX_RESPONSE: usize = 100;
pub const PARTIAL_PACKET_TIMEOUT_MS: u64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    GetInfo,
    GetRxStats,
    GetStatus,
    ResetStream,
    GetQueues,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Timeout = 0x10,
    Invalid = 0x11,
    Busy = 0x12,
    Transport = 0x13,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Packet {
    pub id: u8,
    pub command: Result<Command, Error>,
}

pub fn decode(bytes: &[u8]) -> Result<Command, Error> {
    // ID, bus 0, system page 0, exact command, no payload. All old GPIO,
    // fan, lease and board-safety commands are unsupported on 1004.
    match bytes {
        [_, 0, 0, 1] => Ok(Command::GetInfo),
        [_, 0, 0, 2] => Ok(Command::GetRxStats),
        [_, 0, 0, 3] => Ok(Command::GetStatus),
        [_, 0, 0, 4] => Ok(Command::ResetStream),
        [_, 0, 0, 5] => Ok(Command::GetQueues),
        _ => Err(Error::Invalid),
    }
}

pub fn extract(buf: &mut [u8; MAX_REQUEST], length: &mut usize) -> Option<Packet> {
    if *length < 2 {
        return None;
    }
    let count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let id = if *length >= 3 { buf[2] } else { 0xff };
    if !(6..=MAX_REQUEST).contains(&count) {
        *length = 0;
        return Some(Packet { id, command: Err(Error::Invalid) });
    }
    if *length < count {
        return None;
    }
    let command = decode(&buf[2..count]);
    buf.copy_within(count..*length, 0);
    *length -= count;
    Some(Packet { id, command })
}

pub fn response(id: u8, payload: &[u8]) -> Vec<u8, MAX_RESPONSE> {
    assert!(payload.len() + 3 <= MAX_RESPONSE);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((payload.len() + 3) as u16).to_le_bytes()).unwrap();
    bytes.push(id).unwrap();
    bytes.extend_from_slice(payload).unwrap();
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_transport_commands_on_bus_zero_are_accepted() {
        for (code, command) in [(1, Command::GetInfo), (2, Command::GetRxStats), (3, Command::GetStatus), (4, Command::ResetStream), (5, Command::GetQueues)] {
            assert_eq!(decode(&[42, 0, 0, code]), Ok(command));
            assert_eq!(decode(&[42, 1, 0, code]), Err(Error::Invalid));
            assert_eq!(decode(&[42, 0, 0, code, 0]), Err(Error::Invalid));
        }
        for code in 0x10..=0x14 {
            assert_eq!(decode(&[42, 0, 0, code]), Err(Error::Invalid));
        }
        for page in [6, 9] {
            assert_eq!(decode(&[42, 0, page, 1]), Err(Error::Invalid));
        }
        assert_eq!(decode(&[]), Err(Error::Invalid));
    }

    #[test]
    fn fragmented_and_coalesced_packets_preserve_ids_and_order() {
        let bytes = [6, 0, 42, 0, 0, 3, 6, 0, 43, 0, 0, 4];
        for split in 0..6 {
            let mut buf = [0; MAX_REQUEST];
            buf[..split].copy_from_slice(&bytes[..split]);
            let mut len = split;
            assert_eq!(extract(&mut buf, &mut len), None);
            buf[split..12].copy_from_slice(&bytes[split..]);
            len = 12;
            assert_eq!(extract(&mut buf, &mut len), Some(Packet { id: 42, command: Ok(Command::GetStatus) }));
            assert_eq!(extract(&mut buf, &mut len), Some(Packet { id: 43, command: Ok(Command::ResetStream) }));
            assert_eq!(len, 0);
        }
    }

    #[test]
    fn invalid_lengths_are_bounded_and_errors_use_standard_framing() {
        for declared in [0u16, 1, 5, 65, 4098, u16::MAX] {
            let mut buf = [0; MAX_REQUEST];
            buf[..2].copy_from_slice(&declared.to_le_bytes());
            buf[2] = 7;
            let mut len = 3;
            assert_eq!(extract(&mut buf, &mut len), Some(Packet { id: 7, command: Err(Error::Invalid) }));
            assert_eq!(len, 0);
        }
        assert_eq!(response(42, &[Error::Invalid as u8]).as_slice(), &[4, 0, 42, 0x11]);
    }
}
