use std::{
    ffi::{OsStr, OsString},
    fmt::Write,
    net::SocketAddr,
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Command as StdCommand, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, get_service},
    Router, Server,
};
use bytes::BytesMut;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use miette::{Context, IntoDiagnostic};
use nix::unistd::setsid;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
};
use tower_http::services::ServeDir;
use tracing_subscriber::prelude::*;

#[derive(Parser, Debug)]
struct Args {
    /// Port to listen on. Because WebShell has absolutely zero built-in security, it refuses to bind to any address but `localhost`.
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// Shell to spawn. Defaults to the shell that WebShell was run from
    #[arg(short, long, env = "SHELL")]
    shell: OsString,
}

/// Shared state across connections
#[derive(Debug)]
struct AppState {
    shell: OsString,
    active_terminals: AtomicUsize,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::new([127, 0, 0, 1].into(), args.port);

    let console_layer = console_subscriber::spawn();
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("WEBSHELL_LOG")
        .unwrap_or_else(|_| "webshell=debug,tower_http=debug".into());

    tracing_subscriber::registry()
        .with(console_layer)
        // Important: only apply filtering to the fmt layer, not the console layer. Otherwise, the console won't get the data it needs
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter))
        .init();

    let frontend = get_service(ServeDir::new("ui")).handle_error(handle_error);
    // Serving node_modules directly out of the /n/ prefix means we don't have to deal with module bundling
    let node = get_service(ServeDir::new("node_modules")).handle_error(handle_error);

    let state = Arc::new(AppState {
        shell: args.shell,
        active_terminals: AtomicUsize::new(0),
    });

    let app = Router::new()
        .route("/info", get(handle_info))
        .route("/ws", get(terminal_socket))
        .nest_service("/n", node)
        .fallback_service(frontend)
        .with_state(state);

    let server = Server::bind(&addr).serve(app.into_make_service());
    tracing::info!(?addr, "Listening for connections...");
    server.await.into_diagnostic().wrap_err("server failed")
}

#[tracing::instrument(skip(ws, state))]
async fn terminal_socket(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(|w| async move {
        // Handle active_terminals here, since it simplifies ensuring that the counter is decremented

        state.active_terminals.fetch_add(1, Ordering::Relaxed);
        if let Err(err) = handle_socket(w, &state).await {
            tracing::error!("WebSocket failure: {err}")
        }
        state.active_terminals.fetch_sub(1, Ordering::Relaxed);
    })
}

async fn handle_error(err: std::io::Error) -> impl IntoResponse {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Something went wrong: {err}"),
    )
}

/// Handles a WebSocket terminal connection. This will spawn a new terminal process and stream stdin/stdout to it.
#[tracing::instrument(skip(socket, state))]
async fn handle_socket(socket: WebSocket, state: &AppState) -> miette::Result<()> {
    tracing::info!("Starting new WebShell");

    // The core of this is pretty simple - we split the WebSocket and PTY into read and write halves, then
    // start two Tokio tasks to copy data back and forth.
    // Then, we wait for either a task to end (indicating the client disconnected) or the shell to exit (indicating that it crashed, or the user ran `exit`)
    // Aborting the reader/writer tasks ensures that the WebSocket is disconnected.

    let mut terminal = create_terminal(&state.shell)?;
    // This clones the file descriptor by calling dup(2) or similar. I'm pretty sure we could just create two Files from the same raw file descriptor, although Tokio
    // expects to have unique ownership and relies on that for setting file descriptor state - duplicating seems better than risking strange bugs.
    // We can't use ::tokio::io::split because that locks the underlying stream, preventing us from reading and writing concurrently.
    let mut read_pty = terminal
        .pty
        .try_clone()
        .await
        .into_diagnostic()
        .wrap_err("could not clone PTY")?;
    let mut write_pty = terminal.pty;

    let (mut sender, mut receiver) = socket.split();

    // TODO: these fail spuriously after the terminal exits - they'll attempt to read/write before we've detected the exit and closed the WebSocket.
    //   They fail with errno=5 (I/O error), which is generic enough that I don't really want to blanket-ignore it.

    let mut writer: JoinHandle<()> = tokio::task::Builder::new().name(&format!("{}-writer", terminal.id)).spawn(async move {
        while let Some(msg) = receiver.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(err) => {
                    tracing::debug!(pty = ?write_pty, "WebSocket disconnected (read-side): {err}");
                    break;
                }
            };

            let res = match msg {
                // Depending on the client, it may send data as either binary or text. Either way, we send it straight to the shell
                Message::Binary(data) => write_pty.write_all(&data).await.into_diagnostic(),
                Message::Text(data) => write_pty.write_all(data.as_bytes()).await.into_diagnostic(),
                Message::Close(maybe_frame) => {
                    if let Some(frame) = maybe_frame {
                        tracing::debug!("Closing WebSocket: {:?}", frame);
                    }
                    break;
                }
                msg => {
                    tracing::info!("Ignoring unexpected WebSocket message {:?}", msg);
                    Ok(())
                }
            };

            if let Err(err) = res {
                tracing::debug!(pty = ?write_pty, "PTY write failed: {err}");
                break;
            }
        }
    }).into_diagnostic()?;

    let mut reader: JoinHandle<()> = tokio::task::Builder::new()
        .name(&format!("{}-reader", terminal.id))
        .spawn(async move {
            // Unfortunately, we have to copy the buffer into a Vec to produce a WebSocket message, so this doesn't reuse capacity much.
            // Because the terminal has to read/write only a few characters at a time, the buffers shouldn't be very large.
            let mut buf = BytesMut::with_capacity(64);
            loop {
                if let Err(err) = read_pty.read_buf(&mut buf).await {
                    tracing::debug!(pty = ?read_pty, "PTY read failed: {err}");
                    break;
                }
                if buf.is_empty() {
                    // EOF was reached
                    tracing::debug!(pty = ?read_pty, "PTY closed");
                    break;
                }

                if let Err(err) = sender.send(Message::Binary(buf.to_vec())).await {
                    tracing::debug!(pty = ?read_pty, "WebSocket disconnected (write-side): {err}");
                    break;
                }
                buf.clear();
            }
        })
        .into_diagnostic()?;

    tokio::select! {
        status = terminal.process.wait() => {
            tracing::info!(id = terminal.id, "Terminal exited with status {:?}", status);
            reader.abort();
            writer.abort();
        }
        _ = &mut reader => {
            writer.abort();
            tracing::debug!(id = terminal.id, "Reader task disconnected");
            if let Err(err) = terminal.process.kill().await {
                tracing::warn!(id = terminal.id, "Could not kill shell: {err}");
            } else {
                tracing::debug!(id = terminal.id, "Killed shell");
            }
        },
        _ = &mut writer => {
            reader.abort();
            tracing::debug!(id = terminal.id, "Writer task disconnected");
            if let Err(err) = terminal.process.kill().await {
                tracing::warn!(id = terminal.id, "Could not kill shell: {err}");
            } else {
                tracing::debug!(id = terminal.id, "Killed shell");
            }
        }
    }

    Ok(())
}

/// Handler to return the current server status
#[tracing::instrument]
async fn handle_info(State(state): State<Arc<AppState>>) -> Html<String> {
    let formatted_shell = state.shell.to_string_lossy();
    Html(format!(
        "<p>There are <strong>{}</strong> terminals running {}</p>\n",
        state.active_terminals.load(Ordering::Relaxed),
        formatted_shell
    ))
}

/// A spawned terminal
struct Terminal {
    id: String,
    pty: File,
    process: Child,
}

/// Creates a pseudoterminal running the given shell
#[tracing::instrument]
fn create_terminal(shell: &OsStr) -> miette::Result<Terminal> {
    use nix::pty::{openpty, Winsize};
    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 1,
            ws_ypixel: 1,
        }),
        None,
    )
    .into_diagnostic()
    .wrap_err("Creating PTY failed")?;

    let mut cmd = StdCommand::new(shell);
    unsafe {
        // Safety: we own the slave PTY file descriptor, since we just created it
        // Reusing it for stdin/out/err is also allowed, we don't need to clone it
        cmd.stdin(Stdio::from_raw_fd(pty.slave));
        cmd.stdout(Stdio::from_raw_fd(pty.slave));
        cmd.stderr(Stdio::from_raw_fd(pty.slave));

        // pre_exec safety: all we do is put the process in a new session, which shouldn't perform operations that don't work after a fork()
        // Per https://jvns.ca/blog/2022/07/28/toy-remote-login-server/, we have to put the shell in its own session, so that the Linux
        // terminal system can reinterpret Ctrl-C as a SIGINT to the session's foreground process group.
        cmd.pre_exec(|| {
            let _ = setsid()?;
            Ok(())
        });
    }

    let mut cmd: Command = cmd.into();
    let process = cmd
        .spawn()
        .into_diagnostic()
        .wrap_err_with(|| format!("Could not spawn {shell:?}"))?;

    let mut id: String = shell.to_string_lossy().into();
    id.push('@');
    match process.id() {
        Some(pid) => {
            write!(id, "{pid}").expect("write to a string cannot fail");
        }
        None => id.push_str("<unknown>"),
    }

    tracing::info!(
        pty_self = pty.master,
        pty_shell = pty.slave,
        shell = id,
        "Spawned terminal"
    );
    Ok(Terminal {
        id,
        // Safety: we created the PTY above, so we own it and know that it's valid
        pty: unsafe { File::from_raw_fd(pty.master) },
        process,
    })
}
