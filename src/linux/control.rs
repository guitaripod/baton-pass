use anyhow::{Context, Result};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

pub const BUS_NAME: &str = "dev.ferry.Ferry";
pub const OBJECT_PATH: &str = "/dev/ferry/Ferry";

pub enum Request {
    Toggle(Option<(f64, f64)>),
    Status(oneshot::Sender<String>),
}

struct Service(mpsc::UnboundedSender<Request>);

#[zbus::interface(name = "dev.ferry.Ferry")]
impl Service {
    /// Called by the KWin script with the cursor position as strings, or by `ferry toggle` with empty ones.
    async fn toggle(&self, x: String, y: String) {
        let position = x.trim().parse::<f64>().ok().zip(y.trim().parse::<f64>().ok());
        let _ = self.0.send(Request::Toggle(position));
    }

    async fn status(&self) -> String {
        let (reply, answer) = oneshot::channel();
        if self.0.send(Request::Status(reply)).is_err() {
            return "ferry is shutting down".into();
        }
        answer.await.unwrap_or_else(|_| "no answer".into())
    }
}

pub async fn serve(requests: mpsc::UnboundedSender<Request>) -> Result<zbus::Connection> {
    zbus::connection::Builder::session()
        .name(BUS_NAME)
        .serve_at(OBJECT_PATH, Service(requests))
        .build()
        .await
        .context("claiming dev.ferry.Ferry on the session bus (is another ferry running?)")
}

pub async fn notify(connection: &zbus::Connection, summary: &str, body: &str) {
    let hints: HashMap<&str, zbus::Variant> = HashMap::new();
    let result = connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("Ferry", 0u32, "input-mouse", summary, body, Vec::<&str>::new(), hints, 4000i32),
        )
        .await;
    if let Err(error) = result {
        tracing::debug!("notification failed: {error}");
    }
}

async fn call(method: &str, body: &(&str, &str)) -> Result<zbus::message::Message> {
    let connection = zbus::Connection::session().await?;
    connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(BUS_NAME), method, body)
        .await
        .context("ferry is not running (systemctl --user status ferry)")
}

async fn call_unit(method: &str) -> Result<zbus::message::Message> {
    let connection = zbus::Connection::session().await?;
    connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(BUS_NAME), method, &())
        .await
        .context("ferry is not running (systemctl --user status ferry)")
}

pub async fn toggle() -> Result<()> {
    call("Toggle", &("", "")).await?;
    Ok(())
}

pub async fn status() -> Result<()> {
    let reply = call_unit("Status").await?;
    let text: String = reply.body().deserialize()?;
    println!("{text}");
    Ok(())
}
