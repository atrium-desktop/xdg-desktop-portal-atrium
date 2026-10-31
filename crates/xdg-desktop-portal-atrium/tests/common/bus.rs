//! A minimal blocking facade over zbus 6 for the integration tests.
//!
//! zbus 6 removed `zbus::blocking`, and the integration tests are ordinary
//! synchronous `#[test]` functions. The tests act as **clients**: they connect
//! to a private bus, call methods, and wait for names. For a client, driving
//! each call through `zbus::block_on` is correct — the future both issues the
//! call and polls the connection for the reply, so one `block_on` per call is
//! sufficient.
//!
//! (The daemon itself cannot do that: its methods run *inside* zbus's own
//! dispatch, so it needs a separate driver thread. See
//! `atrium_portal_request::driver` for that side.)
//!
//! This module deliberately knows nothing about portal behaviour, so a test
//! can only use it to talk to a real bus the way a real client would.

use zbus::{Connection, DynamicType};

/// Connect to the bus at `address`.
///
/// # Panics
///
/// Panics if the address is malformed or the connection fails: a test fixture
/// that cannot reach its own private bus has no meaningful way to continue.
pub fn connect(address: &str) -> Connection {
    let builder = zbus::connection::Builder::address(address);
    zbus::block_on(async { builder.build().await }).expect("connect to the private bus")
}

/// Run one `future` to completion on this thread.
///
/// The caller must not already be inside a `block_on` or inside a zbus task;
/// no integration test is.
pub fn run<F: std::future::Future>(future: F) -> F::Output {
    zbus::block_on(future)
}

/// A proxy for `interface` at `path` on `service`.
///
/// # Panics
///
/// Panics if the connection cannot build the proxy, which for a private bus
/// with a well-formed name can only mean the connection has died.
pub fn proxy<'a>(
    connection: &Connection,
    service: &'a str,
    path: &'a str,
    interface: &'a str,
) -> zbus::Proxy<'a> {
    zbus::block_on(zbus::Proxy::new(connection, service, path, interface)).expect("build the proxy")
}

/// Call `method` with `body` and wait for the reply.
///
/// # Panics
///
/// Panics if the call fails: every integration test asserts on the reply, so a
/// transport failure is a test error, not a condition to branch on.
pub fn call<'a, B, R>(proxy: &zbus::Proxy<'a>, method: &str, body: &B) -> R
where
    B: serde::ser::Serialize + DynamicType,
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
{
    try_call(proxy, method, body).unwrap_or_else(|error| panic!("{method} failed: {error}"))
}

/// Call `method` with `body`, returning the error instead of panicking.
pub fn try_call<'a, B, R>(proxy: &zbus::Proxy<'a>, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::ser::Serialize + DynamicType,
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
{
    zbus::block_on(proxy.call(method, body))
}

/// Wait for the next signal on `stream`.
///
/// # Panics
///
/// Panics if the stream ends or errors: a test that is waiting for a signal
/// from its own private bus has nothing left to observe.
pub fn next_signal(stream: &mut zbus::proxy::SignalStream<'_>) -> zbus::Message {
    use futures_util::StreamExt as _;
    run(stream.next()).expect("the signal stream ended")
}
