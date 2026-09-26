//! Bounded Wayland global shortcuts through the desktop portal, without X11 fallback.

use super::shortcut::{
    validate_shortcut_id, ShortcutTriggeredEvent, MAX_SHORTCUTS, SHORTCUT_TRIGGERED_EVENT,
};
use glib::variant::{ObjectPath, ToVariant};
use gtk::{gio, glib};
use std::{cell::RefCell, collections::BTreeMap, time::Duration};
use tauri::Emitter;
use tokio::sync::{mpsc, oneshot};

use super::desktop_portal::{connect, request, Properties, Subscription, PATH, SERVICE};
/// Version-one interface; no compositor-specific APIs are needed.
const INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
/// Current application shortcut set, bounded by the existing desktop API limit.
type Bindings = BTreeMap<String, String>;

thread_local! {
    /// A single main-context actor serializes native session changes for this application.
    static REQUESTS: RefCell<Option<mpsc::Sender<Request>>> = const { RefCell::new(None) };
}

/// Requested change to the complete application shortcut set.
pub(crate) enum Operation {
    /// Register or replace one stable identifier.
    Register(String, String),
    /// Remove one identifier and release its native binding.
    Remove(String),
    /// Release all bindings without opening a permission dialog.
    Clear,
}

/// One bounded command with its asynchronous reply.
struct Request {
    /// Desired registration change.
    operation: Operation,
    /// Completes only after the desktop accepts the updated bindings.
    reply: oneshot::Sender<Result<bool, String>>,
}

/// Dispatches to the GTK context while leaving the command and UI event loops unblocked.
pub(super) async fn execute(app: tauri::AppHandle, operation: Operation) -> Result<bool, String> {
    let (reply, receiver) = oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        REQUESTS.with(|slot| {
            let mut slot = slot.borrow_mut();
            let sender = slot.get_or_insert_with(|| {
                let (sender, receiver) = mpsc::channel(32);
                glib::MainContext::default().spawn_local(run(handle, receiver));
                sender
            });
            if let Err(error) = sender.try_send(Request { operation, reply }) {
                let _ = error
                    .into_inner()
                    .reply
                    .send(Err("Global shortcut request queue is full or closed".into()));
            }
        });
    })
    .map_err(|error| error.to_string())?;
    receiver.await.map_err(|error| error.to_string())?
}

/// Owns one portal session and its activation subscription.
struct Session {
    /// Private connection shared with the actor.
    connection: gio::DBusConnection,
    /// Native session object path.
    path: String,
    /// Activation listener, installed only after successful binding.
    subscription: Option<Subscription>,
}

impl Drop for Session {
    /// Releases native grabs even when binding or validation failed.
    fn drop(&mut self) {
        self.subscription.take();
        self.connection.call(
            Some(SERVICE),
            &self.path,
            "org.freedesktop.portal.Session",
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            3000,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}

/// Converts supported physical key names to the portal's XDG shortcut vocabulary.
fn trigger(accelerator: &str) -> Result<String, String> {
    let mut parts: Vec<String> = accelerator
        .split('+')
        .map(|part| part.trim().to_string())
        .collect();
    let key = parts.pop().ok_or("Missing shortcut key")?;
    let code = if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() {
        format!("Digit{key}")
    } else if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() {
        format!("Key{}", key.to_uppercase())
    } else {
        key
    };
    let code = code
        .parse::<keyboard_types::Code>()
        .map_err(|_| "Unsupported shortcut key")?
        .to_string();
    let key = if let Some(value) = code.strip_prefix("Digit") {
        value.to_string()
    } else if let Some(value) = code.strip_prefix("Key") {
        value.to_lowercase()
    } else if code.starts_with('F') && code[1..].parse::<u8>().is_ok() {
        code
    } else {
        match code.as_str() {
            "Enter" => "Return",
            "Space" => "space",
            "Escape" => "Escape",
            "Tab" => "Tab",
            "Backspace" => "BackSpace",
            "Delete" => "Delete",
            "Insert" => "Insert",
            "Home" => "Home",
            "End" => "End",
            "PageUp" => "Page_Up",
            "PageDown" => "Page_Down",
            "ArrowUp" => "Up",
            "ArrowDown" => "Down",
            "ArrowLeft" => "Left",
            "ArrowRight" => "Right",
            "Minus" => "minus",
            "Equal" => "equal",
            "Comma" => "comma",
            "Period" => "period",
            "Slash" => "slash",
            "Backslash" => "backslash",
            "Semicolon" => "semicolon",
            "Quote" => "apostrophe",
            "Backquote" => "grave",
            "BracketLeft" => "bracketleft",
            "BracketRight" => "bracketright",
            _ => {
                return Err(format!(
                    "This key is not supported by the Wayland shortcut backend: {code}"
                ))
            }
        }
        .to_string()
    };
    for part in &mut parts {
        *part = match part.to_lowercase().as_str() {
            "control" | "ctrl" | "commandorcontrol" | "cmdorctrl" => "CTRL",
            "super" | "meta" | "command" | "cmd" => "LOGO",
            "alt" => "ALT",
            "shift" => "SHIFT",
            _ => return Err("Unsupported shortcut modifier".into()),
        }
        .to_string();
    }
    parts.sort_by_key(|part| match part.as_str() {
        "SHIFT" => 0,
        "CTRL" => 1,
        "ALT" => 2,
        _ => 3,
    });
    parts.dedup();
    parts.push(key);
    Ok(parts.join("+"))
}

/// Creates and binds a complete replacement set; the caller retains its old session on failure.
async fn bind(
    app: &tauri::AppHandle,
    connection: &gio::DBusConnection,
    bindings: &Bindings,
    serial: u64,
) -> Result<Session, String> {
    let token = format!("bt_shortcuts_{serial}");
    let options = Properties::from([
        ("handle_token".into(), token.to_variant()),
        ("session_handle_token".into(), token.to_variant()),
    ]);
    let response = request(
        connection,
        INTERFACE,
        "CreateSession",
        &token,
        (options,).to_variant(),
    )
    .await?;
    let path = response
        .get("session_handle")
        .and_then(|value| value.get::<String>())
        .ok_or("Portal omitted its session handle")?;
    let mut session = Session {
        connection: connection.clone(),
        path,
        subscription: None,
    };
    let path = ObjectPath::try_from(session.path.as_str()).map_err(|error| error.to_string())?;
    let shortcuts = bindings
        .iter()
        .map(|(id, accelerator)| {
            Ok((
                id.clone(),
                Properties::from([
                    (
                        "description".into(),
                        format!("{id} ({accelerator})").to_variant(),
                    ),
                    (
                        "preferred_trigger".into(),
                        trigger(accelerator)?.to_variant(),
                    ),
                ]),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let token = format!("bt_bind_{serial}");
    let options = Properties::from([("handle_token".into(), token.to_variant())]);
    let response = request(
        connection,
        INTERFACE,
        "BindShortcuts",
        &token,
        (path, shortcuts, "", options).to_variant(),
    )
    .await?;
    let accepted = response
        .get("shortcuts")
        .and_then(|value| value.get::<Vec<(String, Properties)>>())
        .ok_or("Portal omitted accepted shortcuts")?;
    if accepted.len() != bindings.len() || accepted.iter().any(|(id, _)| !bindings.contains_key(id))
    {
        return Err("The desktop did not authorize every requested shortcut".into());
    }
    let bindings = bindings.clone();
    let handle = app.clone();
    let expected = session.path.clone();
    let id = connection.signal_subscribe(
        Some(SERVICE),
        Some(INTERFACE),
        Some("Activated"),
        Some(PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, value| {
            let Some((path, id, _, _)) = value.get::<(ObjectPath, String, u64, Properties)>()
            else {
                return;
            };
            if path.as_str() != expected {
                return;
            }
            if let Some(accelerator) = bindings.get(&id) {
                let _ = handle.emit_to(
                    "main",
                    SHORTCUT_TRIGGERED_EVENT,
                    ShortcutTriggeredEvent {
                        shortcut_id: id,
                        accelerator: accelerator.clone(),
                    },
                );
            }
        },
    );
    session.subscription = Some(Subscription {
        connection: connection.clone(),
        id: Some(id),
    });
    Ok(session)
}

/// Applies one bounded change without mutating the committed native session.
fn apply(bindings: &mut Bindings, operation: &Operation) -> Result<bool, String> {
    match operation {
        Operation::Register(id, accelerator) => {
            let id = validate_shortcut_id(id.clone())?;
            let accelerator = accelerator.trim();
            if accelerator.is_empty() || accelerator.len() > 128 {
                return Err("Accelerator cannot be empty or exceed 128 characters".into());
            }
            let normalized = trigger(accelerator)?;
            if bindings.iter().any(|(other, value)| {
                other != &id && trigger(value).ok().as_ref() == Some(&normalized)
            }) {
                return Err("Shortcut is already registered by another identifier".into());
            }
            if !bindings.contains_key(&id) && bindings.len() >= MAX_SHORTCUTS {
                return Err(format!(
                    "Global shortcut count cannot exceed {MAX_SHORTCUTS}"
                ));
            }
            bindings.insert(id, accelerator.to_string());
            Ok(true)
        }
        Operation::Remove(id) => Ok(bindings
            .remove(&validate_shortcut_id(id.clone())?)
            .is_some()),
        Operation::Clear => {
            bindings.clear();
            Ok(true)
        }
    }
}

/// Coalesces concurrent registrations into one consent request and retains only one live set.
async fn run(app: tauri::AppHandle, mut receiver: mpsc::Receiver<Request>) {
    let mut bindings = Bindings::new();
    let mut connection = None;
    let mut session = None;
    let mut serial = 0u64;
    while let Some(first) = receiver.recv().await {
        glib::timeout_future(Duration::from_millis(50)).await;
        let mut requests = vec![first];
        while requests.len() < 32 {
            match receiver.try_recv() {
                Ok(request) => requests.push(request),
                Err(_) => break,
            }
        }
        let mut candidate = bindings.clone();
        let mut replies = Vec::with_capacity(requests.len());
        for request in requests {
            match apply(&mut candidate, &request.operation) {
                Ok(changed) => replies.push((request.reply, changed)),
                Err(error) => {
                    let _ = request.reply.send(Err(error));
                }
            }
        }
        let result = if candidate == bindings {
            Ok(())
        } else if candidate.is_empty() {
            session.take();
            Ok(())
        } else {
            serial += 1;
            let result = async {
                if connection
                    .as_ref()
                    .is_none_or(gio::DBusConnection::is_closed)
                {
                    connection = Some(connect(&app).await?);
                }
                bind(&app, connection.as_ref().unwrap(), &candidate, serial).await
            }
            .await;
            match result {
                Ok(next) => {
                    session = Some(next);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        };
        if result.is_ok() {
            bindings = candidate;
        }
        for (reply, changed) in replies {
            let _ = reply.send(result.clone().map(|()| changed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numeric and letter accelerators use XDG symbols rather than keyboard-code names.
    #[test]
    fn portal_trigger_and_registry_validation() {
        assert_eq!(trigger("Alt+1").unwrap(), "ALT+1");
        assert_eq!(trigger("Control+Shift+KeyA").unwrap(), "SHIFT+CTRL+a");
        let mut values = Bindings::new();
        apply(
            &mut values,
            &Operation::Register("one".into(), "Alt+1".into()),
        )
        .unwrap();
        assert!(apply(
            &mut values,
            &Operation::Register("two".into(), "Alt+Digit1".into())
        )
        .is_err());
        assert!(apply(&mut values, &Operation::Remove("one".into())).unwrap());
        assert!(values.is_empty());
    }
}
