# WebShell

This is a quick-and-dirty port of Julia Evans' fun [remote login server](https://jvns.ca/blog/2022/07/28/toy-remote-login-server/) to Rust.

I thought it'd be interesting to see how the terminal setup translates - with [Nix](https://lib.rs/crates/nix), it's pretty nice!
I also wanted to try out [axum](https://lib.rs/crates/axum), so this runs a WebSocket connected to [Xterm.js](https://xtermjs.org/) instead of a TCP socket.

## Running

You'll need [just](https://github.com/casey/just), `npm`, and a recent Rust/Cargo.

```sh
# First, install dependencies
$ just install
# Start the server
$ just run
```

Then, open http://localhost:3000.

It also starts a [tokio-console](https://github.com/tokio-rs/console) server using the default settings, which you can connect to using `just tokio-console`. The console
also works from the WebShell itself.

Finally, to see a count of active terminals, go to http://localhost:3000/info.