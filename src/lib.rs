// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use futures::future::poll_fn;
use futures::task::noop_waker_ref;
use futures::task::AtomicWaker;
use futures::task::Context;
use futures::task::Poll;
use futures::task::RawWaker;
use futures::task::RawWakerVTable;
use futures::task::Waker;
use tokio::spawn;

use crate::inner::Flow;
use crate::inner::State;
use crate::inner::TlsStreamInner;
use parking_lot::Mutex;
use rustls::ClientConfig;
use rustls::ClientConnection;
use rustls::Connection;
use rustls::ServerConfig;
use rustls::ServerConnection;
use rustls::ServerName;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::net::TcpStream;

mod inner;

pub struct TlsStream(Option<TlsStreamInner>);

impl TlsStream {
  fn new(tcp: TcpStream, mut tls: Connection) -> Self {
    tls.set_buffer_limit(None);

    let inner = TlsStreamInner {
      tcp,
      tls,
      rd_state: State::StreamOpen,
      wr_state: State::StreamOpen,
    };
    Self(Some(inner))
  }

  pub fn new_client_side(
    tcp: TcpStream,
    tls_config: Arc<ClientConfig>,
    server_name: ServerName,
  ) -> Self {
    let tls = ClientConnection::new(tls_config, server_name).unwrap();
    Self::new(tcp, Connection::Client(tls))
  }

  pub fn new_client_side_from(tcp: TcpStream, connection: ClientConnection) -> Self {
    Self::new(tcp, Connection::Client(connection))
  }

  pub fn new_server_side(tcp: TcpStream, tls_config: Arc<ServerConfig>) -> Self {
    let tls = ServerConnection::new(tls_config).unwrap();
    Self::new(tcp, Connection::Server(tls))
  }

  pub fn new_server_side_from(tcp: TcpStream, connection: ServerConnection) -> Self {
    Self::new(tcp, Connection::Server(connection))
  }

  pub fn into_split(self) -> (ReadHalf, WriteHalf) {
    let shared = Shared::new(self);
    let rd = ReadHalf {
      shared: shared.clone(),
    };
    let wr = WriteHalf { shared };
    (rd, wr)
  }

  /// Convenience method to match [`TcpStream`].
  pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
    self.0.as_ref().unwrap().tcp.peer_addr()
  }

  /// Convenience method to match [`TcpStream`].
  pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
    self.0.as_ref().unwrap().tcp.local_addr()
  }

  /// Tokio-rustls compatibility: returns a reference to the underlying TCP
  /// stream, and a reference to the Rustls `Connection` object.
  pub fn get_ref(&self) -> (&TcpStream, &Connection) {
    let inner = self.0.as_ref().unwrap();
    (&inner.tcp, &inner.tls)
  }

  fn inner_mut(&mut self) -> &mut TlsStreamInner {
    self.0.as_mut().unwrap()
  }

  pub async fn handshake(&mut self) -> io::Result<()> {
    poll_fn(|cx| self.inner_mut().poll_handshake(cx)).await
  }

  fn poll_handshake(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    self.inner_mut().poll_handshake(cx)
  }

  pub fn get_alpn_protocol(&mut self) -> Option<&[u8]> {
    self.inner_mut().tls.alpn_protocol()
  }

  pub async fn shutdown(&mut self) -> io::Result<()> {
    poll_fn(|cx| self.inner_mut().poll_shutdown(cx)).await
  }
}

impl AsyncRead for TlsStream {
  fn poll_read(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
  ) -> Poll<io::Result<()>> {
    self.inner_mut().poll_read(cx, buf)
  }
}

impl AsyncWrite for TlsStream {
  fn poll_write(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<io::Result<usize>> {
    self.inner_mut().poll_write(cx, buf)
  }

  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    self.inner_mut().poll_io(cx, Flow::Write)
    // The underlying TCP stream does not need to be flushed.
  }

  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    self.inner_mut().poll_shutdown(cx)
  }
}

impl Drop for TlsStream {
  fn drop(&mut self) {
    let mut inner = self.0.take().unwrap();

    // If read and write are closed, we can fast exit here
    if inner.wr_state != State::StreamOpen && inner.rd_state != State::StreamOpen {
      return;
    }

    let mut cx = Context::from_waker(noop_waker_ref());
    let use_linger_task = inner.poll_close(&mut cx).is_pending();

    if use_linger_task {
      spawn(poll_fn(move |cx: &mut Context| inner.poll_close(cx)));
    } else if cfg!(debug_assertions) {
      spawn(async {}); // Spawn dummy task to detect missing LocalSet.
    }
  }
}

pub struct ReadHalf {
  shared: Arc<Shared>,
}

impl ReadHalf {
  pub fn reunite(self, wr: WriteHalf) -> TlsStream {
    assert!(Arc::ptr_eq(&self.shared, &wr.shared));
    drop(wr); // Drop `wr`, so only one strong reference to `shared` remains.

    Arc::try_unwrap(self.shared)
      .unwrap_or_else(|_| panic!("Arc::<Shared>::try_unwrap() failed"))
      .tls_stream
      .into_inner()
  }
}

impl AsyncRead for ReadHalf {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
  ) -> Poll<io::Result<()>> {
    self
      .shared
      .poll_with_shared_waker(cx, Flow::Read, move |tls, cx| tls.poll_read(cx, buf))
  }
}

pub struct WriteHalf {
  shared: Arc<Shared>,
}

impl WriteHalf {
  pub async fn handshake(&mut self) -> io::Result<()> {
    poll_fn(|cx| {
      self
        .shared
        .poll_with_shared_waker(cx, Flow::Write, |mut tls, cx| tls.poll_handshake(cx))
    })
    .await
  }

  pub async fn shutdown(&mut self) -> io::Result<()> {
    poll_fn(move |cx| {
      self
        .shared
        .poll_with_shared_waker(cx, Flow::Write, |tls, cx| tls.poll_shutdown(cx))
    })
    .await
  }

  pub fn get_alpn_protocol(&self) -> Option<Vec<u8>> {
    self.shared.get_alpn_protocol()
  }
}

impl AsyncWrite for WriteHalf {
  fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
    self
      .shared
      .poll_with_shared_waker(cx, Flow::Write, move |tls, cx| tls.poll_write(cx, buf))
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    self
      .shared
      .poll_with_shared_waker(cx, Flow::Write, |tls, cx| tls.poll_flush(cx))
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    self
      .shared
      .poll_with_shared_waker(cx, Flow::Write, |tls, cx| tls.poll_shutdown(cx))
  }
}

struct Shared {
  tls_stream: Mutex<TlsStream>,
  rd_waker: AtomicWaker,
  wr_waker: AtomicWaker,
}

impl Shared {
  fn new(tls_stream: TlsStream) -> Arc<Self> {
    let self_ = Self {
      tls_stream: Mutex::new(tls_stream),
      rd_waker: AtomicWaker::new(),
      wr_waker: AtomicWaker::new(),
    };
    Arc::new(self_)
  }

  fn poll_with_shared_waker<R>(
    self: &Arc<Self>,
    cx: &mut Context<'_>,
    flow: Flow,
    mut f: impl FnMut(Pin<&mut TlsStream>, &mut Context<'_>) -> R,
  ) -> R {
    match flow {
      Flow::Handshake => unreachable!(),
      Flow::Read => self.rd_waker.register(cx.waker()),
      Flow::Write => self.wr_waker.register(cx.waker()),
    }

    let shared_waker = self.new_shared_waker();
    let mut cx = Context::from_waker(&shared_waker);

    let mut tls_stream = self.tls_stream.lock();
    f(Pin::new(&mut tls_stream), &mut cx)
  }

  const SHARED_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    Self::clone_shared_waker,
    Self::wake_shared_waker,
    Self::wake_shared_waker_by_ref,
    Self::drop_shared_waker,
  );

  fn new_shared_waker(self: &Arc<Self>) -> Waker {
    let self_weak = Arc::downgrade(self);
    let self_ptr = self_weak.into_raw() as *const ();
    let raw_waker = RawWaker::new(self_ptr, &Self::SHARED_WAKER_VTABLE);
    // TODO(bartlomieju):
    #[allow(clippy::undocumented_unsafe_blocks)]
    unsafe {
      Waker::from_raw(raw_waker)
    }
  }

  fn clone_shared_waker(self_ptr: *const ()) -> RawWaker {
    // TODO(bartlomieju):
    #[allow(clippy::undocumented_unsafe_blocks)]
    let self_weak = unsafe { Weak::from_raw(self_ptr as *const Self) };
    let ptr1 = self_weak.clone().into_raw();
    let ptr2 = self_weak.into_raw();
    assert!(ptr1 == ptr2);
    RawWaker::new(self_ptr, &Self::SHARED_WAKER_VTABLE)
  }

  fn wake_shared_waker(self_ptr: *const ()) {
    Self::wake_shared_waker_by_ref(self_ptr);
    Self::drop_shared_waker(self_ptr);
  }

  fn wake_shared_waker_by_ref(self_ptr: *const ()) {
    // TODO(bartlomieju):
    #[allow(clippy::undocumented_unsafe_blocks)]
    let self_weak = unsafe { Weak::from_raw(self_ptr as *const Self) };
    if let Some(self_arc) = Weak::upgrade(&self_weak) {
      self_arc.rd_waker.wake();
      self_arc.wr_waker.wake();
    }
    let _ = self_weak.into_raw();
  }

  fn drop_shared_waker(self_ptr: *const ()) {
    // TODO(bartlomieju):
    #[allow(clippy::undocumented_unsafe_blocks)]
    let _ = unsafe { Weak::from_raw(self_ptr as *const Self) };
  }

  fn get_alpn_protocol(self: &Arc<Self>) -> Option<Vec<u8>> {
    let mut tls_stream = self.tls_stream.lock();
    tls_stream.get_alpn_protocol().map(|s| s.to_vec())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustls::client::ServerCertVerified;
  use rustls::client::ServerCertVerifier;
  use rustls::Certificate;
  use rustls::PrivateKey;
  use std::io::BufRead;
  use std::net::Ipv4Addr;
  use std::net::SocketAddrV4;
  use tokio::io::AsyncReadExt;
  use tokio::io::AsyncWriteExt;
  use tokio::net::TcpListener;
  use tokio::net::TcpSocket;
  use tokio::spawn;

  struct UnsafeVerifier {}

  impl ServerCertVerifier for UnsafeVerifier {
    fn verify_server_cert(
      &self,
      _end_entity: &Certificate,
      _intermediates: &[Certificate],
      _server_name: &ServerName,
      _scts: &mut dyn Iterator<Item = &[u8]>,
      _ocsp_response: &[u8],
      _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
      Ok(ServerCertVerified::assertion())
    }
  }

  fn certificate() -> Certificate {
    let buf_read: &mut dyn BufRead = &mut &include_bytes!("testdata/localhost.crt")[..];
    let cert = rustls_pemfile::read_one(buf_read)
      .expect("Failed to load test cert")
      .unwrap();
    match cert {
      rustls_pemfile::Item::X509Certificate(cert) => Certificate(cert),
      _ => {
        panic!("Unexpected item")
      }
    }
  }

  fn private_key() -> PrivateKey {
    let buf_read: &mut dyn BufRead = &mut &include_bytes!("testdata/localhost.key")[..];
    let cert = rustls_pemfile::read_one(buf_read)
      .expect("Failed to load test key")
      .unwrap();
    match cert {
      rustls_pemfile::Item::PKCS8Key(key) => PrivateKey(key),
      _ => {
        panic!("Unexpected item")
      }
    }
  }

  fn server_config() -> ServerConfig {
    ServerConfig::builder()
      .with_safe_defaults()
      .with_no_client_auth()
      .with_single_cert(vec![certificate()], private_key())
      .expect("Failed to build server config")
  }

  fn client_config() -> ClientConfig {
    ClientConfig::builder()
      .with_safe_defaults()
      .with_custom_certificate_verifier(Arc::new(UnsafeVerifier {}))
      .with_no_client_auth()
  }

  async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
      .await
      .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = spawn(async move { listener.accept().await.unwrap().0 });
    let client = spawn(async move {
      TcpSocket::new_v4()
        .unwrap()
        .connect(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)))
        .await
        .unwrap()
    });

    let (server, client) = (server.await.unwrap(), client.await.unwrap());
    (server, client)
  }

  async fn tls_pair() -> (TlsStream, TlsStream) {
    let (server, client) = tcp_pair().await;
    let server = TlsStream::new_server_side(server, server_config().into());
    let client = TlsStream::new_client_side(
      client,
      client_config().into(),
      "example.com".try_into().unwrap(),
    );

    (server, client)
  }

  #[tokio::test]
  async fn test_client_server() -> Result<(), Box<dyn std::error::Error>> {
    let (mut server, mut client) = tls_pair().await;
    let a = spawn(async move {
      server.write_all(b"hello?").await.unwrap();
      let mut buf = [0; 6];
      server.read_exact(&mut buf).await.unwrap();
      assert_eq!(buf.as_slice(), b"hello!");
    });
    let b = spawn(async move {
      client.write_all(b"hello!").await.unwrap();
      let mut buf = [0; 6];
      client.read_exact(&mut buf).await.unwrap();
      assert_eq!(buf.as_slice(), b"hello?");
    });
    a.await?;
    b.await?;

    Ok(())
  }

  #[tokio::test]
  async fn test_server_shutdown_after_handshake() -> Result<(), Box<dyn std::error::Error>> {
    let (mut server, mut client) = tls_pair().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let a = spawn(async move {
      // Shut down before the handshake
      server.handshake().await.unwrap();
      server.shutdown().await.unwrap();
      tx.send(()).unwrap();
      assert_eq!(
        server
          .write_all(b"hello?")
          .await
          .expect_err("should be shut down")
          .kind(),
        io::ErrorKind::BrokenPipe
      );
    });
    let b = spawn(async move {
      assert!(client.get_ref().1.is_handshaking());
      client.handshake().await.unwrap();
      rx.await.unwrap();
      let mut buf = [0; 6];
      client.read_exact(&mut buf).await.expect_err("early eof");
    });
    a.await?;
    b.await?;

    Ok(())
  }
}
