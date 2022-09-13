# ipc-rpc

Inter-Process Communication Remote Procedure Calls

[![Crates.io](https://img.shields.io/crates/v/ipc-rpc.svg)](https://crates.io/crates/ipc-rpc/)

[Documentation](https://docs.rs/ipc-rpc/)

This Rust library is a wrapper over [`servo/ipc-channel`](https://github.com/servo/ipc-channel) that adds many new features.

- Bi-directional communication by default.
- [Future](https://doc.rust-lang.org/stable/std/future/trait.Future.html) based reply mechanism allows easy and accurate pairing of requests and responses.
- (Optional, enabled by default) Validation on startup that message schemas match between the server and client. No more debugging bad deserializations. Relies on [`schemars`](https://crates.io/crates/schemars).
- Streamlined initialization of IPC channels for common use cases, while still allowing for more flexible and robust dynamic initialization in more unique scenarios.

Compatible with anything that can run `servo/ipc-channel`, which at time of writing includes

- Windows
- MacOS
- Linux
- FreeBSD
- OpenBSD

Additionally, `servo/ipc-channel` supports the following platforms but only while in `inprocess` mode, which is not capable of communication between processes.

- Android
- iOS
- WASI

## tokio

This crate uses the [`tokio`](https://crates.io/crates/tokio) runtime for executing futures, and it is a hard requirement that users of `ipc-rpc` must use `tokio`. There are no plans to add support for other
executors, but sufficient demand for other executors may change that.

# Cargo features

This crate exposes one feature, `message-schema-validation`, which is on by default. This enables functionality related to the [`schemars`](https://crates.io/crates/schemars) crate.
When enabled, the software will attempt to validate the user message schema on initialization of the connection. Failure to validate is not a critical failure, and won't crash the program. 
An error will be emitted in the logs, and this status can be retrieved programmatically via many functions, all called `schema_validated()`.

If you decide that a failure to validate the schema should be a critical failure you can add the following line of code to your program for execution after a connection is established.

### Server
```rust
server.schema_validated().await.unwrap().assert_success();
```

### Client
```rust
client.schema_validated().await.unwrap().assert_success();
```

# Running the examples

The examples include a client and a server which are meant to be run together. The sequence for using them is

```
cargo build --examples
cargo run --example server
```

The server launches the client as part of its operation.

# Limitations

Much like `servo/ipc-channel`, servers may only serve one client. Overcoming this limitation would require work within `servo/ipc-channel`.
