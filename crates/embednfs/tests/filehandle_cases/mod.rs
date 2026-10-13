use bytes::BytesMut;
use embednfs_proto::xdr::XdrEncode;
use embednfs_proto::*;
use tokio::net::TcpStream;

use crate::common::*;

mod handle_state;
mod lookup;
mod persistent;
