//! Opt-in public WordPress source. Fresh source revisions gate every excerpt,
//! cache hit and preview. It never reads a WordPress editor or private post.
use crate::fault::{Fault, Result};
use crate::py::{html_escape, text};
use crate::search::db::Database;
use crate::search::store::db;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_postgres::types::Type;

pub const ORIGIN: &str = "https://cola16.app";
fn error() -> Fault {
    Fault::Other("public WordPress source unavailable")
}
pub fn valid_revision(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn valid_url(v: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(v) else {
        return false;
    };
    u.scheme() == "https"
        && u.host_str() == Some("cola16.app")
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && !v.chars().any(char::is_control)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Post {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub published: String,
    pub modified: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
}
impl Post {
    pub fn valid(&self) -> bool {
        self.id > 0
            && self.id < 10_000_000_000
            && !self.title.is_empty()
            && self.title.len() < 16384
            && valid_url(&self.url)
            && valid_revision(&self.revision)
            && !self.modified.is_empty()
            && !self.published.is_empty()
    }
}
#[derive(Deserialize)]
struct Catalog {
    schema_version: i64,
    revision: String,
    posts: Vec<Post>,
}
#[derive(Clone)]
pub struct Source {
    base: String,
    http: reqwest::Client,
}
impl Source {
    pub fn new(base: &str) -> Result<Self> {
        let u = reqwest::Url::parse(base).map_err(|_| error())?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(error());
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| error())?;
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            http,
        })
    }
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var("RAG_WORDPRESS_API") {
            Ok(v) if !v.is_empty() => Self::new(&v).map(Some),
            _ => Ok(None),
        }
    }
    async fn get(&self, path: &str, limit: usize) -> Result<Value> {
        let mut response = self
            .http
            .get(format!("{}{path}", self.base))
            .header("Host", "cola16.app")
            .header("X-Forwarded-Proto", "https")
            .header("Cache-Control", "no-cache")
            .send()
            .await
            .map_err(|_| error())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Fault::NotReadable);
        }
        if !response.status().is_success() {
            return Err(error());
        }
        let mut raw = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| error())? {
            if raw.len() + chunk.len() > limit {
                return Err(error());
            }
            raw.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&raw).map_err(|_| error())
    }
    pub async fn catalog(&self) -> Result<BTreeMap<i64, Post>> {
        let c: Catalog = serde_json::from_value(self.get("/catalog", 2 * 1024 * 1024).await?)
            .map_err(|_| error())?;
        if c.schema_version != 1 || !valid_revision(&c.revision) || c.posts.len() > 10000 {
            return Err(error());
        }
        let mut out = BTreeMap::new();
        for p in c.posts {
            if !p.valid() || out.insert(p.id, p).is_some() {
                return Err(error());
            }
        }
        Ok(out)
    }
    pub async fn post(&self, id: i64) -> Result<Post> {
        if !(1..10_000_000_000).contains(&id) {
            return Err(Fault::NotReadable);
        }
        let p: Post =
            serde_json::from_value(self.get(&format!("/posts/{id}"), 5 * 1024 * 1024).await?)
                .map_err(|_| error())?;
        if p.id != id
            || !p.valid()
            || !p
                .markdown
                .as_ref()
                .is_some_and(|md| !md.trim().is_empty() && md.len() <= 4 * 1024 * 1024)
        {
            return Err(error());
        }
        Ok(p)
    }
    pub async fn page(&self, id: i64) -> Result<String> {
        let p = self.post(id).await?;
        Ok(render_page(&p))
    }
}

pub struct Store {
    pub source: Source,
    database: Database,
    signature: String,
}
impl Store {
    pub fn new(source: Source, database: Database, signature: String) -> Self {
        Self {
            source,
            database,
            signature,
        }
    }
    pub async fn search(
        &self,
        literal: &str,
        limit: i64,
        minimum: f64,
        history: bool,
    ) -> Result<(Vec<Value>, usize)> {
        let catalog = self.source.catalog().await?;
        let client = self.database.connect().await?;
        client.batch_execute("BEGIN; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout='10s'; SET LOCAL lock_timeout='2s'").await.map_err(db)?;
        let generation = client
            .query_opt(
                "SELECT config_signature FROM notes_rag.blog_generation WHERE singleton",
                &[],
            )
            .await
            .map_err(db)?;
        if !generation.is_some_and(|r| r.get::<_, String>(0) == self.signature) {
            return Err(error());
        }
        let indexed = client
            .query("SELECT id,revision,url FROM notes_rag.blog_posts", &[])
            .await
            .map_err(db)?;
        let ids: Vec<i64> = indexed
            .iter()
            .filter_map(|r| {
                let id: i64 = r.get(0);
                catalog
                    .get(&id)
                    .filter(|p| {
                        p.revision == r.get::<_, String>(1) && p.url == r.get::<_, String>(2)
                    })
                    .map(|_| id)
            })
            .collect();
        let rows=client.query_typed("WITH ranked AS (SELECT p.*,c.ordinal,c.rag_priority,c.content,1-(c.embedding <=> $1::text::public.vector) AS similarity,row_number() OVER(PARTITION BY p.id ORDER BY (c.rag_priority='normal') DESC,c.embedding <=> $1::text::public.vector,c.ordinal) AS pos FROM notes_rag.blog_posts p JOIN notes_rag.blog_chunks c ON c.post_id=p.id WHERE p.id=ANY($2::bigint[]) AND (c.rag_priority='normal' OR $3) AND 1-(c.embedding <=> $1::text::public.vector)>=$4) SELECT * FROM ranked WHERE pos=1 ORDER BY (rag_priority='normal') DESC,similarity DESC,id LIMIT $5",&[(&literal,Type::TEXT),(&ids,Type::INT8_ARRAY),(&history,Type::BOOL),(&minimum,Type::FLOAT8),(&limit,Type::INT8)]).await.map_err(db)?;
        let mut results = Vec::new();
        for row in rows {
            let id: i64 = row.get("id");
            let ordinal: i32 = row.get("ordinal");
            let priority: String = row.get("rag_priority");
            let neighbors=client.query("SELECT ordinal,content FROM notes_rag.blog_chunks WHERE post_id=$1 AND ordinal BETWEEN $2 AND $3 AND rag_priority=$4 ORDER BY ordinal",&[&id,&(ordinal-1),&(ordinal+1),&priority]).await.map_err(db)?;
            let center: String = row.get("content");
            let mut parts = BTreeMap::from([(ordinal, center.clone())]);
            for next in [ordinal + 1, ordinal - 1] {
                if let Some(n) = neighbors.iter().find(|n| n.get::<_, i32>(0) == next) {
                    let content: String = n.get(1);
                    let mut proposed = parts.clone();
                    proposed.insert(next, content);
                    if text::len(
                        &proposed
                            .values()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n"),
                    ) <= 6000
                    {
                        parts = proposed
                    }
                }
            }
            let content = parts
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n---\n\n");
            let context = json!({"matched_ordinal":ordinal,"included_ordinals":parts.keys().copied().collect::<Vec<_>>(),"omitted_ordinals":neighbors.iter().map(|n|n.get::<_,i32>(0)).filter(|n|!parts.contains_key(n)).collect::<Vec<_>>(),"expanded":parts.len()>1,"character_limit":6000});
            results.push(json!({"page_key":row.get::<_,String>("page_key"),"ordinal":ordinal,"title":row.get::<_,String>("title"),"status":"current","rag_priority":priority,"needs_visual_review":row.get::<_,bool>("needs_visual_review"),"source_url":row.get::<_,String>("url"),"source_kind":"wordpress","post_id":id,"source_revision":row.get::<_,String>("revision"),"published":row.get::<_,String>("published"),"modified":row.get::<_,String>("modified"),"content":text::prefix(&content,6000),"truncated":text::len(&content)>6000 || neighbors.len()>parts.len(),"similarity":row.get::<_,f64>("similarity"),"context":context,"evidence_only":true}));
        }
        client.batch_execute("COMMIT").await.map_err(db)?;
        Ok((results, ids.len()))
    }
    pub async fn chunk(&self, key: &str, ordinal: i64) -> Result<Option<Value>> {
        let catalog = self.source.catalog().await?;
        let client = self.database.connect().await?;
        client.batch_execute("BEGIN; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout='10s'; SET LOCAL lock_timeout='2s'").await.map_err(db)?;
        let generation = client
            .query_opt(
                "SELECT config_signature FROM notes_rag.blog_generation WHERE singleton",
                &[],
            )
            .await
            .map_err(db)?;
        if !generation.is_some_and(|r| r.get::<_, String>(0) == self.signature) {
            return Err(error());
        }
        let rows=client.query("SELECT p.*,c.content,c.rag_priority FROM notes_rag.blog_posts p JOIN notes_rag.blog_chunks c ON c.post_id=p.id WHERE p.page_key=$1 AND c.ordinal=$2::bigint",&[&key,&ordinal]).await.map_err(db)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let id: i64 = row.get("id");
        if !catalog.get(&id).is_some_and(|p| {
            p.revision == row.get::<_, String>("revision") && p.url == row.get::<_, String>("url")
        }) {
            return Ok(None);
        }
        Ok(Some(
            json!({"page_key":key,"ordinal":ordinal,"title":row.get::<_,String>("title"),"content":text::prefix(&row.get::<_,String>("content"),131072),"source_url":row.get::<_,String>("url"),"source_kind":"wordpress","post_id":id,"source_revision":row.get::<_,String>("revision"),"status":"current","rag_priority":row.get::<_,String>("rag_priority"),"needs_visual_review":row.get::<_,bool>("needs_visual_review"),"evidence_only":true}),
        ))
    }
}

pub fn is_blog(row: &Value) -> bool {
    row["source_kind"].as_str() == Some("wordpress")
}
pub fn visible(row: &Value, catalog: &BTreeMap<i64, Post>) -> bool {
    row["post_id"]
        .as_i64()
        .and_then(|id| catalog.get(&id))
        .is_some_and(|p| {
            row["source_revision"].as_str() == Some(p.revision.as_str())
                && row["url"].as_str().or_else(|| row["source_url"].as_str())
                    == Some(p.url.as_str())
        })
}
fn render_page(post: &Post) -> String {
    let mut md = markdown_it::MarkdownIt::new();
    markdown_it::plugins::cmark::add(&mut md);
    markdown_it::plugins::html::add(&mut md);
    markdown_it::plugins::extra::tables::add(&mut md);
    markdown_it::plugins::extra::strikethrough::add(&mut md);
    let rendered = md
        .parse(post.markdown.as_deref().unwrap_or_default())
        .render();
    let base = reqwest::Url::parse(&post.url).expect("validated source URL");
    let mut clean = ammonia::Builder::default();
    clean
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(move |tag, attr, value| {
            if (tag == "a" && attr == "href") || (tag == "img" && attr == "src") {
                if tag == "a" && value.starts_with('#') {
                    return Some(Cow::Owned(value.to_string()));
                }
                let mut u = base.join(value).ok()?;
                if u.scheme() == "http" && u.host_str() == Some("cola16.app") {
                    u.set_scheme("https").ok()?;
                }
                if !matches!(u.scheme(), "https" | "http")
                    || !u.username().is_empty()
                    || u.password().is_some()
                {
                    return None;
                }
                if tag == "img"
                    && (u.scheme() != "https"
                        || u.host_str() != Some("cola16.app")
                        || !u.path().starts_with("/wp-content/uploads/")
                        || u.port().is_some())
                {
                    return None;
                }
                return Some(Cow::Owned(u.to_string()));
            }
            Some(Cow::Borrowed(value))
        });
    let safe = clean
        .clean(&rendered)
        .to_string()
        .replace("<img ", "<img loading=\"lazy\" decoding=\"async\" ");
    let replacements = BTreeMap::from([
        ("TITLE", html_escape(&post.title)),
        ("EDIT", html_escape(&post.url)),
        ("AUDIENCE_ATTR", "public".into()),
        ("AUDIENCE", "WordPress 공개 글 · 현재 Markdown".into()),
        (
            "PATH",
            html_escape(&format!(
                "작성 {} UTC · 수정 {} UTC",
                post.published, post.modified
            )),
        ),
        ("SAFE", safe),
    ]);
    // Replace tokens only in the template, never recursively in untrusted text.
    let template = include_str!("reader/page.html")
        .replace("SilverBullet에서 편집 ↗", "WordPress에서 전문 보기 ↗");
    let mut rest = template.as_str();
    let mut out = String::new();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find("}}") else { break };
        out.push_str(
            replacements
                .get(&tail[..end])
                .map(String::as_str)
                .unwrap_or(""),
        );
        rest = &tail[end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn merge(mut notes: Vec<Value>, blog: Vec<Value>, limit: usize) -> Vec<Value> {
    notes.extend(blog);
    notes.sort_by(|a, b| {
        let fallback = |r: &Value| r["rag_priority"].as_str() == Some("fallback");
        fallback(a)
            .cmp(&fallback(b))
            .then_with(|| {
                b["similarity"]
                    .as_f64()
                    .unwrap_or(0.)
                    .total_cmp(&a["similarity"].as_f64().unwrap_or(0.))
            })
            .then_with(|| a["page_key"].as_str().cmp(&b["page_key"].as_str()))
    });
    notes.truncate(limit);
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls_and_revisions_are_strict() {
        assert!(valid_url("https://cola16.app/post/"));
        for s in [
            "https://cola16.app.evil/x",
            "https://u@cola16.app/x",
            "javascript:alert(1)",
            "https://cola16.app/x?editor=1",
        ] {
            assert!(!valid_url(s));
        }
        assert!(!valid_revision(&"g".repeat(64)));
    }
    #[test]
    fn preview_sanitizes_and_keeps_own_images() {
        let p=Post{id:1,title:"<script>x</script>{{SAFE}}".into(),url:"https://cola16.app/post/".into(),published:"2026-01-01".into(),modified:"2026-01-02".into(),revision:"a".repeat(64),markdown:Some("# Hi\n\n![own](/wp-content/uploads/a.png)\n![tracker](https://evil.test/a.png)\n<script>alert(1)</script>\n[a](javascript:alert(2))".into())};
        let s = render_page(&p);
        assert!(s.contains("https://cola16.app/wp-content/uploads/a.png"));
        assert!(!s.contains("src=\"https://evil"));
        assert!(!s.contains("<script>alert(1)"));
        assert!(s.contains("WordPress에서 전문 보기"));
        assert!(!s.contains("SilverBullet에서 편집"));
        assert!(s.contains("&lt;script&gt;x&lt;/script&gt;{{SAFE}}"));
    }
    #[test]
    fn changed_or_removed_posts_hide_excerpts() {
        let p = Post {
            id: 1,
            title: "x".into(),
            url: "https://cola16.app/p/".into(),
            published: "x".into(),
            modified: "x".into(),
            revision: "a".repeat(64),
            markdown: None,
        };
        let c = BTreeMap::from([(1, p)]);
        let r = json!({"post_id":1,"source_revision":"a".repeat(64),"url":"https://cola16.app/p/"});
        assert!(visible(&r, &c));
        assert!(!visible(&r, &BTreeMap::new()));
        let mut r = r;
        r["source_revision"] = "b".repeat(64).into();
        assert!(!visible(&r, &c));
    }
}
