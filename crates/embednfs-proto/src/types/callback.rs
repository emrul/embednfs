use bytes::{Bytes, BytesMut};

use crate::xdr::*;

use super::basic::*;
use super::constants::{OP_CB_RECALL, OP_CB_SEQUENCE};

/// Arguments for `CB_SEQUENCE`.
#[derive(Debug, Clone)]
pub struct CbSequenceArgs4 {
    pub sessionid: Sessionid4,
    pub sequenceid: Sequenceid4,
    pub slotid: Slotid4,
    pub highest_slotid: Slotid4,
    pub cachethis: bool,
}

impl XdrEncode for CbSequenceArgs4 {
    fn encode(&self, dst: &mut BytesMut) {
        dst.extend_from_slice(&self.sessionid);
        self.sequenceid.encode(dst);
        self.slotid.encode(dst);
        self.highest_slotid.encode(dst);
        self.cachethis.encode(dst);
        0u32.encode(dst);
    }
}

/// Successful result payload for `CB_SEQUENCE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbSequenceResOk4 {
    pub sessionid: Sessionid4,
    pub sequenceid: Sequenceid4,
    pub slotid: Slotid4,
    pub highest_slotid: Slotid4,
    pub target_highest_slotid: Slotid4,
}

impl XdrDecode for CbSequenceResOk4 {
    fn decode(src: &mut Bytes) -> XdrResult<Self> {
        let sessionid = decode_fixed_opaque(src, 16)?;
        let mut session = [0u8; 16];
        session.copy_from_slice(&sessionid);
        Ok(Self {
            sessionid: session,
            sequenceid: Sequenceid4::decode(src)?,
            slotid: Slotid4::decode(src)?,
            highest_slotid: Slotid4::decode(src)?,
            target_highest_slotid: Slotid4::decode(src)?,
        })
    }
}

/// Arguments for `CB_RECALL`.
#[derive(Debug, Clone)]
pub struct CbRecallArgs4 {
    pub stateid: Stateid4,
    pub truncate: bool,
    pub fh: NfsFh4,
}

impl XdrEncode for CbRecallArgs4 {
    fn encode(&self, dst: &mut BytesMut) {
        self.stateid.encode(dst);
        self.truncate.encode(dst);
        self.fh.encode(dst);
    }
}

/// A callback operation sent by the server.
#[derive(Debug, Clone)]
pub enum NfsCbArgop4 {
    Sequence(CbSequenceArgs4),
    Recall(CbRecallArgs4),
}

impl XdrEncode for NfsCbArgop4 {
    fn encode(&self, dst: &mut BytesMut) {
        match self {
            NfsCbArgop4::Sequence(args) => {
                OP_CB_SEQUENCE.encode(dst);
                args.encode(dst);
            }
            NfsCbArgop4::Recall(args) => {
                OP_CB_RECALL.encode(dst);
                args.encode(dst);
            }
        }
    }
}

/// A callback operation result returned by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NfsCbResop4 {
    Sequence(NfsStat4, Option<CbSequenceResOk4>),
    Recall(NfsStat4),
}

impl XdrDecode for NfsCbResop4 {
    fn decode(src: &mut Bytes) -> XdrResult<Self> {
        match u32::decode(src)? {
            OP_CB_SEQUENCE => {
                let status = NfsStat4::decode(src)?;
                let res = if status == NfsStat4::Ok {
                    Some(CbSequenceResOk4::decode(src)?)
                } else {
                    None
                };
                Ok(NfsCbResop4::Sequence(status, res))
            }
            OP_CB_RECALL => Ok(NfsCbResop4::Recall(NfsStat4::decode(src)?)),
            op => Err(XdrError::InvalidEnum(op)),
        }
    }
}

/// A `CB_COMPOUND` request.
#[derive(Debug, Clone)]
pub struct CbCompound4Args {
    pub tag: String,
    pub minorversion: u32,
    pub callback_ident: u32,
    pub argarray: Vec<NfsCbArgop4>,
}

impl XdrEncode for CbCompound4Args {
    fn encode(&self, dst: &mut BytesMut) {
        self.tag.encode(dst);
        self.minorversion.encode(dst);
        self.callback_ident.encode(dst);
        (self.argarray.len() as u32).encode(dst);
        for op in &self.argarray {
            op.encode(dst);
        }
    }
}

/// A `CB_COMPOUND` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CbCompound4Res {
    pub status: NfsStat4,
    pub tag: String,
    pub resarray: Vec<NfsCbResop4>,
}

impl XdrDecode for CbCompound4Res {
    fn decode(src: &mut Bytes) -> XdrResult<Self> {
        let status = NfsStat4::decode(src)?;
        let tag = String::decode(src)?;
        let len = u32::decode(src)? as usize;
        let mut resarray = Vec::with_capacity(len);
        for _ in 0..len {
            resarray.push(NfsCbResop4::decode(src)?);
        }
        Ok(Self {
            status,
            tag,
            resarray,
        })
    }
}
