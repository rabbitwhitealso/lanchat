//! lan-core：局域网文字/文件/文件夹传输核心库
//!
//! v2：设备ID = Ed25519 公钥（改名、换 IP 不影响好友）；握手带签名验证；
//! UDP 广播发现；聊天记录存储；本地时间显示。

pub mod crypto;
pub mod discovery;
pub mod error;
pub mod history;
pub mod netinfo;
pub mod node;
pub mod osrand;
pub mod store;
pub mod timeutil;
pub mod transfer;
pub mod wire;

pub use discovery::{Discovery, Found};
pub use error::Error;
pub use history::{History, Kind, Record};
pub use node::{Handler, Node, PeerInfo};
pub use store::{Friend, FriendStore, Identity};
pub use transfer::OfferSummary;
