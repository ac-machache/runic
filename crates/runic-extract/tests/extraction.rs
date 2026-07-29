use runic_extract::extract;

const PAGE: &str = r#"
<html lang="en">
<head>
  <title>Understanding Rust Lifetimes</title>
  <meta name="description" content="A practical guide to lifetimes.">
  <meta name="author" content="Jane Doe">
  <meta property="og:site_name" content="Rust Weekly">
</head>
<body>
  <header class="site-header">
    <nav><a href="/">Home</a> <a href="/archive">Archive</a> <a href="/about">About</a></nav>
  </header>
  <div id="cookie-consent-banner">We use cookies. <button>Accept</button></div>
  <main>
    <article class="post-content">
      <h1>Understanding Rust Lifetimes</h1>
      <p>Lifetimes tell the compiler how long a reference stays valid. See the
         <a href="/book/ch10-03">official chapter</a> for the full treatment.</p>
      <h2>Elision</h2>
      <p>Most signatures need no annotation at all, thanks to <em>elision</em>.</p>
      <ul><li>One input reference</li><li>Method with &amp;self</li></ul>
      <pre><code class="language-rust">fn longest<'a>(x: &'a str) -> &'a str { x }</code></pre>
    </article>
  </main>
  <aside class="sidebar"><h3>Sponsored</h3><p>Buy our course now!</p></aside>
  <footer class="site-footer"><p>Copyright 2026 Rust Weekly</p></footer>
  <script>window.analytics = {track: true};</script>
</body>
</html>
"#;

#[test]
fn chrome_is_stripped_and_content_survives() {
    let result = extract(PAGE, Some("https://rustweekly.example/lifetimes")).unwrap();
    let md = &result.content.markdown;

    assert!(
        md.contains("Understanding Rust Lifetimes"),
        "title lost:\n{md}"
    );
    assert!(md.contains("Elision"), "subheading lost:\n{md}");
    assert!(md.contains("elision"), "body lost:\n{md}");

    for chrome in [
        "Archive",
        "Sponsored",
        "Buy our course",
        "Copyright 2026",
        "analytics",
    ] {
        assert!(!md.contains(chrome), "chrome `{chrome}` leaked:\n{md}");
    }
}

#[test]
fn headings_and_lists_come_back_as_markdown() {
    let result = extract(PAGE, Some("https://rustweekly.example/lifetimes")).unwrap();
    let md = &result.content.markdown;

    assert!(
        md.contains("# Understanding Rust Lifetimes"),
        "no h1:\n{md}"
    );
    assert!(md.contains("## Elision"), "no h2:\n{md}");
    assert!(md.contains("One input reference"), "no list:\n{md}");
    assert!(
        md.lines()
            .any(|line| line.trim_start().starts_with(['-', '*'])),
        "list not rendered as markdown bullets:\n{md}"
    );
}

#[test]
fn links_survive_with_absolute_urls() {
    let result = extract(PAGE, Some("https://rustweekly.example/lifetimes")).unwrap();

    assert!(
        result
            .content
            .markdown
            .contains("https://rustweekly.example/book/ch10-03"),
        "relative link not resolved into the markdown:\n{}",
        result.content.markdown
    );

    let chapter = result
        .content
        .links
        .iter()
        .find(|link| link.text.contains("official chapter"))
        .expect("the in-body link should be collected");
    assert_eq!(chapter.href, "https://rustweekly.example/book/ch10-03");

    assert!(
        !result
            .content
            .links
            .iter()
            .any(|link| link.text == "Archive"),
        "nav links must not be collected: {:?}",
        result.content.links
    );
}

#[test]
fn code_blocks_keep_their_language() {
    let result = extract(PAGE, Some("https://rustweekly.example/lifetimes")).unwrap();
    let block = result
        .content
        .code_blocks
        .first()
        .expect("the rust snippet should be collected");

    assert_eq!(block.language.as_deref(), Some("rust"));
    assert!(block.code.contains("fn longest"));
}

#[test]
fn metadata_comes_off_the_head() {
    let result = extract(PAGE, Some("https://rustweekly.example/lifetimes")).unwrap();
    let meta = &result.metadata;

    assert_eq!(meta.title.as_deref(), Some("Understanding Rust Lifetimes"));
    assert_eq!(meta.author.as_deref(), Some("Jane Doe"));
    assert_eq!(meta.site_name.as_deref(), Some("Rust Weekly"));
    assert_eq!(meta.language.as_deref(), Some("en"));
    assert!(
        meta.word_count > 20,
        "word count too low: {}",
        meta.word_count
    );
}

#[test]
fn a_page_wrapping_form_is_not_treated_as_chrome() {
    let aspnet = format!(
        "<html><body><form id=\"aspnetForm\"><h1>Quarterly Report</h1>{}</form></body></html>",
        "<p>Revenue grew steadily across every region this quarter.</p>".repeat(12)
    );
    let result = extract(&aspnet, None).unwrap();

    assert!(
        result.content.markdown.contains("Quarterly Report"),
        "ASP.NET page-wrapping form was stripped:\n{}",
        result.content.markdown
    );
    assert!(result.content.markdown.contains("Revenue grew steadily"));
}

#[test]
fn a_small_login_form_is_chrome() {
    let html = r#"<html><body>
        <article><h1>Real Article</h1><p>The body of the piece goes here and continues on.</p></article>
        <form class="login-form"><input name="user"><input name="pass"><button>Sign in</button></form>
    </body></html>"#;
    let result = extract(html, None).unwrap();

    assert!(result.content.markdown.contains("Real Article"));
    assert!(
        !result.content.markdown.contains("Sign in"),
        "login form leaked:\n{}",
        result.content.markdown
    );
}

#[test]
fn empty_input_is_an_error() {
    assert!(extract("", None).is_err());
    assert!(extract("   ", None).is_err());
}
