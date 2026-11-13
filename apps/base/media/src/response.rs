//! How stored bytes leave the server. Only raster images render inline; every
//! other type, and every private file, downloads as an opaque octet stream
//! under a sandboxing policy. The filename is percent-encoded (RFC 5987) so it
//! cannot inject headers.
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    fs::File,
    io::{AsyncRead, ReadBuf, Take},
    sync::OwnedSemaphorePermit,
};

const INLINE: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

pub struct StreamFile {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime_type: Option<String>,
}
pub struct Delivery {
    pub content_type: String,
    pub headers: Vec<(&'static str, String)>,
}
impl StreamFile {
    pub fn delivery(&self, private: bool) -> Delivery {
        delivery(&self.filename, self.mime_type.as_deref(), private)
    }
}

/// A stored file opened for the Web host to send as it reads, never held in
/// memory whole: what [`crate::Media::open_signed_asset`] and
/// [`crate::Media::open_attachment`] return. It holds one of Media's stream
/// slots until the reader is dropped, and no database connection.
pub struct FileStream {
    pub filename: String,
    pub mime_type: Option<String>,
    /// The bytes the reader yields: send it as `content-length`.
    pub len: u64,
    pub(crate) reader: Take<File>,
    pub(crate) permit: OwnedSemaphorePermit,
}
impl FileStream {
    pub fn delivery(&self, private: bool) -> Delivery {
        delivery(&self.filename, self.mime_type.as_deref(), private)
    }
    /// The bytes, `len` of them; the stream slot is freed when it drops.
    pub fn into_reader(self) -> impl AsyncRead + Send + Unpin + 'static {
        Reader {
            inner: self.reader,
            _permit: self.permit,
        }
    }
}
struct Reader {
    inner: Take<File>,
    _permit: OwnedSemaphorePermit,
}
impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

fn delivery(filename: &str, mime_type: Option<&str>, private: bool) -> Delivery {
    let mime = mime_type.unwrap_or("").trim().to_ascii_lowercase();
    let inline = !private && INLINE.contains(&mime.as_str());
    let filename: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Delivery {
        content_type: if inline {
            mime
        } else {
            "application/octet-stream".into()
        },
        headers: vec![
            (
                "content-disposition",
                format!(
                    "{}; filename*=UTF-8''{filename}",
                    if inline { "inline" } else { "attachment" }
                ),
            ),
            ("x-content-type-options", "nosniff".into()),
            (
                "content-security-policy",
                "default-src 'none'; sandbox".into(),
            ),
            ("cache-control", "private, no-store".into()),
        ],
    }
}
