//! Mail clients get the same installation brand as web pages, but carry
//! the logo as a MIME part instead of fetching a short-lived signed URL.
use crate::{Branding, LOGO_MAX_BYTES};
use bilibi_mail::{Attachment, BrandFuture, BrandSource, MailBrand};
use tokio::io::AsyncReadExt;
use tokio_postgres::GenericClient;

impl BrandSource for Branding {
    fn current(&self) -> BrandFuture<'_> {
        Box::pin(async {
            match self.mail_brand().await {
                Ok(brand) => brand,
                Err(error) => {
                    bilibi_module_registry::server_log!(
                        Warning,
                        "warning: mail brand lookup failed: {error:?}"
                    );
                    MailBrand::default()
                }
            }
        })
    }
}

impl Branding {
    async fn mail_brand(&self) -> bilibi_database::Result<MailBrand> {
        let client = self.db.get().await?;
        let stored = self.stored(&**client).await?;
        let logo = self.mail_logo(&**client, stored.logo).await?;
        drop(client);

        let mut brand = MailBrand {
            name: stored.name,
            accent: stored.accent,
            logo: None,
        };
        if let Some(logo) = logo {
            let content_type = logo.mime_type.clone().unwrap_or_default();
            let filename = logo.filename.clone();
            let mut bytes = Vec::new();
            if logo.into_reader().read_to_end(&mut bytes).await.is_ok() && !bytes.is_empty() {
                brand.logo = Some(Attachment {
                    filename,
                    content_type,
                    bytes,
                });
            }
        }
        Ok(brand)
    }

    async fn mail_logo(
        &self,
        client: &(impl GenericClient + Sync),
        asset: Option<i64>,
    ) -> bilibi_database::Result<Option<bilibi_media::FileStream>> {
        let Some(asset) = asset else {
            return Ok(None);
        };
        let Some(operator) = bilibi_tenancy::platform_operator(client).await? else {
            return Ok(None);
        };
        match self
            .media
            .open_tenant_public_asset(client, operator.id, asset)
            .await
        {
            Ok(file)
                if file.len <= LOGO_MAX_BYTES
                    && matches!(
                        file.mime_type.as_deref(),
                        Some("image/png" | "image/jpeg" | "image/webp")
                    ) =>
            {
                Ok(Some(file))
            }
            Err(bilibi_media::Error::Database(error)) => Err(error),
            _ => Ok(None),
        }
    }
}
