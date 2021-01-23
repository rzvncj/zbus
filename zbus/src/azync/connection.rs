use async_io::Async;
use async_lock::{Mutex, MutexGuard, RwLock};
use once_cell::sync::OnceCell;
use std::{
    io::{self, ErrorKind},
    os::unix::{
        io::{AsRawFd, RawFd},
        net::UnixStream,
    },
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_core::stream;
use futures_util::{sink::SinkExt, stream::TryStreamExt};

use crate::{
    azync::Authenticated,
    raw::{Connection as RawConnection, Socket},
    Error, Guid, Message, MessageType, Result, DEFAULT_MAX_QUEUED,
};

#[derive(Debug)]
struct ConnectionInner<S> {
    server_guid: Guid,
    cap_unix_fd: bool,
    bus_conn: bool,
    unique_name: OnceCell<String>,

    raw_in_conn: Mutex<RawConnection<Async<S>>>,
    raw_out_conn: Mutex<RawConnection<Async<S>>>,
    // Serial number for next outgoing message
    serial: Mutex<u32>,

    // Queue of incoming messages
    incoming_queue: Mutex<Vec<Message>>,

    // Max number of messages to queue
    max_queued: RwLock<usize>,
}

/// The asynchronous sibling of [`zbus::Connection`].
///
/// Most of the API is very similar to [`zbus::Connection`], except it's asynchronous. However,
/// there are a few differences:
///
/// ### Sending Messages
///
/// For sending messages you can either use [`Connection::send_message`] method or make use of the
/// [`futures_sink::Sink`] implementation that is returned by [`Connection::sink`] method. For
/// latter, you might find [`SinkExt`] API very useful. Keep in mind that [`Connection`] will not
/// manage the serial numbers (cookies) on the messages for you when they are sent through the
/// [`Sink`]. You can manually assign unique serial numbers to them using the
/// [`Connection::assign_serial_num`] method before sending them off, if needed. Having said that,
/// [`Sink`] is mainly useful for sending out signals, as they do not expect a reply, and serial
/// numbers are not very useful for signals either for the same reason.
///
/// ### Receiving Messages
///
/// Unlike [`zbus::Connection`], there is no direct async equivalent of
/// [`zbus::Connection::receive_message`] method provided. This is because the `futures` crate
/// already provides a nice rich API that makes use of the [`stream::Stream`] implementation that is
/// returned by [`Connection::stream`] method.
///
/// However, there is [`Connection::receive_specific`] method, which takes a predicate function,
/// using which you get to decided which message you're interested in. It first checks if there
/// was already a message received by a previous call to [`Connection::receive_specific`]
/// or during a [`Connection::call_method`] call that fits the predicate and returns that immediate.
/// Otherwise, it awaits on the connection for the message of interest to be received. All other
/// messages received, while waiting, are appended to the end of the incoming message queue to be
/// picked up by a following or already awaiting `receive_specific` call or [`stream::Stream`]
/// API.
///
/// In summary, if you're going to call D-Bus methods on the connection in one task, while receiving
/// messages in another, it's best to use `receive_specific` method. Otherwise, you'd want to make
/// use of the `stream` method.
///
/// ### Examples
///
/// #### Get the session bus ID
///
/// ```
///# use zvariant::Type;
///#
///# pollster::block_on(async {
/// use zbus::azync::Connection;
///
/// let mut connection = Connection::new_session().await?;
///
/// let reply = connection
///     .call_method(
///         Some("org.freedesktop.DBus"),
///         "/org/freedesktop/DBus",
///         Some("org.freedesktop.DBus"),
///         "GetId",
///         &(),
///     )
///     .await?;
///
/// let id: &str = reply.body()?;
/// println!("Unique ID of the bus: {}", id);
///# Ok::<(), zbus::Error>(())
///# });
/// ```
///
/// #### Monitoring all messages
///
/// Let's eavesdrop on the session bus 😈 using the [Monitor] interface:
///
/// ```rust,no_run
///# pollster::block_on(async {
/// use futures_util::stream::TryStreamExt;
/// use zbus::azync::Connection;
///
/// let mut connection = Connection::new_session().await?;
///
/// connection
///     .call_method(
///         Some("org.freedesktop.DBus"),
///         "/org/freedesktop/DBus",
///         Some("org.freedesktop.DBus.Monitoring"),
///         "BecomeMonitor",
///         &(&[] as &[&str], 0u32),
///     )
///     .await?;
///
/// while let Some(msg) = connection.stream().await.try_next().await? {
///     println!("Got message: {}", msg);
/// }
///
///# Ok::<(), zbus::Error>(())
///# });
/// ```
///
/// This should print something like:
///
/// ```console
/// Got message: Signal NameAcquired from org.freedesktop.DBus
/// Got message: Signal NameLost from org.freedesktop.DBus
/// Got message: Method call GetConnectionUnixProcessID from :1.1324
/// Got message: Error org.freedesktop.DBus.Error.NameHasNoOwner:
///              Could not get PID of name ':1.1332': no such name from org.freedesktop.DBus
/// Got message: Method call AddMatch from :1.918
/// Got message: Method return from org.freedesktop.DBus
/// ```
///
/// [Monitor]: https://dbus.freedesktop.org/doc/dbus-specification.html#bus-messages-become-monitor
#[derive(Clone, Debug)]
pub struct Connection(Arc<ConnectionInner<Box<dyn Socket>>>);

impl Connection {
    /// Create and open a D-Bus connection from a `UnixStream`.
    ///
    /// The connection may either be set up for a *bus* connection, or not (for peer-to-peer
    /// communications).
    ///
    /// Upon successful return, the connection is fully established and negotiated: D-Bus messages
    /// can be sent and received.
    pub async fn new_unix_client(stream: UnixStream, bus_connection: bool) -> Result<Self> {
        // SASL Handshake
        let auth = Authenticated::client(Async::new(Box::new(stream) as Box<dyn Socket>)?).await?;

        Self::new(auth, bus_connection).await
    }

    /// Create a server `Connection` for the given `UnixStream` and the server `guid`.
    ///
    /// The connection will wait for incoming client authentication handshake & negotiation messages,
    /// for peer-to-peer communications.
    ///
    /// Upon successful return, the connection is fully established and negotiated: D-Bus messages
    /// can be sent and received.
    pub async fn new_unix_server(stream: UnixStream, guid: &Guid) -> Result<Self> {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

        // FIXME: Could and should this be async?
        let creds = getsockopt(stream.as_raw_fd(), PeerCredentials)
            .map_err(|e| Error::Handshake(format!("Failed to get peer credentials: {}", e)))?;

        let auth = Authenticated::server(
            Async::new(Box::new(stream) as Box<dyn Socket>)?,
            guid.clone(),
            creds.uid(),
        )
        .await?;

        Self::new(auth, false).await
    }

    /// Get a stream to receive incoming messages.
    pub async fn stream(&self) -> Stream<'_> {
        let raw_conn = self.0.raw_in_conn.lock().await;
        let incoming_queue = Some(self.0.incoming_queue.lock().await);

        Stream {
            raw_conn,
            incoming_queue,
        }
    }

    /// Get a sink to send out messages.
    pub async fn sink(&self) -> Sink<'_> {
        Sink {
            raw_conn: self.0.raw_out_conn.lock().await,
            cap_unix_fd: self.0.cap_unix_fd,
        }
    }

    /// Receive a specific message.
    ///
    /// This is the same as receiving messages from [`Stream`], except that this takes a predicate
    /// function that decides if the message received should be returned by this method or not. All
    /// messages received during this call that are not returned by it, are pushed to the queue to
    /// be picked by the susubsequent or awaiting call to this method or by the `Stream`.
    pub async fn receive_specific<P>(&self, predicate: P) -> Result<Message>
    where
        P: Fn(&Message) -> Result<bool>,
    {
        loop {
            let mut queue = self.0.incoming_queue.lock().await;
            for (i, msg) in queue.iter().enumerate() {
                if predicate(msg)? {
                    return Ok(queue.remove(i));
                }
            }

            let mut stream = Stream {
                raw_conn: self.0.raw_in_conn.lock().await,
                incoming_queue: None,
            };
            let msg = match stream.try_next().await? {
                Some(msg) => msg,
                None => {
                    // If Stream gives us None, that means the socket was closed
                    return Err(Error::Io(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "socket closed",
                    )));
                }
            };

            if predicate(&msg)? {
                return Ok(msg);
            } else if queue.len() < *self.0.max_queued.read().await {
                queue.push(msg);
            }
        }
    }

    /// Send `msg` to the peer.
    ///
    /// Unlike [`Sink`], this method sets a unique (to this connection) serial number on the message
    /// before sending it off, for you.
    ///
    /// On successfully sending off `msg`, the assigned serial number is returned.
    pub async fn send_message(&self, mut msg: Message) -> Result<u32> {
        let serial = self.assign_serial_num(&mut msg).await?;

        self.sink().await.send(msg).await?;

        Ok(serial)
    }

    /// Send a method call.
    ///
    /// Create a method-call message, send it over the connection, then wait for the reply.
    ///
    /// On succesful reply, an `Ok(Message)` is returned. On error, an `Err` is returned. D-Bus
    /// error replies are returned as [`Error::MethodError`].
    pub async fn call_method<B>(
        &self,
        destination: Option<&str>,
        path: &str,
        iface: Option<&str>,
        method_name: &str,
        body: &B,
    ) -> Result<Message>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method(
            self.unique_name(),
            destination,
            path,
            iface,
            method_name,
            body,
        )?;
        let serial = self.send_message(m).await?;

        loop {
            match self
                .receive_specific(|m| {
                    let h = m.header()?;

                    Ok(h.reply_serial()? == Some(serial))
                })
                .await
            {
                Ok(m) => match m.header()?.message_type()? {
                    MessageType::Error => return Err(m.into()),
                    MessageType::MethodReturn => return Ok(m),
                    _ => continue,
                },
                Err(e) => return Err(e),
            };
        }
    }

    /// Emit a signal.
    ///
    /// Create a signal message, and send it over the connection.
    pub async fn emit_signal<B>(
        &self,
        destination: Option<&str>,
        path: &str,
        iface: &str,
        signal_name: &str,
        body: &B,
    ) -> Result<()>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::signal(
            self.unique_name(),
            destination,
            path,
            iface,
            signal_name,
            body,
        )?;

        self.send_message(m).await.map(|_| ())
    }

    /// Reply to a message.
    ///
    /// Given an existing message (likely a method call), send a reply back to the caller with the
    /// given `body`.
    ///
    /// Returns the message serial number.
    pub async fn reply<B>(&self, call: &Message, body: &B) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method_reply(self.unique_name(), call, body)?;
        self.send_message(m).await
    }

    /// Reply an error to a message.
    ///
    /// Given an existing message (likely a method call), send an error reply back to the caller
    /// with the given `error_name` and `body`.
    ///
    /// Returns the message serial number.
    pub async fn reply_error<B>(&self, call: &Message, error_name: &str, body: &B) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::Type,
    {
        let m = Message::method_error(self.unique_name(), call, error_name, body)?;
        self.send_message(m).await
    }

    /// Checks if `self` is a connection to a message bus.
    ///
    /// This will return `false` for p2p connections.
    pub fn is_bus(&self) -> bool {
        self.0.bus_conn
    }

    /// Assigns a serial number to `msg` that is unique to this connection.
    ///
    /// This method can fail if `msg` is corrupt.
    pub async fn assign_serial_num(&self, msg: &mut Message) -> Result<u32> {
        let serial = self.next_serial().await;
        msg.modify_primary_header(|primary| {
            primary.set_serial_num(serial);

            Ok(())
        })?;

        Ok(serial)
    }

    /// The unique name as assigned by the message bus or `None` if not a message bus connection.
    pub fn unique_name(&self) -> Option<&str> {
        self.0.unique_name.get().map(|s| s.as_str())
    }

    /// Max number of messages to queue.
    pub async fn max_queued(&self) -> usize {
        *self.0.max_queued.read().await
    }

    /// Set the max number of messages to queue.
    ///
    /// Since typically you'd want to set this at instantiation time, this method takes ownership
    /// of `self` and returns an owned `Connection` instance so you can use the builder pattern to
    /// set the value.
    ///
    /// # Example
    ///
    /// ```
    ///# use std::error::Error;
    ///# use zbus::azync::Connection;
    ///# use pollster::block_on;
    ///#
    ///# block_on(async {
    /// let conn = Connection::new_session()
    ///     .await?
    ///     .set_max_queued(30)
    ///     .await;
    /// assert_eq!(conn.max_queued().await, 30);
    ///
    ///#     Ok::<(), zbus::Error>(())
    ///# });
    ///#
    /// // Do something usefull with `conn`..
    ///# Ok::<_, Box<dyn Error + Send + Sync>>(())
    /// ```
    pub async fn set_max_queued(self, max: usize) -> Self {
        *self.0.max_queued.write().await = max;

        self
    }

    /// The server's GUID.
    pub fn server_guid(&self) -> &str {
        self.0.server_guid.as_str()
    }

    /// Get the raw file descriptor of this connection.
    pub async fn as_raw_fd(&self) -> RawFd {
        (self.0.raw_in_conn.lock().await.socket()).as_raw_fd()
    }

    async fn hello_bus(self) -> Result<Self> {
        // TODO: Use fdo module once it's async.
        let name: String = self
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "Hello",
                &(),
            )
            .await?
            .body()?;

        self.0
            .unique_name
            .set(name)
            // programmer (probably our) error if this fails.
            .expect("Attempted to set unique_name twice");

        Ok(self)
    }

    async fn new(
        auth: Authenticated<Async<Box<dyn Socket>>>,
        bus_connection: bool,
    ) -> Result<Self> {
        let auth = auth.into_inner();
        let out_socket = auth.conn.socket().get_ref().try_clone()?;
        let out_conn = RawConnection::wrap(Async::new(out_socket)?);

        let connection = Self(Arc::new(ConnectionInner {
            raw_in_conn: Mutex::new(auth.conn),
            raw_out_conn: Mutex::new(out_conn),
            server_guid: auth.server_guid,
            cap_unix_fd: auth.cap_unix_fd,
            bus_conn: bus_connection,
            serial: Mutex::new(1),
            unique_name: OnceCell::new(),
            incoming_queue: Mutex::new(vec![]),
            max_queued: RwLock::new(DEFAULT_MAX_QUEUED),
        }));

        if !bus_connection {
            return Ok(connection);
        }

        // Now that the server has approved us, we must send the bus Hello, as per specs
        connection.hello_bus().await
    }

    async fn next_serial(&self) -> u32 {
        let mut serial = self.0.serial.lock().await;
        let current = *serial;
        *serial = current + 1;

        current
    }

    /// Create a `Connection` to the session/user message bus.
    pub async fn new_session() -> Result<Self> {
        Self::new(Authenticated::session().await?, true).await
    }

    /// Create a `Connection` to the system-wide message bus.
    pub async fn new_system() -> Result<Self> {
        Self::new(Authenticated::system().await?, true).await
    }

    /// Create a `Connection` for the given [D-Bus address].
    ///
    /// [D-Bus address]: https://dbus.freedesktop.org/doc/dbus-specification.html#addresses
    pub async fn new_for_address(address: &str, bus_connection: bool) -> Result<Self> {
        Self::new(Authenticated::for_address(address).await?, bus_connection).await
    }
}

/// Our [`futures_sink::Sink`] implementation.
///
/// Use [`Connection::sink`] to create an instance of this type.
pub struct Sink<'s> {
    raw_conn: MutexGuard<'s, RawConnection<Async<Box<dyn Socket>>>>,
    cap_unix_fd: bool,
}

impl Sink<'_> {
    fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            match self.raw_conn.try_flush() {
                Ok(()) => return Poll::Ready(Ok(())),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        let poll = self.raw_conn.socket().poll_writable(cx);

                        match poll {
                            Poll::Pending => return Poll::Pending,
                            // Guess socket became ready already so let's try it again.
                            Poll::Ready(Ok(_)) => continue,
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        }
                    } else {
                        return Poll::Ready(Err(Error::Io(e)));
                    }
                }
            }
        }
    }
}

impl futures_sink::Sink<Message> for Sink<'_> {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // TODO: We should have a max queue length in raw::Socket for outgoing messages.
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, msg: Message) -> Result<()> {
        if !msg.fds().is_empty() && !self.cap_unix_fd {
            return Err(Error::Unsupported);
        }

        self.get_mut().raw_conn.enqueue_message(msg);

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.get_mut().flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let sink = self.get_mut();
        match sink.flush(cx) {
            Poll::Ready(Ok(_)) => (),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        Poll::Ready((sink.raw_conn).close())
    }
}

/// Our [`stream::Stream`] implementation.
///
/// Use [`Connection::stream`] to create an instance of this type.
///
/// # Warning
///
/// If you use this in combination with [`Connection::receive_specific`] on the same connection
/// from multiple tasks, you can end up with situation where the stream takes away the message
/// the `receive_specific` is waiting for and end up in a deadlock situation. It is therefore highly
/// recommended not to use such a combination.
pub struct Stream<'s> {
    raw_conn: MutexGuard<'s, RawConnection<Async<Box<dyn Socket>>>>,
    incoming_queue: Option<MutexGuard<'s, Vec<Message>>>,
}

impl<'s> stream::Stream for Stream<'s> {
    type Item = Result<Message>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let stream = self.get_mut();

        if let Some(queue) = &mut stream.incoming_queue {
            if let Some(msg) = queue.pop() {
                return Poll::Ready(Some(Ok(msg)));
            }
        }

        loop {
            match stream.raw_conn.try_receive_message() {
                Ok(m) => return Poll::Ready(Some(Ok(m))),
                Err(Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    let poll = stream.raw_conn.socket().poll_readable(cx);

                    match poll {
                        Poll::Pending => return Poll::Pending,
                        // Guess socket became ready already so let's try it again.
                        Poll::Ready(Ok(_)) => continue,
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                    }
                }
                Err(Error::Io(e)) if e.kind() == ErrorKind::BrokenPipe => return Poll::Ready(None),
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn unix_p2p() {
        pollster::block_on(test_unix_p2p()).unwrap();
    }

    async fn test_unix_p2p() -> Result<()> {
        let guid = Guid::generate();

        let (p0, p1) = UnixStream::pair().unwrap();

        let server = Connection::new_unix_server(p0, &guid);
        let client = Connection::new_unix_client(p1, false);

        let (client_conn, server_conn) = futures_util::try_join!(client, server)?;

        let server_future = async {
            let mut method: Option<Message> = None;
            while let Some(m) = server_conn.stream().await.try_next().await? {
                if m.to_string() == "Method call Test" {
                    method.replace(m);

                    break;
                }
            }
            let method = method.unwrap();

            // Send another message first to check the queueing function on client side.
            server_conn
                .emit_signal(None, "/", "org.zbus.p2p", "ASignalForYou", &())
                .await?;
            server_conn.reply(&method, &("yay")).await
        };

        let client_future = async {
            let reply = client_conn
                .call_method(None, "/", Some("org.zbus.p2p"), "Test", &())
                .await?;
            assert_eq!(reply.to_string(), "Method return");
            // Check we didn't miss the signal that was sent during the call.
            let m = client_conn.stream().await.try_next().await?.unwrap();
            assert_eq!(m.to_string(), "Signal ASignalForYou");
            reply.body::<String>().map_err(|e| e.into())
        };

        let (val, _) = futures_util::try_join!(client_future, server_future)?;
        assert_eq!(val, "yay");

        Ok(())
    }

    #[test]
    fn serial_monotonically_increases() {
        pollster::block_on(test_serial_monotonically_increases());
    }

    async fn test_serial_monotonically_increases() {
        let c = Connection::new_session().await.unwrap();
        let serial = c.next_serial().await + 1;

        for next in serial..serial + 10 {
            assert_eq!(next, c.next_serial().await);
        }
    }
}