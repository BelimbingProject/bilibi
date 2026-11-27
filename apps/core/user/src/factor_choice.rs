//! Neutral 2FA choices around Core User's authenticator pages. Method-specific setup and prompts belong to their owner.
use crate::{two_factor, two_factor_ui, UserService};
use askama::Template;
use bilibi_database::Error;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Button, ButtonVariant, Header, Html,
};
use std::{collections::HashMap, sync::Arc};

pub async fn state(
    users: Arc<UserService>,
    scope: &Scope,
    query: &HashMap<String, String>,
    confirm: bool,
) -> Result<Box<dyn LiveState<Scope, Error>>, Error> {
    let portal = scope.actor().kind() == bilibi_tenancy::AccountKind::Customer;
    let back = query
        .get("return")
        .and_then(|path| crate::safe_return(path))
        .filter(|path| !portal || two_factor_ui::portal_path(path));
    let mut choices = Choices {
        users,
        back,
        confirm,
        portal,
        redirect_method: None,
    };
    let methods = choices.methods(scope).await?;
    let selected = query
        .get("method")
        .map(String::as_str)
        .or_else(|| match methods.as_slice() {
            [method]
                if !confirm || (method.enrolled && method.method.id == two_factor::METHOD.id) =>
            {
                Some(method.method.id)
            }
            _ => None,
        });
    if selected == Some(two_factor::METHOD.id) {
        return Ok(if confirm {
            Box::new(two_factor_ui::ConfirmPage::new(choices.users, scope, query).await?)
        } else {
            Box::new(two_factor_ui::TwoFactorPage::new(choices.users, scope, query).await?)
        });
    }
    if selected.is_some_and(|id| !methods.iter().any(|m| m.method.id == id)) {
        return Err(Error::Invalid);
    }
    choices.redirect_method = selected.map(str::to_owned);
    Ok(Box::new(choices))
}

struct Choices {
    users: Arc<UserService>,
    back: Option<String>,
    confirm: bool,
    portal: bool,
    redirect_method: Option<String>,
}
impl Choices {
    fn destination<'a>(&self, method: &'a bilibi_step_up::MethodChoice) -> &'a str {
        if self.confirm && method.enrolled {
            &method.prompt
        } else {
            &method.setup
        }
    }

    async fn methods(&self, scope: &Scope) -> Result<Vec<bilibi_step_up::MethodChoice>, Error> {
        let mut methods = self
            .users
            .step_up
            .methods(scope, self.back.as_deref().unwrap_or(""))
            .await?;
        if self.portal {
            methods.retain(|method| two_factor_ui::portal_path(self.destination(method)));
        }
        Ok(methods)
    }
}
#[derive(Template)]
#[template(path = "factor_choices.html")]
struct View {
    header: Html,
    confirm: bool,
    choices: Vec<Html>,
}
fn render(value: &impl Template) -> Result<Html, Error> {
    Html::render(value).map_err(|_| Error::Internal)
}
impl LiveState<Scope, Error> for Choices {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if !matches!(request.input, LiveInput::Refresh) {
                return Err(Error::Invalid);
            }
            let methods = self.methods(request.scope).await?;
            let mut choices = Vec::new();
            let mut redirect = None;
            for method in methods {
                let destination = self.destination(&method);
                if self.redirect_method.as_deref() == Some(method.method.id) {
                    redirect = Some(destination.to_owned());
                }
                choices.push(render(&Button {
                    href: destination,
                    variant: ButtonVariant::Secondary,
                    body: Html::text(&format!(
                        "{}{}",
                        method.method.label,
                        if method.enrolled { "" } else { " · Set up" }
                    )),
                    ..Button::default()
                })?);
            }
            let html = View {
                header: render(&Header {
                    title: Html::text(if self.confirm {
                        "Confirm access"
                    } else {
                        "My settings"
                    }),
                    subtitle: "Choose a 2FA method.",
                    ..Header::default()
                })?,
                confirm: self.confirm,
                choices,
            }
            .render()
            .map_err(|_| Error::Internal)?;
            let query = self
                .back
                .iter()
                .map(|p| ("return".into(), p.clone()))
                .collect();
            Ok(LiveBody {
                html,
                query,
                redirect,
                rows: 1,
                ..LiveBody::default()
            })
        })
    }
}
