// Copyright 2017 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Handles entering a connection with a peer.
//!
//! The two main elements of this module are the `Transport` and `ConnectionUpgrade` traits.
//! `Transport` is implemented on objects that allow dialing and listening. `ConnectionUpgrade` is
//! implemented on objects that make it possible to upgrade a connection (for example by adding an
//! encryption middleware to the connection).
//!
//! Thanks to the `Transport::or_transport`, `Transport::with_upgrade` and
//! `UpgradeNode::or_upgrade` methods, you can combine multiple transports and/or upgrades together
//! in a complex chain of protocols negotiation.

use bytes::Bytes;
use futures::{Stream, Poll, Async};
use futures::future::{IntoFuture, Future, ok as future_ok, FutureResult};
use multiaddr::Multiaddr;
use multistream_select;
use std::io::{Cursor, Error as IoError, Read, Write};
use std::iter;
use tokio_io::{AsyncRead, AsyncWrite};

/// A transport is an object that can be used to produce connections by listening or dialing a
/// peer.
///
/// This trait is implemented on concrete transports (eg. TCP, UDP, etc.), but also on wrappers
/// around them.
///
/// > **Note**: The methods of this trait use `self` and not `&self` or `&mut self`. In other
/// >           words, listening or dialing consumes the transport object. This has been designed
/// >           so that you would implement this trait on `&Foo` or `&mut Foo` instead of directly
/// >           on `Foo`.
pub trait Transport {
	/// The raw connection to a peer.
	type RawConn: AsyncRead + AsyncWrite;

	/// The listener produces incoming connections.
	type Listener: Stream<Item = Self::RawConn, Error = IoError>;

	/// A future which indicates that we are currently dialing to a peer.
	type Dial: IntoFuture<Item = Self::RawConn, Error = IoError>;

	/// Listen on the given multi-addr.
	///
	/// Returns the address back if it isn't supported.
	fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, (Self, Multiaddr)>
		where Self: Sized;

	/// Dial to the given multi-addr.
	///
	/// Returns either a future which may resolve to a connection, or gives back the multiaddress.
	fn dial(self, addr: Multiaddr) -> Result<Self::Dial, (Self, Multiaddr)> where Self: Sized;

	/// Builds a new struct that implements `Transport` that contains both `self` and `other`.
	///
	/// The returned object will redirect its calls to `self`, except that if `listen_on` or `dial`
	/// return an error then `other` will be tried.
	#[inline]
	fn or_transport<T>(self, other: T) -> OrTransport<Self, T>
		where Self: Sized
	{
		OrTransport(self, other)
	}

	/// Wraps this transport inside an upgrade. Whenever a connection that uses this transport
	/// is established, it is wrapped inside the upgrade.
	///
	/// > **Note**: The concept of an *upgrade* for example includes middlewares such *secio*
	/// >           (communication encryption), *multiplex*, but also a protocol handler.
	#[inline]
	fn with_upgrade<U>(self, upgrade: U) -> UpgradedNode<Self, U>
		where Self: Sized,
			  U: ConnectionUpgrade<Self::RawConn>
	{
		UpgradedNode {
			transports: self,
			upgrade: upgrade,
		}
	}
}

/// Dummy implementation of `Transport` that just denies every single attempt.
#[derive(Debug, Copy, Clone)]
pub struct DeniedTransport;

impl Transport for DeniedTransport {
	// TODO: could use `!` for associated types once stable
	type RawConn = Cursor<Vec<u8>>;
	type Listener = Box<Stream<Item = Self::RawConn, Error = IoError>>;
	type Dial = Box<Future<Item = Self::RawConn, Error = IoError>>;

	#[inline]
	fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, (Self, Multiaddr)> {
		Err((DeniedTransport, addr))
	}

	#[inline]
	fn dial(self, addr: Multiaddr) -> Result<Self::Dial, (Self, Multiaddr)> {
		Err((DeniedTransport, addr))
	}
}

/// Struct returned by `or_transport()`.
#[derive(Debug, Copy, Clone)]
pub struct OrTransport<A, B>(A, B);

impl<A, B> Transport for OrTransport<A, B>
	where A: Transport,
		  B: Transport
{
	type RawConn = EitherSocket<A::RawConn, B::RawConn>;
	type Listener = EitherStream<A::Listener, B::Listener>;
	type Dial = EitherTransportFuture<
		<A::Dial as IntoFuture>::Future,
		<B::Dial as IntoFuture>::Future,
	>;

	fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, (Self, Multiaddr)> {
		let (first, addr) = match self.0.listen_on(addr) {
			Ok(connec) => return Ok(EitherStream::First(connec)),
			Err(err) => err,
		};

		match self.1.listen_on(addr) {
			Ok(connec) => Ok(EitherStream::Second(connec)),
			Err((second, addr)) => Err((OrTransport(first, second), addr)),
		}
	}

	fn dial(self, addr: Multiaddr) -> Result<Self::Dial, (Self, Multiaddr)> {
		let (first, addr) = match self.0.dial(addr) {
			Ok(connec) => return Ok(EitherTransportFuture::First(connec.into_future())),
			Err(err) => err,
		};

		match self.1.dial(addr) {
			Ok(connec) => Ok(EitherTransportFuture::Second(connec.into_future())),
			Err((second, addr)) => Err((OrTransport(first, second), addr)),
		}
	}
}

/// Implements `Stream` and dispatches all method calls to either `First` or `Second`.
#[derive(Debug, Copy, Clone)]
pub enum EitherStream<A, B> {
	First(A),
	Second(B),
}

impl<A, B> Stream for EitherStream<A, B>
	where A: Stream<Error = IoError>,
		  B: Stream<Error = IoError>
{
	type Item = EitherSocket<A::Item, B::Item>;
	type Error = IoError;

	#[inline]
	fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
		match self {
			&mut EitherStream::First(ref mut a) => {
				a.poll().map(|i| i.map(|v| v.map(EitherSocket::First)))
			}
			&mut EitherStream::Second(ref mut a) => {
				a.poll().map(|i| i.map(|v| v.map(EitherSocket::Second)))
			}
		}
	}
}

/// Implements `Future` and redirects calls to either `First` or `Second`.
///
/// Additionally, the output will be wrapped inside a `EitherSocket`.
///
/// > **Note**: This type is needed because of the lack of `-> impl Trait` in Rust. It can be
/// >           removed eventually.
#[derive(Debug, Copy, Clone)]
pub enum EitherTransportFuture<A, B> {
	First(A),
	Second(B),
}

impl<A, B> Future for EitherTransportFuture<A, B>
	where A: Future<Error = IoError>,
		  B: Future<Error = IoError>
{
	type Item = EitherSocket<A::Item, B::Item>;
	type Error = IoError;

	#[inline]
	fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
		match self {
			&mut EitherTransportFuture::First(ref mut a) => {
				let item = try_ready!(a.poll());
				Ok(Async::Ready(EitherSocket::First(item)))
			}
			&mut EitherTransportFuture::Second(ref mut b) => {
				let item = try_ready!(b.poll());
				Ok(Async::Ready(EitherSocket::Second(item)))
			}
		}
	}
}

/// Implements `AsyncRead` and `AsyncWrite` and dispatches all method calls to either `First` or
/// `Second`.
#[derive(Debug, Copy, Clone)]
pub enum EitherSocket<A, B> {
	First(A),
	Second(B),
}

impl<A, B> AsyncRead for EitherSocket<A, B>
	where A: AsyncRead,
		  B: AsyncRead
{
	#[inline]
	unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
		match self {
			&EitherSocket::First(ref a) => a.prepare_uninitialized_buffer(buf),
			&EitherSocket::Second(ref b) => b.prepare_uninitialized_buffer(buf),
		}
	}
}

impl<A, B> Read for EitherSocket<A, B>
	where A: Read,
		  B: Read
{
	#[inline]
	fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
		match self {
			&mut EitherSocket::First(ref mut a) => a.read(buf),
			&mut EitherSocket::Second(ref mut b) => b.read(buf),
		}
	}
}

impl<A, B> AsyncWrite for EitherSocket<A, B>
	where A: AsyncWrite,
		  B: AsyncWrite
{
	#[inline]
	fn shutdown(&mut self) -> Poll<(), IoError> {
		match self {
			&mut EitherSocket::First(ref mut a) => a.shutdown(),
			&mut EitherSocket::Second(ref mut b) => b.shutdown(),
		}
	}
}

impl<A, B> Write for EitherSocket<A, B>
	where A: Write,
		  B: Write
{
	#[inline]
	fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
		match self {
			&mut EitherSocket::First(ref mut a) => a.write(buf),
			&mut EitherSocket::Second(ref mut b) => b.write(buf),
		}
	}

	#[inline]
	fn flush(&mut self) -> Result<(), IoError> {
		match self {
			&mut EitherSocket::First(ref mut a) => a.flush(),
			&mut EitherSocket::Second(ref mut b) => b.flush(),
		}
	}
}

/// Implemented on structs that describe a possible upgrade to a connection between two peers.
///
/// The generic `C` is the type of the incoming connection before it is upgraded.
///
/// > **Note**: The `upgrade` method of this trait uses `self` and not `&self` or `&mut self`.
/// >           This has been designed so that you would implement this trait on `&Foo` or
/// >           `&mut Foo` instead of directly on `Foo`.
pub trait ConnectionUpgrade<C: AsyncRead + AsyncWrite> {
	/// Iterator returned by `protocol_names`.
	type NamesIter: Iterator<Item = (Bytes, Self::UpgradeIdentifier)>;
	/// Type that serves as an identifier for the protocol. This type only exists to be returned
	/// by the `NamesIter` and then be passed to `upgrade`.
	///
	/// This is only useful on implementations that dispatch between multiple possible upgrades.
	/// Any basic implementation will probably just use the `()` type.
	type UpgradeIdentifier;

	/// Returns the name of the protocols to advertise to the remote.
	fn protocol_names(&self) -> Self::NamesIter;

	/// Type of the stream that has been upgraded. Generally wraps around `C` and `Self`.
	///
	/// > **Note**: For upgrades that add an intermediary layer (such as `secio` or `multiplex`),
	/// >           this associated type must implement `AsyncRead + AsyncWrite`.
	type Output;
	/// Type of the future that will resolve to `Self::Output`.
	type Future: Future<Item = Self::Output, Error = IoError>;

	/// This method is called after protocol negotiation has been performed.
	///
	/// Because performing the upgrade may not be instantaneous (eg. it may require a handshake),
	/// this function returns a future instead of the direct output.
	fn upgrade(self, socket: C, id: Self::UpgradeIdentifier) -> Self::Future;
}

/// See `or_upgrade()`.
#[derive(Debug, Copy, Clone)]
pub struct OrUpgrade<A, B>(A, B);

impl<C, A, B> ConnectionUpgrade<C> for OrUpgrade<A, B>
	where C: AsyncRead + AsyncWrite,
		  A: ConnectionUpgrade<C>,
		  B: ConnectionUpgrade<C>
{
	type NamesIter = NamesIterChain<A::NamesIter, B::NamesIter>;
	type UpgradeIdentifier = EitherUpgradeIdentifier<A::UpgradeIdentifier, B::UpgradeIdentifier>;

	#[inline]
	fn protocol_names(&self) -> Self::NamesIter {
		NamesIterChain {
			first: self.0.protocol_names(),
			second: self.1.protocol_names(),
		}
	}

	type Output = EitherSocket<A::Output, B::Output>;
	type Future = EitherConnUpgrFuture<A::Future, B::Future>;

	#[inline]
	fn upgrade(self, socket: C, id: Self::UpgradeIdentifier) -> Self::Future {
		match id {
			EitherUpgradeIdentifier::First(id) => {
				EitherConnUpgrFuture::First(self.0.upgrade(socket, id))
			}
			EitherUpgradeIdentifier::Second(id) => {
				EitherConnUpgrFuture::Second(self.1.upgrade(socket, id))
			}
		}
	}
}

/// Internal struct used by the `OrUpgrade` trait.
#[derive(Debug, Copy, Clone)]
pub enum EitherUpgradeIdentifier<A, B> {
	First(A),
	Second(B),
}

/// Implements `Future` and redirects calls to either `First` or `Second`.
///
/// Additionally, the output will be wrapped inside a `EitherSocket`.
///
/// > **Note**: This type is needed because of the lack of `-> impl Trait` in Rust. It can be
/// >           removed eventually.
#[derive(Debug, Copy, Clone)]
pub enum EitherConnUpgrFuture<A, B> {
	First(A),
	Second(B),
}

impl<A, B> Future for EitherConnUpgrFuture<A, B>
	where A: Future<Error = IoError>,
		  B: Future<Error = IoError>
{
	type Item = EitherSocket<A::Item, B::Item>;
	type Error = IoError;

	#[inline]
	fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
		match self {
			&mut EitherConnUpgrFuture::First(ref mut a) => {
				let item = try_ready!(a.poll());
				Ok(Async::Ready(EitherSocket::First(item)))
			}
			&mut EitherConnUpgrFuture::Second(ref mut b) => {
				let item = try_ready!(b.poll());
				Ok(Async::Ready(EitherSocket::Second(item)))
			}
		}
	}
}

/// Internal type used by the `OrUpgrade` struct.
///
/// > **Note**: This type is needed because of the lack of `-> impl Trait` in Rust. It can be
/// >           removed eventually.
#[derive(Debug, Copy, Clone)]
pub struct NamesIterChain<A, B> {
	first: A,
	second: B,
}

impl<A, B, AId, BId> Iterator for NamesIterChain<A, B>
	where A: Iterator<Item = (Bytes, AId)>,
		  B: Iterator<Item = (Bytes, BId)>
{
	type Item = (Bytes, EitherUpgradeIdentifier<AId, BId>);

	#[inline]
	fn next(&mut self) -> Option<Self::Item> {
		if let Some((name, id)) = self.first.next() {
			return Some((name, EitherUpgradeIdentifier::First(id)));
		}
		if let Some((name, id)) = self.second.next() {
			return Some((name, EitherUpgradeIdentifier::Second(id)));
		}
		None
	}

	#[inline]
	fn size_hint(&self) -> (usize, Option<usize>) {
		let (min1, max1) = self.first.size_hint();
		let (min2, max2) = self.second.size_hint();
		let max = match (max1, max2) {
			(Some(max1), Some(max2)) => max1.checked_add(max2),
			_ => None,
		};
		(min1.saturating_add(min2), max)
	}
}

/// Implementation of the `ConnectionUpgrade` that negotiates the `/plaintext/1.0.0` protocol and
/// simply passes communications through without doing anything more.
///
/// > **Note**: Generally used as an alternative to `secio` if a security layer is not desirable.
// TODO: move `PlainText` to a separate crate?
#[derive(Debug, Copy, Clone)]
pub struct PlainText;

impl<C> ConnectionUpgrade<C> for PlainText
	where C: AsyncRead + AsyncWrite
{
	type Output = C;
	type Future = FutureResult<C, IoError>;
	type UpgradeIdentifier = ();
	type NamesIter = iter::Once<(Bytes, ())>;

	#[inline]
	fn upgrade(self, i: C, _: ()) -> Self::Future {
		future_ok(i)
	}

	#[inline]
	fn protocol_names(&self) -> Self::NamesIter {
		iter::once((Bytes::from("/plaintext/1.0.0"), ()))
	}
}

/// Implements the `Transport` trait. Dials or listens, then upgrades any dialed or received
/// connection.
///
/// See the `Transport::with_upgrade` method.
#[derive(Debug, Clone)]
pub struct UpgradedNode<T, C> {
	transports: T,
	upgrade: C,
}

impl<'a, T, C> UpgradedNode<T, C>
	where T: Transport + 'a,
		  C: ConnectionUpgrade<T::RawConn> + 'a
{
	/// Builds a new struct that implements `ConnectionUpgrade` that contains both `self` and
	/// `other_upg`.
	///
	/// The returned object will try to negotiate either the protocols of `self` or the protocols
	/// of `other_upg`, then upgrade the connection to the negogiated protocol.
	#[inline]
	pub fn or_upgrade<D>(self, other_upg: D) -> UpgradedNode<T, OrUpgrade<C, D>>
		where D: ConnectionUpgrade<T::RawConn> + 'a
	{
		UpgradedNode {
			transports: self.transports,
			upgrade: OrUpgrade(self.upgrade, other_upg),
		}
	}

	/// Tries to dial on the `Multiaddr` using the transport that was passed to `new`, then upgrade
	/// the connection.
	///
	/// Note that this does the same as `Transport::dial`, but with less restrictions on the trait
	/// requirements.
	#[inline]
	pub fn dial(
		self,
		addr: Multiaddr,
	) -> Result<Box<Future<Item = C::Output, Error = IoError> + 'a>, (Self, Multiaddr)> {
		let upgrade = self.upgrade;

		let dialed_fut = match self.transports.dial(addr) {
			Ok(f) => f.into_future(),
			Err((trans, addr)) => {
				let builder = UpgradedNode {
					transports: trans,
					upgrade: upgrade,
				};

				return Err((builder, addr));
			}
		};

		let future = dialed_fut
			// Try to negotiate the protocol.
			.and_then(move |connection| {
				let iter = upgrade.protocol_names()
					.map(|(name, id)| (name, <Bytes as PartialEq>::eq, id));
				let negotiated = multistream_select::dialer_select_proto(connection, iter)
					.map_err(|err| panic!("{:?}", err));      // TODO:
				negotiated.map(|(upgrade_id, conn)| (upgrade_id, conn, upgrade))
			})
			.and_then(|(upgrade_id, connection, upgrade)| {
				upgrade.upgrade(connection, upgrade_id)
			});

		Ok(Box::new(future))
	}

	/// Start listening on the multiaddr using the transport that was passed to `new`.
	/// Then whenever a connection is opened, it is upgraded.
	///
	/// Note that this does the same as `Transport::listen_on`, but with less restrictions on the
	/// trait requirements.
	#[inline]
	pub fn listen_on(
		self,
		addr: Multiaddr,
	) -> Result<Box<Stream<Item = C::Output, Error = IoError> + 'a>, (Self, Multiaddr)>
		where C::NamesIter: Clone, // TODO: not elegant
			  C: Clone
	{
		let upgrade = self.upgrade;

		let listening_stream = match self.transports.listen_on(addr) {
			Ok(l) => l,
			Err((trans, addr)) => {
				let builder = UpgradedNode {
					transports: trans,
					upgrade: upgrade,
				};

				return Err((builder, addr));
			}
		};

		let stream = listening_stream
			// Try to negotiate the protocol.
			.and_then(move |connection| {
				let upgrade = upgrade.clone();
				#[inline]
				fn iter_map<T>((n, t): (Bytes, T)) -> (Bytes, fn(&Bytes,&Bytes)->bool, T) {
					(n, <Bytes as PartialEq>::eq, t)
				}
				let iter = upgrade.protocol_names().map(iter_map);
				let negotiated = multistream_select::listener_select_proto(connection, iter)
					.map_err(|err| panic!("{:?}", err));      // TODO:
				negotiated.map(move |(upgrade_id, conn)| (upgrade_id, conn, upgrade))
					.map_err(|_| panic!())    // TODO:
			})
			.map_err(|_| panic!())      // TODO:
			.and_then(|(upgrade_id, connection, upgrade)| {
				upgrade.upgrade(connection, upgrade_id)
			});

		Ok(Box::new(stream))
	}
}

impl<T, C> Transport for UpgradedNode<T, C>
	where T: Transport + 'static,
		  C: ConnectionUpgrade<T::RawConn> + 'static,
		  C::Output: AsyncRead + AsyncWrite,
		  C::NamesIter: Clone, // TODO: not elegant
		  C: Clone
{
	type RawConn = C::Output;
	type Listener = Box<Stream<Item = C::Output, Error = IoError>>;
	type Dial = Box<Future<Item = C::Output, Error = IoError>>;

	#[inline]
	fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, (Self, Multiaddr)>
		where Self: Sized
	{
		self.listen_on(addr)
	}

	#[inline]
	fn dial(self, addr: Multiaddr) -> Result<Self::Dial, (Self, Multiaddr)>
		where Self: Sized
	{
		self.dial(addr)
	}
}
