use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    /// 网络或磁盘 IO 错误
    Io(io::Error),
    /// 本地原因（路径不合法、文件在发送中变化等）
    Local(String),
    /// 对方发来了不符合协议的数据
    Protocol(String),
    /// 对方明确返回的错误
    Remote { code: u16, msg: String },
    /// 对方拒绝（好友请求或文件接收）
    Rejected,
    /// 好友信息（名字/IP/设备ID）对不上，需要重新添加
    FriendInvalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "网络/磁盘错误: {e}"),
            Error::Local(m) => write!(f, "{m}"),
            Error::Protocol(m) => write!(f, "协议错误: {m}"),
            Error::Remote { code, msg } => write!(f, "对方返回错误({code}): {msg}"),
            Error::Rejected => write!(f, "对方拒绝了请求"),
            Error::FriendInvalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
