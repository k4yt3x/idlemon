use std::sync::Arc;

use anyhow::Context as _;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use x11rb::connection::Connection as _;
use zbus::fdo::DBusProxy;

mod idle_monitor;
use idle_monitor::{IdleMonitor, OBJECT_PATH, WELL_KNOWN_NAME};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (x11_connection, screen_num) = x11rb::connect(None).context("connect to X server")?;
    let root_window = x11_connection
        .setup()
        .roots
        .get(screen_num)
        .context("X11 screen index out of range")?
        .root;
    let x11_connection = Arc::new(x11_connection);

    let (monitor, state) = IdleMonitor::new(x11_connection.clone(), root_window);

    let connection = zbus::connection::Builder::session()
        .context("open session bus")?
        .serve_at(OBJECT_PATH, monitor)
        .context("register IdleMonitor at object path")?
        .build()
        .await
        .context("build D-Bus connection")?;

    match connection.request_name(WELL_KNOWN_NAME).await {
        Ok(()) => {
            tracing::info!(name = WELL_KNOWN_NAME, "acquired well-known bus name");
        }
        Err(zbus::Error::NameTaken) => {
            anyhow::bail!(
                "another process already owns {WELL_KNOWN_NAME} on the session bus; refusing to compete"
            );
        }
        Err(error) => return Err(error).context("request well-known bus name"),
    }

    let interface_ref = connection
        .object_server()
        .interface::<_, IdleMonitor>(OBJECT_PATH)
        .await
        .context("look up registered IdleMonitor interface")?;

    let dbus_proxy = DBusProxy::new(&connection)
        .await
        .context("create org.freedesktop.DBus proxy")?;
    let name_owner_changed_stream = dbus_proxy
        .receive_name_owner_changed()
        .await
        .context("subscribe to NameOwnerChanged")?;
    let mut cleanup_handle = tokio::spawn(idle_monitor::run_cleanup_loop(
        state.clone(),
        name_owner_changed_stream,
    ));

    let mut poll_handle = tokio::spawn(idle_monitor::run_poll_loop(
        interface_ref,
        state,
        x11_connection,
        root_window,
    ));

    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let exit_result = tokio::select! {
        signal_result = tokio::signal::ctrl_c() => {
            signal_result.context("await SIGINT")?;
            tracing::info!("received SIGINT");
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
            Ok(())
        }
        joined = &mut poll_handle => match joined {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.context("poll loop exited with error")),
            Err(join_error) => Err(anyhow::anyhow!("poll loop task panicked: {join_error}")),
        },
        joined = &mut cleanup_handle => match joined {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.context("cleanup loop exited with error")),
            Err(join_error) => Err(anyhow::anyhow!("cleanup loop task panicked: {join_error}")),
        },
    };

    poll_handle.abort();
    cleanup_handle.abort();
    exit_result
}
