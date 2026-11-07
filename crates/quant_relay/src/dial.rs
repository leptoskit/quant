//! Dialing a relay, and carrying a byte stream over WebSocket frames.
//!
//! Both the daemon (its listener, and each accepted connection) and clients
//! dial *out* to the relay, so one dialer serves everyone. The control frames
//! of the handshake are JSON text; after that, [`WsStream`] presents the
//! binary frames as an ordinary byte stream, which is what TLS runs over.

use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures_util::SinkExt;
use futures_util::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Bytes;
use tokio_tungstenite::tungstenite::Message;

use crate::proto::AcceptAnswer;
use crate::proto::Challenge;

/// A dialed relay socket.
pub type Dial = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How much of a write becomes one binary frame; the relay's frame limit is
/// a mebibyte, and this leaves room for any transport overhead.
const CHUNK: usize = 256 * 1024;

/// Open a WebSocket to `url` (`ws://` or `wss://`).
pub async fn dial(url: &str) -> io::Result<Dial> {
	let (socket, _) = connect_async(url)
		.await
		.map_err(|error| io::Error::other(format!("cannot reach the relay at {url}: {error}")))?;
	Ok(socket)
}

/// Receive the next text frame as JSON, skipping pings. `Ok(None)` means the
/// socket closed.
pub async fn recv_text<T: DeserializeOwned>(socket: &mut Dial) -> io::Result<Option<T>> {
	loop {
		match socket.next().await {
			Some(Ok(Message::Text(text))) => {
				return serde_json::from_str(&text)
					.map(Some)
					.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
			}
			Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
			Some(Ok(Message::Close(_))) | None => return Ok(None),
			Some(Ok(Message::Binary(_) | Message::Frame(_))) => {
				return Err(io::Error::new(
					io::ErrorKind::InvalidData,
					"the relay sent binary where text was expected",
				));
			}
			Some(Err(error)) => return Err(io::Error::other(error)),
		}
	}
}

/// Send one value as a JSON text frame.
pub async fn send_text<T: Serialize>(socket: &mut Dial, value: &T) -> io::Result<()> {
	let text = serde_json::to_string(value).map_err(io::Error::other)?;
	socket
		.send(Message::Text(text.into()))
		.await
		.map_err(io::Error::other)
}

/// Send a ping, which is what keeps the listener's connection alive through
/// NATs and proxies.
pub async fn ping(socket: &mut Dial) -> io::Result<()> {
	socket
		.send(Message::Ping(Bytes::new()))
		.await
		.map_err(io::Error::other)
}

/// Close the socket politely.
pub async fn close(socket: &mut Dial) {
	let _ = socket.close(None).await;
}

/// Run the listen handshake: read the challenge, answer signed by `key` for
/// `host`. Returns the challenge bytes and the answer that was sent.
pub async fn listen_handshake(
	socket: &mut Dial,
	host: &str,
	key: &crate::key::RelayKey,
	invite: Option<&str>,
) -> io::Result<()> {
	let challenge: Challenge = recv_text(socket)
		.await?
		.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "relay closed"))?;
	let bytes = crate::proto::challenge_bytes(&challenge.challenge)
		.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad relay challenge"))?;
	let mut answer = key.sign_listen(host, &bytes);
	answer.invite = invite.map(ToOwned::to_owned);
	send_text(socket, &answer).await
}

/// Run the accept handshake for `conn`, as `key` for `host`. On success the
/// socket is authenticated and ready to carry bytes.
pub async fn accept_handshake(
	socket: &mut Dial,
	host: &str,
	conn: &str,
	key: &crate::key::RelayKey,
) -> io::Result<()> {
	let challenge: Challenge = recv_text(socket)
		.await?
		.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "relay closed"))?;
	let bytes = crate::proto::challenge_bytes(&challenge.challenge)
		.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad relay challenge"))?;
	let answer: AcceptAnswer = key.sign_accept(host, conn, &bytes);
	send_text(socket, &answer).await
}

/// A dialed socket as a byte stream. Reads concatenate binary frames;
/// writes are split into frames no larger than the relay accepts.
pub struct WsStream {
	socket: Dial,
	pending: Bytes,
	closed: bool,
}

impl WsStream {
	/// Wrap a dialed socket.
	pub fn new(socket: Dial) -> Self {
		Self {
			socket,
			pending: Bytes::new(),
			closed: false,
		}
	}
}

impl tokio::io::AsyncRead for WsStream {
	fn poll_read(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		out: &mut tokio::io::ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		loop {
			if !self.pending.is_empty() {
				let take = out.remaining().min(self.pending.len());
				let end = take;
				out.put_slice(&self.pending[..end]);
				let rest = self.pending.split_off(end);
				self.pending = rest;
				return Poll::Ready(Ok(()));
			}
			if self.closed {
				return Poll::Ready(Ok(()));
			}
			match self.socket.poll_next_unpin(cx) {
				Poll::Ready(Some(Ok(Message::Binary(bytes)))) => self.pending = bytes,
				Poll::Ready(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => {}
				Poll::Ready(Some(Ok(Message::Close(_))) | None) => {
					self.closed = true;
					return Poll::Ready(Ok(()));
				}
				Poll::Ready(Some(Ok(Message::Text(_)))) => {
					return Poll::Ready(Err(io::Error::new(
						io::ErrorKind::InvalidData,
						"text frame inside a spliced stream",
					)));
				}
				Poll::Ready(Some(Err(error))) => {
					return Poll::Ready(Err(io::Error::other(error)));
				}
				Poll::Pending => return Poll::Pending,
			}
		}
	}
}

impl tokio::io::AsyncWrite for WsStream {
	fn poll_write(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		if buf.is_empty() {
			return Poll::Ready(Ok(0));
		}
		if let Err(error) = std::task::ready!(self.socket.poll_ready_unpin(cx)) {
			return Poll::Ready(Err(io::Error::other(error)));
		}
		let take = buf.len().min(CHUNK);
		let frame = Message::Binary(Bytes::copy_from_slice(&buf[..take]));
		if let Err(error) = self.socket.start_send_unpin(frame) {
			return Poll::Ready(Err(io::Error::other(error)));
		}
		Poll::Ready(Ok(take))
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
	}

	fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		if !self.closed {
			if let Err(error) = self.socket.start_send_unpin(Message::Close(None)) {
				return Poll::Ready(Err(io::Error::other(error)));
			}
			self.closed = true;
		}
		self.socket.poll_flush_unpin(cx).map_err(io::Error::other)
	}
}
