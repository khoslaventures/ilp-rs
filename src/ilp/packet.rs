use super::errors::ParseError;
use byteorder::{BigEndian, ReadBytesExt};
use bytes::BufMut;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use oer::{MutBufOerExt, ReadOerExt};
use std::io::prelude::*;
use std::io::Cursor;
use std::str;

// TODO zero-copy (de)serialization

// Note this format includes a dot before the milliseconds so we need to remove that before using the output
static INTERLEDGER_TIMESTAMP_FORMAT: &'static str = "%Y%m%d%H%M%S%3f";

pub trait Serializable<T> {
    fn from_bytes(bytes: &[u8]) -> Result<T, ParseError>;
    fn to_bytes(&self) -> Vec<u8>;
}

#[derive(Debug, PartialEq, Clone)]
#[repr(u8)]
pub enum PacketType {
    IlpPrepare = 12,
    IlpFulfill = 13,
    IlpReject = 14,
    Unknown,
}
impl From<u8> for PacketType {
    fn from(type_int: u8) -> Self {
        match type_int {
            12 => PacketType::IlpPrepare,
            13 => PacketType::IlpFulfill,
            14 => PacketType::IlpReject,
            _ => PacketType::Unknown,
        }
    }
}

fn serialize_envelope(packet_type: PacketType, contents: &[u8]) -> Vec<u8> {
    // TODO do this mutably so we don't need to copy the data
    let mut buf = Vec::new();
    buf.put_u8(packet_type as u8);
    buf.put_var_octet_string(contents);
    buf
}

fn deserialize_envelope(bytes: &[u8]) -> Result<(PacketType, Vec<u8>), ParseError> {
    let mut reader = Cursor::new(bytes);
    let packet_type = PacketType::from(reader.read_u8()?);
    Ok((packet_type, reader.read_var_octet_string()?))
}

#[derive(Debug, PartialEq, Clone)]
pub enum IlpPacket {
    Prepare(IlpPrepare),
    Fulfill(IlpFulfill),
    Reject(IlpReject),
}

impl Serializable<IlpPacket> for IlpPacket {
    fn from_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        match PacketType::from(bytes[0]) {
            PacketType::IlpPrepare => Ok(IlpPacket::Prepare(IlpPrepare::from_bytes(bytes)?)),
            PacketType::IlpFulfill => Ok(IlpPacket::Fulfill(IlpFulfill::from_bytes(bytes)?)),
            PacketType::IlpReject => Ok(IlpPacket::Reject(IlpReject::from_bytes(bytes)?)),
            _ => Err(ParseError::InvalidPacket(format!(
                "Unknown packet type: {}",
                bytes[0]
            ))),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        match self {
            IlpPacket::Prepare(prepare) => prepare.to_bytes(),
            IlpPacket::Fulfill(fulfill) => fulfill.to_bytes(),
            IlpPacket::Reject(reject) => reject.to_bytes(),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct IlpPrepare {
    pub amount: u64,
    pub expires_at: DateTime<Utc>,
    // TODO make this just a pointer
    pub execution_condition: Bytes,
    pub destination: String,
    pub data: Bytes,
}

impl IlpPrepare {
    pub fn new<A, B, C, D>(
        destination: A,
        amount: u64,
        execution_condition: B,
        expires_at: C,
        data: D,
    ) -> Self
    where
        String: From<A>,
        Bytes: From<B>,
        DateTime<Utc>: From<C>,
        Bytes: From<D>,
    {
        IlpPrepare {
            amount,
            destination: String::from(destination),
            execution_condition: Bytes::from(execution_condition),
            expires_at: DateTime::from(expires_at),
            data: Bytes::from(data),
        }
    }
}

impl Serializable<IlpPrepare> for IlpPrepare {
    fn from_bytes(bytes: &[u8]) -> Result<IlpPrepare, ParseError> {
        let (packet_type, contents) = deserialize_envelope(bytes)?;
        if packet_type != PacketType::IlpPrepare {
            return Err(ParseError::WrongType(String::from(
                "attempted to deserialize other packet type as IlpPrepare",
            )));
        }

        let mut reader = Cursor::new(contents);
        let amount = reader.read_u64::<BigEndian>()?;
        let mut expires_at_buf = [0; 17];
        reader.read_exact(&mut expires_at_buf)?;
        let expires_at_str = String::from_utf8(expires_at_buf.to_vec())?;
        let expires_at = Utc
            .datetime_from_str(&expires_at_str, INTERLEDGER_TIMESTAMP_FORMAT)?
            .with_timezone(&Utc);
        let mut execution_condition: [u8; 32] = [0; 32];
        reader.read_exact(&mut execution_condition)?;
        let destination_bytes = reader.read_var_octet_string()?;
        // TODO make sure address is only ASCII characters
        let destination = String::from_utf8(destination_bytes.to_vec())
            .map_err(|_| ParseError::InvalidPacket(String::from("destination is not utf8")))?;
        let data = Bytes::from(reader.read_var_octet_string()?);
        Ok(IlpPrepare {
            amount,
            expires_at,
            execution_condition: Bytes::from(&execution_condition[..]),
            destination,
            data,
        })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.put_u64_be(self.amount);
        buf.put(
            self.expires_at
                .format(INTERLEDGER_TIMESTAMP_FORMAT)
                .to_string()
                .as_bytes(),
        );
        buf.put(&self.execution_condition);
        buf.put_var_octet_string(&self.destination.to_string().into_bytes());
        buf.put_var_octet_string(&self.data);
        serialize_envelope(PacketType::IlpPrepare, &buf)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct IlpFulfill {
    pub fulfillment: Bytes,
    pub data: Bytes,
}

impl IlpFulfill {
    pub fn new<F, D>(fulfillment: F, data: D) -> Self
    where
        Bytes: From<F>,
        Bytes: From<D>,
    {
        // TODO check fulfillment length
        IlpFulfill {
            fulfillment: Bytes::from(fulfillment),
            data: Bytes::from(data),
        }
    }
}

impl Serializable<IlpFulfill> for IlpFulfill {
    fn from_bytes(bytes: &[u8]) -> Result<IlpFulfill, ParseError> {
        let (packet_type, contents) = deserialize_envelope(bytes)?;
        if packet_type != PacketType::IlpFulfill {
            return Err(ParseError::WrongType(String::from(
                "attempted to deserialize other packet type as IlpFulfill",
            )));
        }

        let mut reader = Cursor::new(contents);
        let mut fulfillment = [0; 32];
        reader.read_exact(&mut fulfillment)?;
        let data = reader.read_var_octet_string()?;
        Ok(IlpFulfill::new(&fulfillment[..], data))
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        buf.put(&self.fulfillment[0..32]);
        buf.put_var_octet_string(&self.data[..]);
        serialize_envelope(PacketType::IlpFulfill, &buf)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct IlpReject {
    pub code: String,
    pub message: String,
    pub triggered_by: String,
    pub data: Bytes,
}

impl IlpReject {
    pub fn new<C, M, T, D>(code: C, message: M, triggered_by: T, data: D) -> Self
    where
        String: From<C>,
        String: From<M>,
        String: From<T>,
        Bytes: From<D>,
    {
        IlpReject {
            code: String::from(code),
            message: String::from(message),
            triggered_by: String::from(triggered_by),
            data: Bytes::from(data),
        }
    }
}

impl Serializable<IlpReject> for IlpReject {
    fn from_bytes(bytes: &[u8]) -> Result<IlpReject, ParseError> {
        let (packet_type, contents) = deserialize_envelope(bytes)?;
        if packet_type != PacketType::IlpReject {
            return Err(ParseError::WrongType(String::from(
                "attempted to deserialize other packet type as IlpReject",
            )));
        }

        let mut reader = Cursor::new(contents);
        let mut code_bytes = [0; 3];
        reader.read_exact(&mut code_bytes)?;
        // TODO: make sure code is valid
        let code = String::from_utf8(code_bytes.to_vec())
            .map_err(|_| ParseError::InvalidPacket(String::from("code is not utf8")))?;
        let triggered_by_bytes = reader.read_var_octet_string()?;
        let triggered_by = String::from_utf8(triggered_by_bytes.to_vec())
            .map_err(|_| ParseError::InvalidPacket(String::from("triggered_by is not utf8")))?;
        let message_bytes = reader.read_var_octet_string()?;
        let message = String::from_utf8(message_bytes.to_vec())
            .map_err(|_| ParseError::InvalidPacket(String::from("message is not utf8")))?;
        let data = Bytes::from(reader.read_var_octet_string()?.to_vec());

        Ok(IlpReject::new(code, message, triggered_by, data))
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        buf.put(self.code.as_bytes());
        buf.put_var_octet_string(self.triggered_by.as_bytes());
        buf.put_var_octet_string(self.message.as_bytes());
        buf.put_var_octet_string(&self.data[..]);
        serialize_envelope(PacketType::IlpReject, &buf)
    }
}

pub struct MaxPacketAmountDetails {
    pub amount_received: u64,
    pub max_amount: u64,
}

pub fn parse_f08_error(reject: &IlpReject) -> Option<MaxPacketAmountDetails> {
    if reject.code == "F08" && reject.data.len() >= 16 {
        let mut cursor = Cursor::new(&reject.data[..]);
        let amount_received = cursor.read_u64::<BigEndian>().ok()?;
        let max_amount = cursor.read_u64::<BigEndian>().ok()?;
        Some(MaxPacketAmountDetails {
            amount_received,
            max_amount,
        })
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) fn create_f08_error(amount_received: u64, max_amount: u64) -> IlpReject {
    let mut data: Vec<u8> = Vec::new();
    data.put_u64_be(amount_received);
    data.put_u64_be(max_amount);
    IlpReject {
        code: String::from("F08"),
        message: String::new(),
        triggered_by: String::new(),
        data: Bytes::from(&data[..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex;

    lazy_static! {
        static ref DATA: Vec<u8> = hex::decode("6c99f6a969473028ef46e09b471581c915b6d5496329c1e3a1c2748d7422a7bdcc798e286cabe3197cccfc213e930b8dba57c7abdf2d1f3b2511689de4f0eff441f53da0feffd23249a355b26c3bd0256d5122e7ccdf159fd6cb083dd73cb29397967871becd04890492119c5e3e6b024be35de26466f60c16d90a21054fb13800120cfb85b0df76e50aacd68526fd043026d3d02010c671987a1f6501b5085f0d7d5897624be5862f98c01df65792970181a87d0f3c586a0ca6bd89dc372c45eef5b38a6307b16f1d7d31e8d92e5982c9dd2986eaad581f212d43da9c5cb7b948fc18914be90219709d0c26d3b5f4ad879d8494bb3aebfe612ec54041e4a380f0").unwrap();
    }

    #[cfg(test)]
    mod ilp_prepare {
        use super::*;

        lazy_static! {
            static ref EXPIRES_AT: DateTime<Utc> = DateTime::parse_from_rfc3339("2018-06-07T20:48:42.483Z").unwrap().with_timezone(&Utc);
            static ref EXECUTION_CONDITION: [u8; 32] = {
                let mut buf = [0; 32];
                buf.copy_from_slice(&hex::decode("117b434f1a54e9044f4f54923b2cff9e4a6d420ae281d5025d7bb040c4b4c04a").unwrap()[..32]);
                buf
            };

            static ref PREPARE_1: IlpPrepare = IlpPrepare {
                amount: 107,
                destination: "example.alice".to_string(),
                expires_at: *EXPIRES_AT,
                execution_condition: Bytes::from(&EXECUTION_CONDITION[..]),
                data: Bytes::from(DATA.to_vec()),
            };
            // TODO find a better way of loading test fixtures
            static ref PREPARE_1_SERIALIZED: Vec<u8> = hex::decode("0c82014b000000000000006b3230313830363037323034383432343833117b434f1a54e9044f4f54923b2cff9e4a6d420ae281d5025d7bb040c4b4c04a0d6578616d706c652e616c6963658201016c99f6a969473028ef46e09b471581c915b6d5496329c1e3a1c2748d7422a7bdcc798e286cabe3197cccfc213e930b8dba57c7abdf2d1f3b2511689de4f0eff441f53da0feffd23249a355b26c3bd0256d5122e7ccdf159fd6cb083dd73cb29397967871becd04890492119c5e3e6b024be35de26466f60c16d90a21054fb13800120cfb85b0df76e50aacd68526fd043026d3d02010c671987a1f6501b5085f0d7d5897624be5862f98c01df65792970181a87d0f3c586a0ca6bd89dc372c45eef5b38a6307b16f1d7d31e8d92e5982c9dd2986eaad581f212d43da9c5cb7b948fc18914be90219709d0c26d3b5f4ad879d8494bb3aebfe612ec54041e4a380f0").unwrap();
        }

        #[test]
        fn from_bytes() {
            assert_eq!(
                IlpPrepare::from_bytes(&PREPARE_1_SERIALIZED).unwrap(),
                *PREPARE_1
            );
        }

        #[test]
        fn to_bytes() {
            assert_eq!(PREPARE_1.to_bytes(), *PREPARE_1_SERIALIZED);
        }
    }

    #[cfg(test)]
    mod ilp_fulfill {
        use super::*;

        lazy_static! {
            static ref FULFILLMENT: [u8; 32] = {
                let mut buf = [0; 32];
                buf.copy_from_slice(&hex::decode("117b434f1a54e9044f4f54923b2cff9e4a6d420ae281d5025d7bb040c4b4c04a").unwrap()[..32]);
                buf
            };

            static ref FULFILL_1: IlpFulfill = IlpFulfill::new(
                &FULFILLMENT[..],
                DATA.to_vec(),
            );
            static ref FULFILL_1_SERIALIZED: Vec<u8> = hex::decode("0d820124117b434f1a54e9044f4f54923b2cff9e4a6d420ae281d5025d7bb040c4b4c04a8201016c99f6a969473028ef46e09b471581c915b6d5496329c1e3a1c2748d7422a7bdcc798e286cabe3197cccfc213e930b8dba57c7abdf2d1f3b2511689de4f0eff441f53da0feffd23249a355b26c3bd0256d5122e7ccdf159fd6cb083dd73cb29397967871becd04890492119c5e3e6b024be35de26466f60c16d90a21054fb13800120cfb85b0df76e50aacd68526fd043026d3d02010c671987a1f6501b5085f0d7d5897624be5862f98c01df65792970181a87d0f3c586a0ca6bd89dc372c45eef5b38a6307b16f1d7d31e8d92e5982c9dd2986eaad581f212d43da9c5cb7b948fc18914be90219709d0c26d3b5f4ad879d8494bb3aebfe612ec54041e4a380f0").unwrap();
        }

        #[test]
        fn from_bytes() {
            assert_eq!(
                IlpFulfill::from_bytes(&FULFILL_1_SERIALIZED).unwrap(),
                *FULFILL_1
            );
        }

        #[test]
        fn to_bytes() {
            assert_eq!(FULFILL_1.to_bytes(), *FULFILL_1_SERIALIZED);
        }
    }

    #[cfg(test)]
    mod ilp_reject {
        use super::*;

        lazy_static! {
            static ref REJECT_1: IlpReject = IlpReject::new(
                "F99",
                "Some error",
                "example.connector",
                DATA.to_vec(),
            );

            static ref REJECT_1_SERIALIZED: Vec<u8> = hex::decode("0e820124463939116578616d706c652e636f6e6e6563746f720a536f6d65206572726f728201016c99f6a969473028ef46e09b471581c915b6d5496329c1e3a1c2748d7422a7bdcc798e286cabe3197cccfc213e930b8dba57c7abdf2d1f3b2511689de4f0eff441f53da0feffd23249a355b26c3bd0256d5122e7ccdf159fd6cb083dd73cb29397967871becd04890492119c5e3e6b024be35de26466f60c16d90a21054fb13800120cfb85b0df76e50aacd68526fd043026d3d02010c671987a1f6501b5085f0d7d5897624be5862f98c01df65792970181a87d0f3c586a0ca6bd89dc372c45eef5b38a6307b16f1d7d31e8d92e5982c9dd2986eaad581f212d43da9c5cb7b948fc18914be90219709d0c26d3b5f4ad879d8494bb3aebfe612ec54041e4a380f0").unwrap();
        }

        #[test]
        fn from_bytes() {
            assert_eq!(
                IlpReject::from_bytes(&REJECT_1_SERIALIZED).unwrap(),
                *REJECT_1
            );
        }

        #[test]
        fn to_bytes() {
            assert_eq!(REJECT_1.to_bytes(), *REJECT_1_SERIALIZED);
        }
    }
}
