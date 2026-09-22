//! One read-only AT-SPI2 question: what has focus right now?
//!
//! Vision models already read the screen; the tree is not their action space.
//! What pixels cannot give reliably is the exact text of the focused field,
//! the caret position, the active window's title and which application owns
//! it. `get_focused` answers just that, in text, and nothing else.
//!
//! Coordinates AT-SPI reports on Wayland are relative to the window and in
//! logical pixels; without the compositor they cannot be mapped to the
//! screenshot, so they are not reported.

use std::collections::HashMap;

use anyhow::{Context, Result};
use atspi::connection::AccessibilityConnection;
use atspi::proxy::{
    accessible::AccessibleProxy, cache::CacheProxy, text::TextProxy, value::ValueProxy,
};
use atspi::{Interface, Role, State, StateSet};
use futures_util::future::join_all;
use zbus::proxy::CacheProperties;
use zbus::Connection;

/// Longest text excerpt returned for the focused element.
const TEXT_LIMIT: usize = 2000;
/// Upper bound on nodes visited while looking for the focused element.
const WALK_LIMIT: usize = 4000;

const ROOT: &str = "/org/a11y/atspi/accessible/root";
const REGISTRY: &str = "org.a11y.atspi.Registry";

pub struct Ui {
    conn: Connection,
}

#[derive(Clone)]
struct Node {
    bus: String,
    path: String,
    role: Role,
    name: String,
    states: StateSet,
    children: Vec<String>,
}

impl Ui {
    pub async fn connect() -> Result<Self> {
        let env = crate::session::hydrate();
        crate::session::apply(&env);
        let a = AccessibilityConnection::new()
            .await
            .context("connect to the AT-SPI bus (is a graphical session running for this user?)")?;
        Ok(Self { conn: a.connection().clone() })
    }

    async fn acc(&self, bus: &str, path: &str) -> Result<AccessibleProxy<'static>> {
        Ok(AccessibleProxy::builder(&self.conn)
            .destination(bus.to_owned())?
            .path(path.to_owned())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }

    async fn node(&self, bus: &str, path: &str) -> Result<Node> {
        let a = self.acc(bus, path).await?;
        let (name, role, states, children) = tokio::join!(a.name(), a.get_role(), a.get_state(), a.get_children());
        Ok(Node {
            bus: bus.to_owned(),
            path: path.to_owned(),
            role: role.unwrap_or(Role::Invalid),
            name: name.unwrap_or_default(),
            states: states.unwrap_or_default(),
            children: children.unwrap_or_default().iter().map(|c| c.path_as_str().to_owned()).collect(),
        })
    }

    /// Every application on the bus with at least one window, with its windows.
    async fn apps(&self) -> Result<Vec<(Node, Vec<Node>)>> {
        let root = self.acc(REGISTRY, ROOT).await?;
        let refs = root.get_children().await.context("list applications")?;
        let apps = join_all(refs.iter().filter_map(|r| r.name_as_str().map(|n| self.node(n, r.path_as_str())))).await;
        let mut out = Vec::new();
        for app in apps.into_iter().flatten() {
            if app.children.is_empty() {
                continue;
            }
            let wins = join_all(app.children.iter().map(|p| self.node(&app.bus, p))).await;
            out.push((app, wins.into_iter().flatten().collect()));
        }
        Ok(out)
    }

    /// Depth-first search under `win` for the node with the FOCUSED state,
    /// pruning subtrees that are not showing. Uses the application's cache
    /// (one round trip) when it has one, live calls otherwise.
    async fn find_focused(&self, win: &Node) -> Result<Option<Node>> {
        let mut cache: HashMap<String, Node> = HashMap::new();
        if let Ok(c) = CacheProxy::builder(&self.conn)
            .destination(win.bus.clone())?
            .path("/org/a11y/atspi/cache")?
            .cache_properties(CacheProperties::No)
            .build()
            .await
        {
            if let Ok(items) = c.get_items().await {
                let mut kids: HashMap<String, Vec<(i32, String)>> = HashMap::new();
                for it in &items {
                    kids.entry(it.parent.path_as_str().to_owned())
                        .or_default()
                        .push((it.index, it.object.path_as_str().to_owned()));
                }
                for it in items {
                    let path = it.object.path_as_str().to_owned();
                    let mut ch = kids.remove(&path).unwrap_or_default();
                    ch.sort();
                    cache.insert(
                        path.clone(),
                        Node {
                            bus: win.bus.clone(),
                            path,
                            role: it.role,
                            name: it.name,
                            states: it.states,
                            children: ch.into_iter().map(|(_, p)| p).collect(),
                        },
                    );
                }
            }
        }
        let mut stack = vec![win.path.clone()];
        let mut seen = 0usize;
        while let Some(path) = stack.pop() {
            seen += 1;
            if seen > WALK_LIMIT {
                break;
            }
            let n = match cache.get(&path) {
                Some(n) => n.clone(),
                None => match self.node(&win.bus, &path).await {
                    Ok(n) => n,
                    Err(_) => continue,
                },
            };
            if n.states.contains(State::Focused) {
                return Ok(Some(n));
            }
            let hidden = !n.states.is_empty() && !n.states.contains(State::Showing) && path != win.path;
            if hidden || n.states.contains(State::Defunct) {
                continue;
            }
            stack.extend(n.children.iter().rev().cloned());
        }
        Ok(None)
    }

    /// The active window and its focused element, rendered as text.
    pub async fn get_focused(&self) -> Result<String> {
        let apps = self.apps().await?;
        let mut active: Option<(&Node, &Node)> = None;
        for (app, wins) in &apps {
            if let Some(w) = wins.iter().find(|w| w.states.contains(State::Active)) {
                active = Some((app, w));
                break;
            }
        }
        let Some((app, win)) = active else {
            let list: Vec<String> = apps
                .iter()
                .map(|(a, w)| format!("{} ({} window{})", a.name, w.len(), if w.len() == 1 { "" } else { "s" }))
                .collect();
            return Ok(format!(
                "No active window reports itself through AT-SPI (the focused app may not support accessibility). Apps on the bus with windows: {}",
                if list.is_empty() { "none".to_owned() } else { list.join(", ") }
            ));
        };
        let mut out = format!("App: {}\nWindow: {} \"{}\"\n", app.name, win.role, win.name);
        match self.find_focused(win).await? {
            None => out.push_str("Focused: nothing inside the window reports focus\n"),
            Some(n) => {
                let a = self.acc(&n.bus, &n.path).await?;
                let ifaces = a.get_interfaces().await.unwrap_or_default();
                let states: Vec<String> = n
                    .states
                    .iter()
                    .filter(|s| matches!(s, State::Editable | State::Checked | State::Selected | State::Expanded | State::Pressed | State::ReadOnly | State::Required | State::Invalid | State::Busy))
                    .map(|s| s.to_string().to_lowercase())
                    .collect();
                let disabled = !n.states.contains(State::Enabled) && !n.states.is_empty();
                out.push_str(&format!("Focused: {} \"{}\"", n.role, n.name));
                if !states.is_empty() || disabled {
                    let mut s = states;
                    if disabled {
                        s.push("disabled".into());
                    }
                    out.push_str(&format!(" [{}]", s.join(", ")));
                }
                if let Ok(d) = a.description().await {
                    if !d.is_empty() {
                        out.push_str(&format!(" ({d})"));
                    }
                }
                out.push('\n');
                if ifaces.contains(Interface::Value) {
                    if let Ok(v) = ValueProxy::builder(&self.conn)
                        .destination(n.bus.clone())?
                        .path(n.path.clone())?
                        .cache_properties(CacheProperties::No)
                        .build()
                        .await
                    {
                        if let (Ok(cur), Ok(lo), Ok(hi)) = (v.current_value().await, v.minimum_value().await, v.maximum_value().await) {
                            out.push_str(&format!("Value: {cur} (range {lo}..{hi})\n"));
                        }
                    }
                }
                if ifaces.contains(Interface::Text) && n.role != Role::Label {
                    if let Ok(t) = TextProxy::builder(&self.conn)
                        .destination(n.bus.clone())?
                        .path(n.path.clone())?
                        .cache_properties(CacheProperties::No)
                        .build()
                        .await
                    {
                        let count = t.character_count().await.unwrap_or(0);
                        let caret = t.caret_offset().await.unwrap_or(-1);
                        let sel = t.get_selection(0).await.ok().filter(|(s, e)| s != e);
                        if let Ok(text) = t.get_text(0, count).await {
                            let shown: String = text.chars().take(TEXT_LIMIT).collect();
                            let more = if text.chars().count() > TEXT_LIMIT { format!(" … ({count} chars total)") } else { String::new() };
                            out.push_str(&format!("Caret: {caret} of {count}"));
                            if let Some((s, e)) = sel {
                                out.push_str(&format!(", selection {s}..{e}"));
                            }
                            out.push_str(&format!("\nText:\n{shown}{more}\n"));
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}
