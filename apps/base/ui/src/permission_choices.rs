//! Selectable permission keys, shared by role and user access forms.
use askama::Template;

#[derive(Clone, Debug)]
pub struct PermissionChoice {
    pub key: String,
    pub dom_id: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "permission_choices.html")]
pub struct PermissionChoices {
    pub options: Vec<PermissionChoice>,
}
