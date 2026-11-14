//! Execute the real discovery validators, without a database or business writes.
//! Run: cargo run -p bilibi-web --example field-vocabulary
use bilibi_agent_api::schema;
use bilibi_grid::{Field, GridContribution, Source, TableDecl};
use bilibi_module_registry::{Catalog, Consumer, Contribution, Provenance};
use bilibi_ui::{PersonalDataClass, RecordField, RecordFieldType};
use serde_json::{json, Value};

bilibi_ui::record_fields! { "base/grid", mod fields {
    ID: bilibi_ui::RecordField::new("id", "ID", bilibi_ui::RecordFieldType::Integer),
    STATE: bilibi_ui::RecordField::new("state", "State", bilibi_ui::RecordFieldType::Enum).choices(&["active", "pending"]),
    SEGMENT: bilibi_ui::RecordField::new("segment", "Segment", bilibi_ui::RecordFieldType::String).picklist("example.segment"),
} }
static GOOD: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new(
        "field_vocab_probe",
        "Vocabulary probe",
        "admin.person.list",
        Source::global("SELECT 1 AS id"),
        "id",
        &[Field::record(fields::ID)],
    )],
    links: &[],
};
static BAD: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new(
        "field_vocab_probe",
        "Vocabulary probe",
        "admin.person.list",
        Source::global("SELECT 1 AS id"),
        "id",
        &[Field {
            label: "Different label",
            ..Field::record(fields::ID)
        }],
    )],
    links: &[],
};
fn object(field: RecordField, constraints: Value) -> Value {
    json!({"type":"object","properties":{field.id:field.input(constraints)}})
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let modules = bilibi_web::modules()?;
    println!(
        "PASS: all installed vocabularies, Grid tables and operation contracts pass discovery"
    );
    let catalog = Catalog::discover(&modules)?;
    let email = catalog
        .entries::<RecordField>(bilibi_ui::RECORD_FIELDS)?
        .iter()
        .find(|entry| {
            entry.provenance.module == "core/person"
                && entry
                    .get::<RecordField>()
                    .is_ok_and(|field| field.id == "email")
        })
        .ok_or("missing contact email vocabulary")?
        .get::<RecordField>()?;
    assert_eq!(email.personal, Some(PersonalDataClass::Email));
    let valid = object(*email, json!({"maxLength":254,"nullable":true}));
    schema::check(&valid)?;
    assert!(schema::validate(&valid, &json!({"email":"person@example.org"})).is_ok());
    assert!(schema::validate(&valid, &json!({"email":12})).is_err());
    assert!(schema::validate(&valid, &json!({"unexpected":"value"})).is_err());
    println!("PASS: contact input preserves label, personal-data class, limits, nullable and unknown-field refusal");
    for (key, value) in [
        ("title", json!("Work email")),
        ("type", json!("integer")),
        ("x-bilibi-personal-data", json!("non_personal")),
        ("x-bilibi-picklist", json!("another.list")),
        ("x-bilibi-field", json!("missing.field")),
    ] {
        let mut invalid = valid.clone();
        invalid["properties"]["email"][key] = value;
        assert!(
            schema::check(&invalid).is_err(),
            "accepted conflicting {key}"
        );
        println!("PASS: contract refuses conflicting {key}");
    }
    schema::check(&object(fields::STATE, json!({"enum":["pending"]})))?;
    assert!(schema::check(&object(fields::STATE, json!({"enum":["unknown"]}))).is_err());
    schema::check(&object(fields::SEGMENT, json!({})))?;
    assert!(schema::check(&object(
        fields::SEGMENT,
        json!({"x-bilibi-picklist":"other.segment"})
    ))
    .is_err());
    assert!(schema::check(&object(
        RecordField {
            ty: RecordFieldType::Integer,
            ..fields::SEGMENT
        },
        json!({})
    ))
    .is_err());
    println!("PASS: choices may narrow; foreign choices, pick-lists and copied metadata fail");
    for (grid, should_pass) in [(&GOOD, true), (&BAD, false)] {
        let extra = Contribution::new(
            bilibi_grid::TABLES,
            "field-vocabulary.probe",
            Provenance {
                module: "base/grid",
                source: file!(),
                line: line!(),
            },
            grid,
        );
        let mut contributions: Vec<_> = inventory::iter::<Contribution>.into_iter().collect();
        contributions.push(&extra);
        let consumers: Vec<_> = inventory::iter::<Consumer>.into_iter().collect();
        let result = Catalog::build(&modules, consumers, contributions);
        assert_eq!(result.is_ok(), should_pass);
        if let Err(error) = result {
            assert!(
                error.to_string().contains("disagrees"),
                "unexpected refusal: {error}"
            );
            println!("PASS: boot refuses Grid disagreement: {error}");
        }
    }
    println!("PASS: vocabulary checks complete; no business data changed");
    Ok(())
}
