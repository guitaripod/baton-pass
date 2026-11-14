use super::control::{BUS_NAME, Request};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::OwnedObjectPath;

const ITEM_PATH: &str = "/StatusNotifierItem";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

/// What the panel icon shows: which machine has the mouse and keyboard, and how the link is.
#[derive(Clone, PartialEq, Debug)]
pub struct Look {
    pub icon: &'static str,
    pub status: &'static str,
    pub headline: String,
    pub details: String,
}

struct Item {
    look: Look,
    requests: mpsc::UnboundedSender<Request>,
}

type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "Hardware"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "baton-pass"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "Baton Pass"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        self.look.status
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        self.look.icon
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        (self.look.icon.to_string(), Vec::new(), self.look.headline.clone(), self.look.details.clone())
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/NO_DBUSMENU").expect("valid object path")
    }

    /// A left click carries the mouse and keyboard over, or brings them back.
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.requests.send(Request::Toggle(None));
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}

/// Baton Pass's icon in the panel's system tray, served on the daemon's own bus connection.
pub struct Tray {
    connection: zbus::Connection,
    shown: Look,
}

impl Tray {
    /// Publishes the icon and registers it with the panel, again whenever the panel restarts.
    pub async fn start(connection: &zbus::Connection, requests: mpsc::UnboundedSender<Request>, look: Look) -> zbus::Result<Self> {
        connection.object_server().at(ITEM_PATH, Item { look: look.clone(), requests }).await?;
        let watched = connection.clone();
        tokio::task::spawn_local(async move {
            register(&watched).await;
            let Ok(bus) = zbus::fdo::DBusProxy::new(&watched).await else { return };
            let Ok(mut owners) = bus.receive_name_owner_changed_with_args(&[(0, WATCHER)]).await else { return };
            while let Some(change) = owners.next().await {
                if change.args().is_ok_and(|args| args.new_owner().is_some()) {
                    register(&watched).await;
                }
            }
        });
        Ok(Self { connection: connection.clone(), shown: look })
    }

    /// Updates the icon and tooltip when they changed since the last call.
    pub async fn show(&mut self, look: Look) {
        if self.shown == look {
            return;
        }
        let Ok(item) = self.connection.object_server().interface::<_, Item>(ITEM_PATH).await else { return };
        let previous = std::mem::replace(&mut self.shown, look.clone());
        item.get_mut().await.look = look.clone();
        let emitter = item.signal_emitter();
        let icon_changed = previous.icon != look.icon;
        let status_changed = previous.status != look.status;
        if icon_changed && let Err(error) = Item::new_icon(emitter).await {
            tracing::debug!("tray icon update: {error}");
        }
        if status_changed && let Err(error) = Item::new_status(emitter, look.status).await {
            tracing::debug!("tray status update: {error}");
        }
        if let Err(error) = Item::new_tool_tip(emitter).await {
            tracing::debug!("tray tooltip update: {error}");
        }
    }
}

async fn register(connection: &zbus::Connection) {
    let result = connection
        .call_method(Some(WATCHER), "/StatusNotifierWatcher", Some(WATCHER), "RegisterStatusNotifierItem", &(BUS_NAME,))
        .await;
    if let Err(error) = result {
        tracing::debug!("no system tray to show the icon in: {error}");
    }
}
