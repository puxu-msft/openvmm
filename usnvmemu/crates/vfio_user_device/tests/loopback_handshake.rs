// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W1 loopback 集成测：真 `vfio_user_transport::server_handshake` ⇄ client
//! `VfioUserClient::handshake`，验证协商一致。socketpair，Linux 直跑，无需 VTL。

use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;

/// happy path：client 握手成功，拿到 server major/minor/caps。
#[test]
fn loopback_handshake_succeeds() {
    let (server_end, client_end) = UnixStream::pair().unwrap();

    // server 半段在线程跑真 server_handshake。
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        vfio_user_transport::server_handshake(&mut s)?;
        Ok(())
    });

    let mut client = VfioUserClient::from_stream(client_end);
    let negotiated = client.handshake().expect("client handshake should succeed");

    // server 回 PROTOCOL_MAJOR + min(client_minor, server_minor)。
    assert_eq!(negotiated.server_major, 0, "PROTOCOL_MAJOR");
    assert!(
        negotiated.server_minor <= 1,
        "server_minor 应 ≤ client 提议（PROTOCOL_MINOR=1）"
    );
    assert!(
        negotiated.server_caps_json.contains("capabilities"),
        "server caps JSON 应含 capabilities：{}",
        negotiated.server_caps_json
    );

    server.join().unwrap().expect("server_handshake should succeed");
}
