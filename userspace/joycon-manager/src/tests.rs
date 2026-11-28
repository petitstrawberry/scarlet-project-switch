use super::*;
use scarlet_ui::{RenderingPipeline, element::RenderElement, views::ButtonRenderObject};

fn buttons(element: &dyn Element, out: &mut Vec<Button>) {
    if let Some(button) = element
        .as_any()
        .downcast_ref::<RenderElement<Button, ButtonRenderObject>>()
    {
        out.push(button.view().clone());
    }
    for child in element.children() {
        buttons(child.as_ref(), out);
    }
}

fn controls(pipeline: &mut RenderingPipeline) -> Vec<Button> {
    pipeline.render_for_present().unwrap();
    let mut out = vec![];
    buttons(pipeline.element_tree().root().unwrap(), &mut out);
    out
}

fn press(controls: &[Button], label: &str) {
    controls
        .iter()
        .find(|b| b.label() == label)
        .expect(label)
        .invoke_on_click();
}

fn show(app: &Controllers, revision: u64, pads: Vec<Pad>) {
    app.inventory.set(Inventory {
        snapshot: Some(Snapshot { revision, pads }),
        error: None,
    });
    app.busy.set(false);
}

fn single(half: Half, player: u8) -> Pad {
    Pad {
        player,
        layout: Layout::Single {
            half,
            orientation: Orientation::Upright,
        },
    }
}

#[test]
fn rendered_controls_address_the_chosen_half_and_forget_disconnected_partners() {
    let (tx, rx) = mpsc::channel();
    let app = Controllers::new(tx);
    let left = single(Half::Left(2), 1);
    let first = single(Half::Right(0), 2);
    let second = single(Half::Right(4), 3);
    show(&app, 50, vec![left, first, second]);
    let mut pipeline = RenderingPipeline::new();
    pipeline.set_root(app.create_element());
    pipeline.layout_initial();
    pipeline.resize(Size::new(480.0, 540.0));
    press(&controls(&mut pipeline), "Combine…");
    let choices = controls(&mut pipeline);
    assert!(
        choices
            .iter()
            .any(|b| b.label() == "Combine with Right 1 · Player 2")
    );
    press(&choices, "Combine with Right 5 · Player 3");
    assert!(matches!(
        rx.try_recv(),
        Ok(Request::Apply(50, Edit::Combine { left: 2, right: 4 }))
    ));
    let pair = Pad {
        player: 1,
        layout: Layout::Pair { left: 2, right: 4 },
    };
    show(&app, 51, vec![pair, first]);
    press(&controls(&mut pipeline), "Use separately");
    assert!(matches!(
        rx.try_recv(),
        Ok(Request::Apply(51, Edit::Split { left: 2, right: 4 }))
    ));
    show(&app, 52, vec![left, first, second]);
    press(&controls(&mut pipeline), "Combine…");
    show(&app, 53, vec![left]);
    let remaining = controls(&mut pipeline);
    assert_eq!(
        remaining.iter().map(Button::label).collect::<Vec<_>>(),
        ["Use sideways"]
    );
    press(&remaining, "Use sideways");
    assert!(matches!(
        rx.try_recv(),
        Ok(Request::Apply(
            53,
            Edit::Orient {
                half: Half::Left(2),
                orientation: Orientation::Sideways
            }
        ))
    ));
    show(&app, 54, vec![]);
    assert!(controls(&mut pipeline).is_empty());
}

#[cfg(not(target_os = "scarlet"))]
#[test]
fn unavailable_service_is_distinct_from_an_empty_seat_and_can_be_retried() {
    let (tx, rx) = mpsc::channel();
    let app = Controllers::new(tx);
    app.requests.send(Request::Visible(true)).unwrap();
    app.requests.send(Request::Visible(false)).unwrap();
    let inventory = app.inventory.clone();
    let busy = app.busy.clone();
    drop(app);
    observe(rx, inventory.clone(), busy);
    assert!(inventory.get().snapshot.is_none());
    assert!(inventory.get().error.is_some());
    let (tx, rx) = mpsc::channel();
    let app = Controllers::new(tx);
    app.inventory.set(inventory.get());
    let mut pipeline = RenderingPipeline::new();
    pipeline.set_root(app.create_element());
    pipeline.layout_initial();
    pipeline.resize(Size::new(400.0, 360.0));
    press(&controls(&mut pipeline), "Retry");
    assert!(matches!(rx.try_recv(), Ok(Request::Refresh)));
}
