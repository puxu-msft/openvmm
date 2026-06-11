// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! sans-IO 线消息类型。
//!
//! [`WireMessage`] = vfio-user 消息的**纯数据**部分（header + payload），
//! 不含任何 IO 句柄。fd（SCM_RIGHTS 传递的 OwnedFd）由 transport 端的
//! `Message` 包装，因为 fd 是平台 IO 资源、wire crate 不该见。
//!
//! 抽自 `vfio_user_transport::framing::Message`（W0.5，2026-06-11）——为
//! W1 underhill client 复用 wire 类型 + sans-IO 序列化做准备。

use crate::proto::Header;

/// vfio-user 线消息的纯数据部分（header + payload），不含 fd。
///
/// transport 端用 `Message { wire: WireMessage, fds }` 包装并经 `Deref`
/// 暴露 `header` / `payload`。
#[derive(Clone)]
pub struct WireMessage {
    /// 16 字节定长消息头。
    pub header: Header,
    /// payload 字节（不含 header）。
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for WireMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireMessage")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}
