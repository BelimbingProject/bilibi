//! One tenant, using the same reviewed rename contract as agents.
use crate::{web::Pages, DETAIL, PAGES, RENAME};
use askama::Template;
use bilibi_agent_api::{page::Call, Error as ContractError};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState};
use bilibi_ui::{
    BackLink, Card, CommitStates, CommitStatus, Fact, Facts, Header, Html, InlineEdit, InlineField,
    Page as UiPage, PageVariant,
};
use serde_json::{json, Value};

fn render(template: impl Template) -> Result<Html> {
    Html::render(&template).map_err(|_| Error::Internal)
}

#[derive(Template)]
#[template(
    source = r#"{% import "workspace.html" as workspace %}{% call workspace::announce(kind="core/tenant", record_id=id) %}{% endcall %}{{ body }}"#,
    ext = "html"
)]
struct View {
    id: String,
    body: Html,
}

pub(super) struct Detail {
    pages: Pages,
    id: i64,
    cached: Option<Value>,
    statuses: CommitStates,
    notice: String,
}
impl Detail {
    pub fn new(pages: Pages, id: i64) -> Self {
        Self {
            pages,
            id,
            cached: None,
            statuses: CommitStates::default(),
            notice: String::new(),
        }
    }
    async fn body(&mut self, scope: &Scope) -> Result<String> {
        let (tenant, editable) = match self
            .pages
            .contracts
            .read(scope, DETAIL, &json!({"tenant_id":self.id}))
            .await
        {
            Ok(value) => {
                let tenant = value["result"].clone();
                self.cached = Some(tenant.clone());
                let client = self.pages.db.get().await?;
                let allowed = self
                    .pages
                    .authorization
                    .authorize_scope(&**client, scope, bilibi_tenancy::RENAME)
                    .await;
                let editable = match allowed {
                    Ok(()) if self.id == scope.tenant() => true,
                    Ok(()) => match bilibi_tenancy::prove_operator(&**client, scope).await {
                        Ok(_) => true,
                        Err(Error::Forbidden) => false,
                        Err(error) => return Err(error),
                    },
                    Err(Error::Forbidden) => false,
                    Err(error) => return Err(error),
                };
                (tenant, editable)
            }
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                (self.cached.clone().ok_or(Error::Forbidden)?, false)
            }
            Err(ContractError::Backend(error)) => return Err(error),
            Err(_) => return Err(Error::Internal),
        };
        let name = tenant["name"].as_str().ok_or(Error::Internal)?;
        let version = tenant["name_version"].as_i64().ok_or(Error::Internal)?;
        let mut field = InlineField::new("tenant-name", "name", "Name", name)
            .version(version)
            .status(self.statuses.get("name"));
        field.id_value = self.id.to_string();
        if !editable {
            field = field.readonly();
        }
        let facts = render(Facts {
            id: "tenant-facts",
            items: vec![Fact {
                title: "Name",
                value: render(InlineEdit { field })?,
                ..Fact::default()
            }],
        })?;
        let mut content = Html::EMPTY;
        {
            content.push(&render(BackLink {
                href: "/tenancy/tenants",
                title: "Back to tenants",
                ..BackLink::default()
            })?);
        }
        content.push(&render(Header {
            title: Html::text(name),
            ..Header::default()
        })?);
        content.push(&render(Card {
            id: "tenant-detail",
            body: facts,
            ..Card::default()
        })?);
        let body = render(UiPage {
            id: "tenant-record",
            variant: PageVariant::Detail,
            body: content,
            ..UiPage::default()
        })?;
        Ok(render(View {
            id: self.id.to_string(),
            body,
        })?
        .to_string())
    }
}
impl LiveState<Scope, Error> for Detail {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.notice.clear();
            if let LiveInput::Event(value) = request.input {
                if value["type"].as_str() != Some("save_field")
                    || value["field"].as_str() != Some("name")
                    || value["id"].as_str() != Some(&self.id.to_string())
                {
                    return Err(Error::Invalid);
                }
                let submitted = value["value"].as_str().ok_or(Error::Invalid)?;
                let version = value["version"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                let call = Call {
                    scope: request.scope,
                    credential: request.credential,
                    service: PAGES,
                    event: "save_field",
                    contract: RENAME,
                };
                let status = match self
                    .pages
                    .contracts
                    .submit(
                        &call,
                        json!({"tenant_id":self.id,"name":submitted,"version":version}),
                    )
                    .await
                {
                    Ok(_) => CommitStatus::Saved,
                    Err(ContractError::InvalidInput(fields)) => {
                        let messages: Vec<&str> = fields
                            .get("name")
                            .into_iter()
                            .flatten()
                            .map(String::as_str)
                            .collect();
                        CommitStatus::refusal("Name", submitted, &messages)
                    }
                    Err(ContractError::Backend(Error::Conflict)) => CommitStatus::refusal(
                        "Name",
                        submitted,
                        &["has changed since this edit began; cancel and try again"],
                    ),
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.notice = "You do not have permission to rename this tenant.".into();
                        self.statuses.clear_saved();
                        CommitStatus::Error(self.notice.clone())
                    }
                    Err(ContractError::Backend(
                        error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(error),
                    Err(_) => CommitStatus::failure(),
                };
                self.statuses.put("name", status);
            }
            let html = self.body(request.scope).await?;
            Ok(LiveBody {
                html,
                notice: self.notice.clone(),
                ..LiveBody::default()
            })
        })
    }
}
