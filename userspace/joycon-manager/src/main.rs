use joycond::{Edit, Half, Layout, Orientation, Pad, Snapshot};
use scarlet_ui::prelude::*;
use scarlet_ui::views::containers::ViewTuple;
use scarlet_ui::{
    ComponentElement, PlatformWindow, Surface, SurfaceRole, TextStyle, TextTone, hstack,
};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Clone, Default, PartialEq, Eq)]
struct Inventory {
    snapshot: Option<Snapshot>,
    error: Option<String>,
}

enum Request {
    Visible(bool),
    Refresh,
    Apply(u64, Edit),
}

// Keep IPC off the UI thread. Only an open, visible window refreshes for
// changes from physical grip gestures or controllers connecting/disconnecting.
fn observe(requests: Receiver<Request>, inventory: State<Inventory>, busy: State<bool>) {
    let mut visible = false;
    loop {
        let request = if visible {
            match requests.recv_timeout(Duration::from_secs(2)) {
                Ok(request) => request,
                Err(RecvTimeoutError::Timeout) => Request::Refresh,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            let Ok(request) = requests.recv() else { return };
            request
        };
        let mut error = None;
        let editing = matches!(request, Request::Apply(..));
        let result = match request {
            Request::Visible(false) => {
                visible = false;
                continue;
            }
            Request::Visible(true) => {
                visible = true;
                joycond::list()
            }
            Request::Refresh => joycond::list(),
            Request::Apply(revision, edit) => joycond::apply(revision, edit).or_else(|reason| {
                error = Some(reason);
                joycond::list()
            }),
        };
        let old = inventory.get();
        let next = match result {
            Ok(snapshot) => {
                if !editing && old.snapshot.as_ref() == Some(&snapshot) {
                    error = old.error.clone();
                }
                Inventory {
                    snapshot: Some(snapshot),
                    error,
                }
            }
            Err(reason) => Inventory {
                snapshot: None,
                error: Some(reason),
            },
        };
        if next != old {
            inventory.set(next);
        }
        if editing {
            busy.set(false);
        }
    }
}

#[derive(Default, Clone)]
struct Rows(Vec<Box<dyn View>>);

impl Rows {
    fn push(&mut self, view: impl View) {
        self.0.push(Box::new(view));
    }
}

impl ViewTuple for Rows {
    fn create_elements(&self) -> Vec<Box<dyn Element>> {
        self.0.iter().map(|v| v.create_element()).collect()
    }
    fn clone_views(&self) -> Vec<Box<dyn View>> {
        self.0.clone()
    }
    fn collect_listenables<'a>(&'a self, out: &mut Vec<&'a dyn Listenable>) {
        for view in &self.0 {
            out.extend(view.listenables());
        }
    }
}

fn half_label(half: Half) -> String {
    match half {
        Half::Left(i) => format!("Left {}", i + 1),
        Half::Right(i) => format!("Right {}", i + 1),
    }
}

fn partners(snapshot: &Snapshot, selected: Half) -> Vec<(Half, u8, Edit)> {
    snapshot
        .pads
        .iter()
        .filter_map(|&pad| {
            let Layout::Single { half, .. } = pad.layout else {
                return None;
            };
            let (left, right) = match (selected, half) {
                (Half::Left(left), Half::Right(right)) | (Half::Right(right), Half::Left(left)) => {
                    (left, right)
                }
                _ => return None,
            };
            Some((half, pad.player, Edit::Combine { left, right }))
        })
        .collect()
}

#[derive(Clone)]
struct Controllers {
    inventory: State<Inventory>,
    choosing: State<Option<(u64, Half)>>,
    busy: State<bool>,
    requests: Sender<Request>,
}

impl Controllers {
    fn edit(&self, label: String, revision: u64, edit: Edit) -> Button {
        let app = self.clone();
        Button::new(label)
            .enabled(!self.busy.get())
            .on_click(move || {
                if app.busy.get() {
                    return;
                }
                app.busy.set(true);
                app.choosing.set(None);
                if app.requests.send(Request::Apply(revision, edit)).is_err() {
                    app.inventory.set(Inventory {
                        snapshot: None,
                        error: Some("Joy-Con service connection closed.".into()),
                    });
                    app.busy.set(false);
                }
            })
    }

    fn card(&self, snapshot: &Snapshot, pad: Pad) -> impl View + Clone + use<> {
        let rev = snapshot.revision;
        let mut actions = Rows::default();
        let mut chooser = Rows::default();
        let description = match pad.layout {
            Layout::Single { half, orientation } => {
                let (mode, label, next) = match orientation {
                    Orientation::Upright => ("Upright", "Use sideways", Orientation::Sideways),
                    Orientation::Sideways => ("Sideways", "Use upright", Orientation::Upright),
                };
                actions.push(self.edit(
                    label.into(),
                    rev,
                    Edit::Orient {
                        half,
                        orientation: next,
                    },
                ));
                let peers = partners(snapshot, half);
                if !peers.is_empty() {
                    let open = self.choosing.get() == Some((rev, half));
                    let choosing = self.choosing.clone();
                    actions.push(
                        Button::new(if open { "Cancel" } else { "Combine…" })
                            .enabled(!self.busy.get())
                            .on_click(move || choosing.set((!open).then_some((rev, half)))),
                    );
                    if open {
                        for (peer, player, edit) in peers {
                            chooser.push(self.edit(
                                format!("Combine with {} · Player {player}", half_label(peer)),
                                rev,
                                edit,
                            ));
                        }
                    }
                }
                format!("{} · {mode}", half_label(half))
            }
            Layout::Pair { left, right } => {
                actions.push(self.edit("Use separately".into(), rev, Edit::Split { left, right }));
                format!(
                    "{} + {}",
                    half_label(Half::Left(left)),
                    half_label(Half::Right(right))
                )
            }
        };
        let mut content = Rows::default();
        content.push(
            hstack! {
                IconView::new(Icon::User),
                Text::new(format!("Player {}", pad.player)).text_style(TextStyle::Headline),
            }
            .spacing(8.0),
        );
        content.push(Text::new(description).tone(TextTone::Secondary));
        content.push(HStack::new(actions).spacing(8.0));
        content.0.extend(chooser.0);
        let body = VStack::new(content)
            .alignment(Alignment::Leading)
            .spacing(9.0)
            .padding(16.0)
            .frame_width(f32::INFINITY)
            .content_alignment(Alignment::Leading);
        Surface::new(body, SurfaceRole::Section).corner_radius(12.0)
    }

    fn content(&self) -> impl View + Clone + use<> {
        let inventory = self.inventory.get();
        let mut rows = Rows::default();
        rows.push(Text::new("Connected controllers").text_style(TextStyle::Title));
        match &inventory.snapshot {
            Some(snapshot) if snapshot.pads.is_empty() => {
                rows.push(Text::new("No wireless Joy-Con connected."))
            }
            Some(snapshot) => {
                let mut pads = snapshot.pads.clone();
                pads.sort_by_key(|pad| pad.player);
                for pad in pads {
                    let key = match pad.layout {
                        Layout::Single { half, .. } => half_label(half),
                        Layout::Pair { left, right } => format!("pair-{left}-{right}"),
                    };
                    rows.push(self.card(snapshot, pad).key(key));
                }
            }
            None if inventory.error.is_none() => rows.push(Text::new("Loading controllers…")),
            None => {}
        }
        if let Some(error) = inventory.error {
            rows.push(
                Text::new(error)
                    .text_style(TextStyle::Callout)
                    .line_limit(None),
            );
            if inventory.snapshot.is_none() {
                let requests = self.requests.clone();
                rows.push(Button::new("Retry").on_click(move || {
                    let _ = requests.send(Request::Refresh);
                }));
            }
        }
        ScrollView::new(
            VStack::new(rows)
                .spacing(14.0)
                .alignment(Alignment::Leading)
                .padding(20.0),
        )
        .vertical()
    }
}

impl View for Controllers {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(ComponentElement::new_with_builder(self.clone(), |app| {
            Box::new(app.content())
        }))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.inventory, &self.choosing, &self.busy]
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Application for Controllers {
    fn scenes(&self) -> impl Scene {
        Window::new("Joy-Con", self.clone()).size(Size::new(560.0, 560.0))
    }
    fn on_window_created(&mut self, _: &WindowContext, _: &mut dyn PlatformWindow) {
        let _ = self.requests.send(Request::Visible(true));
    }
    fn on_window_suspended_changed(&mut self, _: &WindowContext, suspended: bool) {
        let _ = self.requests.send(Request::Visible(!suspended));
    }
}

impl Controllers {
    fn new(requests: Sender<Request>) -> Self {
        Self {
            inventory: State::initial(StateId::new(1)),
            choosing: State::initial(StateId::new(2)),
            busy: State::initial(StateId::new(3)),
            requests,
        }
    }
}

#[cfg(not(test))]
fn main() -> scarlet_ui::Result<()> {
    let (requests, receiver) = mpsc::channel();
    let mut app = Controllers::new(requests);
    let inventory = app.inventory.clone();
    let busy = app.busy.clone();
    std::thread::spawn(move || observe(receiver, inventory, busy));
    app.run()
}

#[cfg(test)]
mod tests;
