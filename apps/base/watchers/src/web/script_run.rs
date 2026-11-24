//! The approved script card shared by each version on the watcher record page.
use super::*;

pub(super) fn cards(
    definition: &crate::definition::Definition,
    dom: &str,
    version: i32,
) -> Result<Html> {
    let mut cards = Html::EMPTY;
    for (index, script) in definition.scripts().enumerate() {
        let form_id = format!("{dom}-script-{}-run", index + 1);
        let mut fields = Html::EMPTY;
        for field in [
            Field::named("version")
                .kind(InputKind::Hidden)
                .value(version.to_string()),
            Field::named("script")
                .kind(InputKind::Hidden)
                .value((index + 1).to_string()),
            Field::named("subject").label("Record ID").hint(format!(
                "Bilibi reads these approved fields and the current version as you: {}.",
                serde_json::to_string(&script.fields).unwrap_or_default()
            )),
        ] {
            fields.push(&render(field)?);
        }
        let action = render(Button {
            variant: ButtonVariant::Primary,
            id: &format!("{form_id}-submit"),
            button_type: "submit",
            body: Html::text("Review and run"),
            ..Button::default()
        })?;
        let form = Form {
            id: &form_id,
            event: "run_script",
            body: fields,
            actions: action,
            ..Form::default()
        };
        let mut run = render(SectionHeading {
            title: &format!("Run {}", script.name),
            id: &format!("{form_id}-heading"),
            description:
                "This page uses the same reviewed operation as the command line and a delegated agent.",
            ..SectionHeading::default()
        })?;
        run.push(&render(form)?);
        cards.push(&render(Card {
            id: &format!("{form_id}-card"),
            inner_class: "p-4",
            role: "region",
            labelledby: &format!("{form_id}-heading"),
            body: run,
            ..Card::default()
        })?);
    }
    Ok(cards)
}
