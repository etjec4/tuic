use self::{
    authenticate::{AuthenticateBroadcast, IsAuthenticated},
    dispatch::DispatchError,
    udp::{RecvPacketReceiver, UdpPacketFrom, UdpPacketSource, UdpSessionMap},
};
use futures_util::StreamExt;
use quinn::{
    Connecting, Connection as QuinnConnection, ConnectionError, Datagrams, IncomingBiStreams,
    IncomingUniStreams, NewConnection,
};
use std::{sync::Arc, time::Duration};
use tokio::time;

mod authenticate;
mod dispatch;
mod task;
mod udp;

#[derive(Clone)]
pub struct Connection {
    controller: QuinnConnection,
    udp_packet_from: UdpPacketFrom,
    udp_sessions: Arc<UdpSessionMap>,
    expected_token_digest: [u8; 32],
    is_authenticated: IsAuthenticated,
    authenticate_broadcast: Arc<AuthenticateBroadcast>,
}

impl Connection {
    pub async fn handle(conn: Connecting, exp_token_dgst: [u8; 32], auth_timeout: Duration) {
        let rmt_addr = conn.remote_address();

        match conn.await {
            Ok(NewConnection {
                connection,
                uni_streams,
                bi_streams,
                datagrams,
                ..
            }) => {
                log::debug!("[{rmt_addr}] [established]");

                let (udp_sessions, recv_pkt_rx) = UdpSessionMap::new();
                let (is_authed, auth_bcast) = IsAuthenticated::new(connection.clone());

                let conn = Self {
                    controller: connection,
                    udp_packet_from: UdpPacketFrom::new(),
                    udp_sessions: Arc::new(udp_sessions),
                    expected_token_digest: exp_token_dgst,
                    is_authenticated: is_authed,
                    authenticate_broadcast: auth_bcast,
                };

                let listen_uni_streams =
                    tokio::spawn(Self::listen_uni_streams(conn.clone(), uni_streams));
                let listen_bi_streams =
                    tokio::spawn(Self::listen_bi_streams(conn.clone(), bi_streams));
                let listen_datagrams =
                    tokio::spawn(Self::listen_datagrams(conn.clone(), datagrams));
                let listen_received_udp_packet =
                    tokio::spawn(Self::listen_received_udp_packet(conn.clone(), recv_pkt_rx));
                let handle_authentication_timeout =
                    tokio::spawn(Self::handle_authentication_timeout(conn, auth_timeout));

                let (
                    listen_uni_streams,
                    listen_bi_streams,
                    listen_datagrams,
                    listen_received_udp_packet,
                    handle_authentication_timeout,
                ) = unsafe {
                    tokio::try_join!(
                        listen_uni_streams,
                        listen_bi_streams,
                        listen_datagrams,
                        listen_received_udp_packet,
                        handle_authentication_timeout
                    )
                    .unwrap_unchecked()
                };

                let res = listen_uni_streams
                    .and(listen_bi_streams)
                    .and(listen_datagrams)
                    .and(listen_received_udp_packet)
                    .and(handle_authentication_timeout);

                match res {
                    Ok(())
                    | Err(ConnectionError::LocallyClosed)
                    | Err(ConnectionError::TimedOut) => log::debug!("[{rmt_addr}] [disconnected]"),
                    Err(err) => log::error!("[{rmt_addr}] {err}"),
                }
            }
            Err(err) => log::error!("[{rmt_addr}] {err}"),
        }
    }

    async fn listen_uni_streams(
        self,
        mut uni_streams: IncomingUniStreams,
    ) -> Result<(), ConnectionError> {
        while let Some(stream) = uni_streams.next().await {
            let stream = stream?;
            let conn = self.clone();

            tokio::spawn(async move {
                match conn.process_uni_stream(stream).await {
                    Ok(()) => {}
                    Err(err) => {
                        conn.controller
                            .close(err.as_error_code(), err.to_string().as_bytes());

                        let rmt_addr = conn.controller.remote_address();
                        log::error!("[{rmt_addr}] {err}");
                    }
                }
            });
        }

        Err(ConnectionError::LocallyClosed)?
    }

    async fn listen_bi_streams(
        self,
        mut bi_streams: IncomingBiStreams,
    ) -> Result<(), ConnectionError> {
        while let Some(stream) = bi_streams.next().await {
            let (send, recv) = stream?;
            let conn = self.clone();

            tokio::spawn(async move {
                match conn.process_bi_stream(send, recv).await {
                    Ok(()) => {}
                    Err(err) => {
                        conn.controller
                            .close(err.as_error_code(), err.to_string().as_bytes());

                        let rmt_addr = conn.controller.remote_address();
                        log::error!("[{rmt_addr}] {err}");
                    }
                }
            });
        }

        Err(ConnectionError::LocallyClosed)?
    }

    async fn listen_datagrams(self, mut datagrams: Datagrams) -> Result<(), ConnectionError> {
        while let Some(datagram) = datagrams.next().await {
            let datagram = datagram?;
            let conn = self.clone();

            tokio::spawn(async move {
                match conn.process_datagram(datagram).await {
                    Ok(()) => {}
                    Err(err) => {
                        conn.controller
                            .close(err.as_error_code(), err.to_string().as_bytes());

                        let rmt_addr = conn.controller.remote_address();
                        log::error!("[{rmt_addr}] {err}");
                    }
                }
            });
        }

        Err(ConnectionError::LocallyClosed)?
    }

    async fn listen_received_udp_packet(
        self,
        mut recv_pkt_rx: RecvPacketReceiver,
    ) -> Result<(), ConnectionError> {
        while let Some((assoc_id, pkt, addr)) = recv_pkt_rx.recv().await {
            let conn = self.clone();

            tokio::spawn(async move {
                match conn.process_received_udp_packet(assoc_id, pkt, addr).await {
                    Ok(()) => {}
                    Err(err) => {
                        conn.controller
                            .close(err.as_error_code(), err.to_string().as_bytes());

                        let rmt_addr = conn.controller.remote_address();
                        log::error!("[{rmt_addr}] {err}");
                    }
                }
            });
        }

        Ok(())
    }

    async fn handle_authentication_timeout(self, timeout: Duration) -> Result<(), ConnectionError> {
        time::sleep(timeout).await;

        if self.is_authenticated.check() {
            Ok(())
        } else {
            let err = DispatchError::AuthenticationTimeout;

            self.controller
                .close(err.as_error_code(), err.to_string().as_bytes());
            self.authenticate_broadcast.wake();

            let rmt_addr = self.controller.remote_address();
            log::error!("[{rmt_addr}] {err}");

            Err(ConnectionError::LocallyClosed)?
        }
    }
}