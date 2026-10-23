//! Bounded PDF extraction in a disposable worker process.

use std::{
    io::{Read, Write},
    process::Stdio,
    time::Duration,
};

use pdf_oxide::{PdfDocument, converters::ConversionOptions};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    time::{Instant, timeout_at},
};

const MAX_INPUT: usize = 50 * 1024 * 1024;
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_PAGES: usize = 200;
static EXTRACTIONS: Semaphore = Semaphore::const_new(4);

#[derive(Debug, Serialize, Deserialize)]
pub struct PdfText {
    pub markdown: String,
    pub pages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub encrypted: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("PDF extraction deadline exceeded")]
    Timeout,
    #[error("PDF extraction failed or exceeded limits")]
    Failed,
    #[error("encrypted PDF is unsupported")]
    Encrypted,
    #[error("PDF requires OCR: no usable text layer")]
    NoTextLayer,
}

impl PdfText {
    fn validate(&self) -> Result<(), DocumentError> {
        if self.encrypted {
            return Err(DocumentError::Encrypted);
        }
        if self.pages == 0 || self.pages > MAX_PAGES || self.markdown.len() > MAX_OUTPUT {
            return Err(DocumentError::Failed);
        }
        if self.markdown.chars().filter(|c| !c.is_whitespace()).count() < 20 * self.pages {
            return Err(DocumentError::NoTextLayer);
        }
        Ok(())
    }
}

/// Queueing, input transfer, extraction, and worker exit share one deadline.
pub async fn pdf_to_markdown(bytes: &[u8], deadline: Instant) -> Result<PdfText, DocumentError> {
    if bytes.len() > MAX_INPUT {
        return Err(DocumentError::Failed);
    }
    let deadline = deadline.min(Instant::now() + Duration::from_secs(30));
    timeout_at(deadline, async {
        let _permit = EXTRACTIONS
            .acquire()
            .await
            .map_err(|_| DocumentError::Failed)?;
        let mut child = Command::new(std::env::current_exe().map_err(|_| DocumentError::Failed)?)
            .env_clear()
            .arg("extract-pdf")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| DocumentError::Failed)?;
        let mut stdin = child.stdin.take().ok_or(DocumentError::Failed)?;
        let stdout = child.stdout.take().ok_or(DocumentError::Failed)?;
        let transfer = async {
            stdin
                .write_all(bytes)
                .await
                .map_err(|_| DocumentError::Failed)?;
            drop(stdin);
            Ok::<_, DocumentError>(())
        };
        let receive = async {
            let mut output = Vec::new();
            stdout
                .take((MAX_OUTPUT + 1) as u64)
                .read_to_end(&mut output)
                .await
                .map_err(|_| DocumentError::Failed)?;
            if output.len() > MAX_OUTPUT {
                return Err(DocumentError::Failed);
            }
            Ok(output)
        };
        let (_, output) = tokio::try_join!(transfer, receive)?;
        let status = child.wait().await.map_err(|_| DocumentError::Failed)?;
        let pdf: PdfText = serde_json::from_slice(&output).map_err(|_| DocumentError::Failed)?;
        pdf.validate()?;
        if !status.success() {
            return Err(DocumentError::Failed);
        }
        Ok(pdf)
    })
    .await
    .unwrap_or(Err(DocumentError::Timeout))
}

/// Worker entry point: never prints parser diagnostics or document contents to stderr.
pub fn extract_pdf_stdio() -> Result<(), DocumentError> {
    #[cfg(unix)]
    {
        rlimit::Resource::AS
            .set(1024 * 1024 * 1024, 1024 * 1024 * 1024)
            .map_err(|_| DocumentError::Failed)?;
        rlimit::Resource::CPU
            .set(30, 30)
            .map_err(|_| DocumentError::Failed)?;
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| DocumentError::Failed)?;
    if bytes.len() > MAX_INPUT {
        return Err(DocumentError::Failed);
    }
    std::panic::set_hook(Box::new(|_| {}));
    let pdf = std::panic::catch_unwind(|| extract(bytes)).map_err(|_| DocumentError::Failed)??;
    let output = serde_json::to_vec(&pdf).map_err(|_| DocumentError::Failed)?;
    if output.len() > MAX_OUTPUT {
        return Err(DocumentError::Failed);
    }
    std::io::stdout()
        .lock()
        .write_all(&output)
        .map_err(|_| DocumentError::Failed)?;
    pdf.validate()
}

fn extract(bytes: Vec<u8>) -> Result<PdfText, DocumentError> {
    let doc = PdfDocument::from_bytes(bytes).map_err(|_| DocumentError::Failed)?;
    // Report encryption over the bounded protocol, even for an empty-password PDF.
    if doc.is_encrypted() {
        return Ok(PdfText {
            markdown: String::new(),
            pages: 0,
            title: None,
            encrypted: true,
        });
    }
    let pages = doc.page_count().map_err(|_| DocumentError::Failed)?;
    if pages == 0 || pages > MAX_PAGES {
        return Err(DocumentError::Failed);
    }
    let mut markdown = String::new();
    let options = ConversionOptions {
        include_images: false,
        ..ConversionOptions::default()
    };
    for page in 0..pages {
        let text = doc
            .to_markdown(page, &options)
            .map_err(|_| DocumentError::Failed)?;
        if markdown.len() + text.len() + 2 > MAX_OUTPUT {
            return Err(DocumentError::Failed);
        }
        if page > 0 {
            markdown.push_str("\n\n");
        }
        markdown.push_str(&text);
    }
    let title = title(&doc);
    Ok(PdfText {
        markdown,
        pages,
        title,
        encrypted: false,
    })
}

fn title(doc: &PdfDocument) -> Option<String> {
    let info = doc.trailer().as_dict()?.get("Info")?;
    let info = doc.resolve_object(info).ok()?;
    let title = info.as_dict()?.get("Title")?;
    let title = doc.resolve_object(title).ok()?;
    let title = pdf_oxide::optional_content::decode_pdf_text_string(title.as_string()?);
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}
