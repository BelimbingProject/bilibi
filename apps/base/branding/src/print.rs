use crate::{Branding, LOGO_MAX_BYTES};
use bilibi_ui::{Accent, PRODUCT};
use tokio::io::AsyncReadExt;
use tokio_postgres::GenericClient;

/// Installation identity for a renderer with no network access.
#[derive(Default)]
pub struct PrintBrand {
    name: String,
    logo: Option<PrintLogo>,
    accent: Accent,
    include_mark: bool,
}

pub struct PrintLogo {
    mime: &'static str,
    bytes: Vec<u8>,
}

impl PrintLogo {
    pub fn mime(&self) -> &'static str {
        self.mime
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl PrintBrand {
    pub fn name(&self) -> &str {
        if self.name.is_empty() {
            PRODUCT
        } else {
            &self.name
        }
    }

    pub fn logo(&self) -> Option<&PrintLogo> {
        self.logo.as_ref()
    }

    pub fn include_mark(&self) -> bool {
        self.include_mark
    }

    pub fn use_product_mark(&self) -> bool {
        self.name.is_empty() && self.logo.is_none()
    }

    pub fn accent(&self) -> Accent {
        self.accent
    }
}

impl Branding {
    /// Resolve the same installation settings as the staff shell. If the
    /// product mark is requested, load the operator's approved public raster
    /// through Media and keep the signed link inside this boundary.
    pub async fn for_print(
        &self,
        client: &(impl GenericClient + Sync),
        include_mark: bool,
    ) -> bilibi_database::Result<PrintBrand> {
        let stored = self.stored(client).await?;
        let logo = match stored.logo.filter(|_| include_mark) {
            Some(id) => self.logo_bytes(client, id).await?,
            None => None,
        };
        Ok(PrintBrand {
            name: stored.name,
            logo,
            accent: stored.accent,
            include_mark,
        })
    }

    async fn logo_bytes(
        &self,
        client: &(impl GenericClient + Sync),
        asset: i64,
    ) -> bilibi_database::Result<Option<PrintLogo>> {
        let Some(url) = self.logo_url(client, Some(asset)).await? else {
            return Ok(None);
        };
        let Some((_, token)) = url.split_once("?token=") else {
            return Ok(None);
        };
        let file = match self.media.open_signed_asset(client, asset, token).await {
            Ok(file) => file,
            Err(bilibi_media::Error::Database(error)) => return Err(error),
            Err(_) => return Ok(None),
        };
        let mime = match file.mime_type.as_deref() {
            Some("image/png") => "image/png",
            Some("image/jpeg") => "image/jpeg",
            Some("image/webp") => "image/webp",
            _ => return Ok(None),
        };
        if !(1..=LOGO_MAX_BYTES).contains(&file.len) {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(file.len as usize);
        let len = file.len as usize;
        if file.into_reader().read_to_end(&mut bytes).await.ok() != Some(len) {
            return Ok(None);
        }
        Ok(Some(PrintLogo { mime, bytes }))
    }
}
