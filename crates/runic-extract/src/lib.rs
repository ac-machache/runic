pub(crate) mod data_island;
pub mod error;
pub mod extractor;
pub mod markdown;
pub mod metadata;
#[allow(dead_code)]
pub(crate) mod noise;
pub mod structured_data;
pub mod types;

pub use error::ExtractError;
pub use types::{CodeBlock, Content, ExtractionOptions, ExtractionResult, Image, Link, Metadata};

use scraper::Html;
use url::Url;

pub fn extract(html: &str, url: Option<&str>) -> Result<ExtractionResult, ExtractError> {
    extract_with_options(html, url, &ExtractionOptions::default())
}

pub fn extract_with_options(
    html: &str,
    url: Option<&str>,
    options: &ExtractionOptions,
) -> Result<ExtractionResult, ExtractError> {
    if html.trim().is_empty() {
        return Err(ExtractError::NoContent);
    }

    let doc = Html::parse_document(html);
    let base_url = url
        .map(|raw| Url::parse(raw).map_err(|_| ExtractError::InvalidUrl(raw.to_string())))
        .transpose()?;

    let mut meta = metadata::extract(&doc, url);
    let mut content = extractor::extract_content(&doc, base_url.as_ref(), options);
    meta.word_count = word_count_of(&content);

    if options.only_main_content && meta.word_count < 30 {
        let relaxed = ExtractionOptions {
            only_main_content: false,
            ..options.clone()
        };
        let retry = extractor::extract_content(&doc, base_url.as_ref(), &relaxed);
        if word_count_of(&retry) > meta.word_count {
            meta.word_count = word_count_of(&retry);
            content = retry;
        }
    }

    if meta.word_count < 200 && options.include_selectors.is_empty() {
        let whole_body = ExtractionOptions {
            include_selectors: vec!["body".to_string()],
            exclude_selectors: options.exclude_selectors.clone(),
            only_main_content: false,
            include_raw_html: false,
        };
        let widened = extractor::extract_content(&doc, base_url.as_ref(), &whole_body);
        let widened_count = word_count_of(&widened);
        if widened_count > meta.word_count * 2 && widened_count > 50 {
            meta.word_count = widened_count;
            content = widened;
        }
    }

    if let Some(island) = data_island::try_extract(&doc, meta.word_count, &content.markdown) {
        content.markdown.push_str("\n\n");
        content.markdown.push_str(&island);
        meta.word_count = extractor::word_count(&content.markdown);
    }

    let mut structured_data = structured_data::extract_json_ld(html);
    structured_data.extend(structured_data::extract_next_data(html));
    structured_data.extend(structured_data::extract_sveltekit(html));

    Ok(ExtractionResult {
        metadata: meta,
        content,
        structured_data,
    })
}

fn word_count_of(content: &Content) -> usize {
    extractor::word_count(&content.plain_text).max(extractor::word_count(&content.markdown))
}
