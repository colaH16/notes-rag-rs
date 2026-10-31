//! Port of `search.SearchStore`: every read happens inside one
//! `REPEATABLE READ READ ONLY` transaction with 10 s statement / 2 s lock
//! timeouts, after re-checking the schema version and the producer's
//! scope/config signatures. SQL text is kept identical to the Python source
//! (only `%s` placeholders become `$n`), including the exact cosine scan with
//! index/bitmap scans disabled. The schema is not modified.
//!
//! Learning note (Python → Rust): psycopg opened a fresh autocommit connection
//! per snapshot and used `connection.transaction()`; here a fresh
//! `tokio_postgres::Client` is opened per snapshot and `BEGIN`/`COMMIT` are
//! explicit. Dropping the client on any error aborts the transaction
//! server-side. psycopg sent `str` parameters as text; every parameter here is
//! bound with an explicit type, so `%s::public.vector` stays an explicit
//! text→vector cast exactly as before.

use super::context::{Hit, Neighbor};
use super::db::Database;
use crate::fault::{Fault, Result};
use crate::timefmt;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Row};

/// Replace psycopg `%s` placeholders with PostgreSQL `$n` placeholders.
pub fn numbered(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    let mut rest = sql;
    while let Some(index) = rest.find("%s") {
        n += 1;
        out.push_str(&rest[..index]);
        out.push_str(&format!("${n}"));
        rest = &rest[index + 2..];
    }
    out.push_str(rest);
    out
}

/// One typed bind parameter.
pub type Param<'a> = (&'a (dyn ToSql + Sync), Type);

/// Snapshot metadata (`info` in Python), serialised in Python key order.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub indexed_pages: i64,
    pub updated_at: String,
    pub folder: String,
    pub scoped_pages: i64,
    pub access_revision: String,
}

impl Info {
    pub fn to_json(&self) -> serde_json::Map<String, Value> {
        let value = json!({
            "indexed_pages": self.indexed_pages,
            "updated_at": self.updated_at,
            "folder": self.folder,
            "scoped_pages": self.scoped_pages,
            "access_revision": self.access_revision,
        });
        value.as_object().cloned().unwrap_or_default()
    }
}

/// Live-file visibility gate, called on a blocking thread.
pub type Visibility = Arc<dyn Fn(&[String]) -> Result<BTreeSet<String>> + Send + Sync>;

#[derive(Clone)]
pub struct SearchStore {
    pub database: Database,
    pub scope_signature: String,
    pub generation_signature: String,
    /// `None` means unrestricted (tests only; production always passes IDs).
    pub allowed_notebooks: Option<Vec<String>>,
    pub access_revision: String,
    pub visibility: Option<Visibility>,
}

/// An open read-only snapshot (`with store.snapshot() as (connection, info)`).
pub struct Snapshot {
    pub client: Client,
    pub info: Info,
}

impl Snapshot {
    /// Leave the read-only transaction (`COMMIT`, as psycopg's context does).
    pub async fn finish(self) -> Result<()> {
        self.client.batch_execute("COMMIT").await.map_err(db)
    }

    pub async fn rows(&self, sql: &str, params: &[Param<'_>]) -> Result<Vec<Row>> {
        self.client.query_typed(sql, params).await.map_err(db)
    }
}

/// Database errors may contain private SQL values: never surface the text.
pub fn db(_: tokio_postgres::Error) -> Fault {
    Fault::Other("database failure")
}

fn get<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, name: &str) -> Result<T> {
    row.try_get(name).map_err(db)
}

impl SearchStore {
    /// `access_clause(alias)` → (SQL fragment, whether the notebook array is bound).
    pub fn access_clause(&self, alias: &str) -> (String, bool) {
        let page = if alias.is_empty() {
            "notes_rag.pages.page_key".to_string()
        } else {
            format!("{alias}page_key")
        };
        let present = format!(
            "NOT EXISTS (SELECT 1 FROM notes_rag.missing_pages absent WHERE absent.page_key = {page})"
        );
        match &self.allowed_notebooks {
            None => (present, false),
            Some(_) => (
                format!("{present} AND {alias}notebook_id = ANY(%s::text[])"),
                true,
            ),
        }
    }

    pub fn notebooks(&self) -> Vec<String> {
        self.allowed_notebooks.clone().unwrap_or_default()
    }

    /// `snapshot(folder)`
    pub async fn snapshot(&self, folder: &str) -> Result<Snapshot> {
        crate::scope::validate_folder(folder)?;
        let client = self.database.connect().await?;
        client
            .batch_execute(
                "BEGIN; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY; \
                 SET LOCAL statement_timeout = '10s'; SET LOCAL lock_timeout = '2s'",
            )
            .await
            .map_err(db)?;
        let versions: Vec<i32> = client
            .query_typed("SELECT version FROM notes_rag.schema_version", &[])
            .await
            .map_err(db)?
            .iter()
            .map(|row| get(row, "version"))
            .collect::<Result<_>>()?;
        if versions != [1] {
            return Err(Fault::search("Unsupported index schema."));
        }
        // updated_at is returned as exact seconds/microseconds plus the session
        // time-zone offset, so Python's `datetime.isoformat()` is rebuilt
        // byte-for-byte without a date library.
        let generation = client
            .query_typed(
                "SELECT scope_signature, config_signature, \
                 extract(epoch FROM date_trunc('second', updated_at))::int8 AS updated_seconds, \
                 (extract(microseconds FROM updated_at)::int8 % 1000000) AS updated_micros, \
                 extract(timezone FROM updated_at)::int4 AS updated_offset \
                 FROM notes_rag.generation WHERE singleton",
                &[],
            )
            .await
            .map_err(db)?;
        let mismatch =
            || Fault::search("Index scope or embedding generation mismatch; search disabled.");
        let Some(generation) = generation.first() else {
            return Err(mismatch());
        };
        let scope: String = get(generation, "scope_signature")?;
        let config: String = get(generation, "config_signature")?;
        if scope != self.scope_signature || config != self.generation_signature {
            return Err(mismatch());
        }
        let seconds: i64 = get(generation, "updated_seconds")?;
        let micros: i64 = get(generation, "updated_micros")?;
        let offset: i32 = get(generation, "updated_offset")?;
        let updated_at = timefmt::isoformat(seconds * 1_000_000 + micros, offset);

        let (access, bound) = self.access_clause("");
        let notebooks = self.notebooks();
        let mut params: Vec<Param> = Vec::new();
        if bound {
            params.push((&notebooks, Type::TEXT_ARRAY));
        }
        let count_sql = numbered(&format!(
            "SELECT count(*) AS pages FROM notes_rag.pages WHERE {access}"
        ));
        let indexed_pages: i64 = get(
            &client.query_typed(&count_sql, &params).await.map_err(db)?[0],
            "pages",
        )?;
        let scoped_pages = if folder.is_empty() {
            indexed_pages
        } else {
            let prefix = format!("{folder}/");
            let mut params: Vec<Param> = vec![(&prefix, Type::TEXT), (&prefix, Type::TEXT)];
            if bound {
                params.push((&notebooks, Type::TEXT_ARRAY));
            }
            let sql = numbered(&format!(
                "SELECT count(*) AS pages FROM notes_rag.pages WHERE left(relative_path, char_length(%s)) = %s AND {access}"
            ));
            get(
                &client.query_typed(&sql, &params).await.map_err(db)?[0],
                "pages",
            )?
        };
        let info = Info {
            indexed_pages,
            updated_at,
            folder: folder.to_string(),
            scoped_pages,
            access_revision: self.access_revision.clone(),
        };
        Ok(Snapshot { client, info })
    }

    /// `check(folder)`
    pub async fn check(&self, folder: &str) -> Result<Info> {
        let snapshot = self.snapshot(folder).await?;
        let info = snapshot.info.clone();
        snapshot.finish().await?;
        Ok(info)
    }

    /// `folders()`: catalog of indexed, live-visible pages (no embedding call).
    pub async fn folders(&self) -> Result<Value> {
        let snapshot = self.snapshot("").await?;
        let (access, bound) = self.access_clause("");
        let notebooks = self.notebooks();
        let mut params: Vec<Param> = Vec::new();
        if bound {
            params.push((&notebooks, Type::TEXT_ARRAY));
        }
        let sql = numbered(&format!(
            "SELECT relative_path FROM notes_rag.pages WHERE {access} ORDER BY relative_path"
        ));
        let paths: Vec<String> = snapshot
            .rows(&sql, &params)
            .await?
            .iter()
            .map(|row| get(row, "relative_path"))
            .collect::<Result<_>>()?;
        let info = snapshot.info.clone();
        snapshot.finish().await?;
        let paths: Vec<String> = match &self.visibility {
            Some(visible) => {
                let visible = visible.clone();
                tokio::task::spawn_blocking(move || visible(&paths))
                    .await
                    .map_err(|_| Fault::Other("visibility task failed"))??
                    .into_iter()
                    .collect()
            }
            None => paths
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        let mut catalog = crate::scope::folder_catalog(&paths)?;
        if let Some(map) = catalog.as_object_mut() {
            map.insert("updated_at".into(), info.updated_at.into());
            map.insert(
                "access_revision".into(),
                self.access_revision.clone().into(),
            );
        }
        Ok(catalog)
    }

    /// `search(vector, limit, min_similarity, include_history, folder)`
    pub async fn search(
        &self,
        vector: &str,
        limit: i64,
        min_similarity: f64,
        include_history: bool,
        folder: &str,
    ) -> Result<(Vec<Hit>, serde_json::Map<String, Value>)> {
        let snapshot = self.snapshot(folder).await?;
        snapshot
            .client
            .batch_execute("SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off")
            .await
            .map_err(db)?;
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let mut rows = self
            .stage(
                &snapshot,
                vector,
                "normal",
                limit,
                &[],
                &prefix,
                min_similarity,
            )
            .await?;
        let normal_count = rows.len() as i64;
        if include_history && normal_count < limit {
            let excluded: Vec<String> = rows.iter().map(|r| r.page_key.clone()).collect();
            let history = self
                .stage(
                    &snapshot,
                    vector,
                    "fallback",
                    limit - normal_count,
                    &excluded,
                    &prefix,
                    min_similarity,
                )
                .await?;
            rows.extend(history);
        }
        let (access, bound) = self.access_clause("p.");
        let neighbor_sql = numbered(&format!(
            "
                    SELECT c.page_key, c.ordinal, c.rag_priority, c.content
                    FROM notes_rag.chunks c JOIN notes_rag.pages p USING(page_key)
                    WHERE c.page_key = %s AND c.ordinal IN (%s, %s)
                      AND c.rag_priority = %s
                      AND (%s = 'fallback' OR p.status = 'current')
                      AND {access} ORDER BY c.ordinal
                "
        ));
        let notebooks = self.notebooks();
        let mut expanded = Vec::with_capacity(rows.len());
        for row in &rows {
            let (before, after) = (row.ordinal - 1, row.ordinal + 1);
            let mut params: Vec<Param> = vec![
                (&row.page_key, Type::TEXT),
                (&before, Type::INT8),
                (&after, Type::INT8),
                (&row.rag_priority, Type::TEXT),
                (&row.rag_priority, Type::TEXT),
            ];
            if bound {
                params.push((&notebooks, Type::TEXT_ARRAY));
            }
            let neighbors = snapshot
                .rows(&neighbor_sql, &params)
                .await?
                .iter()
                .map(neighbor_from)
                .collect::<Result<Vec<_>>>()?;
            expanded.push(super::context::contextual_row(
                row,
                &neighbors,
                super::context::CONTEXT_CHARACTERS,
            ));
        }
        let mut info = snapshot.info.to_json();
        snapshot.finish().await?;
        info.insert("normal_results".into(), normal_count.into());
        info.insert(
            "fallback_results".into(),
            (rows.len() as i64 - normal_count).into(),
        );
        Ok((expanded, info))
    }

    #[allow(clippy::too_many_arguments)]
    async fn stage(
        &self,
        snapshot: &Snapshot,
        vector: &str,
        priority: &str,
        amount: i64,
        excluded: &[String],
        prefix: &str,
        min_similarity: f64,
    ) -> Result<Vec<Hit>> {
        let (access, bound) = self.access_clause("p.");
        let sql = numbered(&format!(
            "
                    WITH ranked AS (
                        SELECT p.page_key, c.ordinal, p.title, p.relative_path, p.status,
                               p.needs_visual_review, c.rag_priority, c.content,
                               1 - (c.embedding <=> %s::public.vector) AS similarity,
                               row_number() OVER (PARTITION BY p.page_key
                                   ORDER BY c.embedding <=> %s::public.vector, c.ordinal) AS position
                        FROM notes_rag.chunks c JOIN notes_rag.pages p USING (page_key)
                        WHERE c.rag_priority = %s AND (%s = 'fallback' OR p.status = 'current')
                          AND NOT (p.page_key = ANY(%s::text[]))
                          AND left(p.relative_path, char_length(%s)) = %s
                          AND {access}
                    ) SELECT * FROM ranked WHERE position = 1 AND similarity >= %s
                    ORDER BY similarity DESC, page_key, ordinal LIMIT %s
                "
        ));
        let (vector, priority, prefix, excluded) = (
            vector.to_string(),
            priority.to_string(),
            prefix.to_string(),
            excluded.to_vec(),
        );
        let notebooks = self.notebooks();
        let mut params: Vec<Param> = vec![
            (&vector, Type::TEXT),
            (&vector, Type::TEXT),
            (&priority, Type::TEXT),
            (&priority, Type::TEXT),
            (&excluded, Type::TEXT_ARRAY),
            (&prefix, Type::TEXT),
            (&prefix, Type::TEXT),
        ];
        if bound {
            params.push((&notebooks, Type::TEXT_ARRAY));
        }
        params.push((&min_similarity, Type::FLOAT8));
        params.push((&amount, Type::INT8));
        snapshot
            .rows(&sql, &params)
            .await?
            .iter()
            .map(|row| hit_from(row, true))
            .collect()
    }

    /// `chunk(page_key, ordinal)`
    pub async fn chunk(&self, page_key: &str, ordinal: i64) -> Result<(Option<Hit>, Info)> {
        let snapshot = self.snapshot("").await?;
        let (access, bound) = self.access_clause("p.");
        let sql = numbered(&format!(
            "
                SELECT p.page_key, c.ordinal, p.title, p.relative_path, p.status,
                       p.needs_visual_review, c.rag_priority, c.content
                FROM notes_rag.chunks c JOIN notes_rag.pages p USING(page_key)
                WHERE p.page_key = %s AND c.ordinal = %s AND {access}"
        ));
        let page_key = page_key.to_string();
        let notebooks = self.notebooks();
        let mut params: Vec<Param> = vec![(&page_key, Type::TEXT), (&ordinal, Type::INT8)];
        if bound {
            params.push((&notebooks, Type::TEXT_ARRAY));
        }
        let rows = snapshot.rows(&sql, &params).await?;
        let info = snapshot.info.clone();
        let hit = rows.first().map(|r| hit_from(r, false)).transpose()?;
        snapshot.finish().await?;
        Ok((hit, info))
    }
}

fn hit_from(row: &Row, with_similarity: bool) -> Result<Hit> {
    Ok(Hit {
        page_key: get(row, "page_key")?,
        ordinal: i64::from(get::<i32>(row, "ordinal")?),
        title: get(row, "title")?,
        relative_path: get(row, "relative_path")?,
        status: get(row, "status")?,
        needs_visual_review: get(row, "needs_visual_review")?,
        rag_priority: get(row, "rag_priority")?,
        content: get(row, "content")?,
        similarity: if with_similarity {
            Some(get::<f64>(row, "similarity")?)
        } else {
            None
        },
        context: None,
    })
}

fn neighbor_from(row: &Row) -> Result<Neighbor> {
    Ok(Neighbor {
        page_key: get(row, "page_key")?,
        ordinal: i64::from(get::<i32>(row, "ordinal")?),
        rag_priority: get(row, "rag_priority")?,
        content: get(row, "content")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_numbered_in_order() {
        assert_eq!(
            numbered("a = %s AND b IN (%s, %s)"),
            "a = $1 AND b IN ($2, $3)"
        );
        assert_eq!(numbered("no params"), "no params");
    }
}
