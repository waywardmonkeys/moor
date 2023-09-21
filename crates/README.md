Directory layout for `crates/`

  * `values` - crate that implements the core MOO discriminated union (`Var`) value type,
    plus all associated types and traits and interfaces used by other crates.
  * `kernel` - the actual implementation of the system: database, compiler, virtual machine,
    task scheduler, implementations of all builtin functions, etc.
  * `server` - a "monolithic" server which links kernel and provides telnet and websocket and repl
    hosts.
  * `daemon` - exposes the functionality of the system over a ZeroMQ based RPC interface, without
    exposing any network protocol to the outside world. Instead, that functionality is provided by...
  * `host` - a binary which connects to `daemon` and provides telnet (for now) and (in the future)
    websocket and HTTP external interfaces. The idea being that the `daemon` can go up and down, or
    be located on a different physical machine from the network `host`
  * `rpc-common` - crate providing types used by both `daemon` and `host`, for the RPC interface
  * `regexpr-binding` - crate providing bindings to the old regular expressions library used by
    the LambdaMOO server, for compatibility with existing cores. This is a temporary measure until
    this can be reworked with use of the `regex` crate and some compatibility translation