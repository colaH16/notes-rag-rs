//! Development-only differential probe (not shipped in the image).
//!
//! Reads `[{"fn": name, "args": {...}}, ...]` on stdin and prints one result
//! per case: `{"ok": value}` or `{"err": "<python exception class>"}`.
//! `tests/oracle/compare.py` feeds the same cases to the pinned Python
//! functions and diffs the outputs.

use notes_rag_rs::fault::Fault;
use notes_rag_rs::py::{float, json as pyjson, path as pypath, text, url};
use notes_rag_rs::reader::{Reader, ReaderConfig};
use notes_rag_rs::search::context::{
    Context, Hit, Neighbor, citation_url, contextual_row, result_item,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::io::Read;

fn kind(fault: &Fault) -> &'static str {
    match fault {
        Fault::NotReadable => "NotReadable",
        Fault::Value(_) => "ValueError",
        Fault::Os => "OSError",
        Fault::Search(_) => "SearchError",
        Fault::Denied { .. } => "Denied",
        Fault::Other(_) => "Exception",
    }
}

fn s<'a>(args: &'a Value, name: &str) -> &'a str {
    args[name].as_str().unwrap_or_default()
}

fn hit(value: &Value) -> Hit {
    Hit {
        page_key: s(value, "page_key").into(),
        ordinal: value["ordinal"].as_i64().unwrap_or_default(),
        title: s(value, "title").into(),
        relative_path: s(value, "relative_path").into(),
        status: s(value, "status").into(),
        needs_visual_review: value["needs_visual_review"].as_bool().unwrap_or_default(),
        rag_priority: s(value, "rag_priority").into(),
        content: s(value, "content").into(),
        similarity: value.get("similarity").and_then(Value::as_f64),
        context: value.get("context").map(|c| Context {
            matched_ordinal: c["matched_ordinal"].as_i64().unwrap_or_default(),
            included_ordinals: c["included_ordinals"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
            omitted_ordinals: c["omitted_ordinals"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
            expanded: c["expanded"].as_bool().unwrap_or_default(),
            character_limit: c["character_limit"].as_u64().unwrap_or_default() as usize,
        }),
    }
}

struct Env {
    _dir: tempfile::TempDir,
    reader: Reader,
    selection: std::path::PathBuf,
}

fn environment() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().join("corpus");
    std::fs::create_dir(&corpus).unwrap();
    let selection = dir.path().join("markdown.json");
    std::fs::write(
        &selection,
        json!({"schemaVersion": 1, "sourceArchiveRoot": "a", "silverBulletRoot": "OneNote/markdown",
               "notebooks": [{"id": "nb-public", "name": "public"}, {"id": "nb-private", "name": "private"},
                             {"id": "dup1", "name": "dup"}, {"id": "dup2", "name": "dup"}]})
        .to_string(),
    )
    .unwrap();
    let policy = dir.path().join("access.json");
    std::fs::write(
        &policy,
        json!({"schemaVersion": 1, "publicNotebookIds": ["nb-public"]}).to_string(),
    )
    .unwrap();
    let revision = notes_rag_rs::policy::revision_of(&["nb-public".to_string()]);
    let reader = Reader::new(ReaderConfig {
        root: corpus,
        selection: selection.clone(),
        policy,
        revision,
        note_prefix: "/OneNote/markdown/".into(),
        silverbullet_origin: "https://notes.cola16.app".into(),
        vision_state: None,
        native_notes: true,
    })
    .unwrap();
    Env {
        _dir: dir,
        reader,
        selection,
    }
}

fn run(env: &Env, name: &str, args: &Value) -> Result<Value, Fault> {
    Ok(match name {
        "quote" => json!(url::quote(s(args, "s"), s(args, "safe"))),
        "unquote" => json!(url::unquote(s(args, "s"))),
        "urlencode" => {
            let pairs: Vec<(&str, &str)> = args["pairs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap(), p[1].as_str().unwrap()))
                .collect();
            json!(url::urlencode(&pairs))
        }
        "urlsplit" => match url::urlsplit(s(args, "s")) {
            Some(u) => json!([u.scheme, u.netloc, u.path, u.query, u.fragment]),
            None => return Err(Fault::Value("Invalid IPv6 URL")),
        },
        "query_param" => json!(url::query_param(s(args, "raw").as_bytes(), s(args, "name"))),
        "normpath" => json!(pypath::normpath(s(args, "s"))),
        "dirname" => json!(pypath::dirname(s(args, "s"))),
        "suffix" => json!(pypath::suffix(s(args, "s"))),
        "stem" => json!(pypath::stem(s(args, "s"))),
        "float_repr" => json!(float::repr(args["x"].as_f64().unwrap())),
        "dumps_ascii" => json!(pyjson::dumps_ascii(&args["v"])),
        "dumps_unicode" => json!(pyjson::dumps_unicode(&args["v"])),
        "normalize_ws" => json!(text::normalize_whitespace(s(args, "s"))),
        "strip" => json!(text::strip(s(args, "s"))),
        "splitlines" => json!(text::splitlines_keepends(s(args, "s"))),
        "casefold" => json!(text::casefold(s(args, "s"))),
        "isidentifier" => json!(text::is_identifier(s(args, "s"))),
        "py_str" => json!(pyjson::to_str(&args["v"])),
        "validate_folder" => json!(notes_rag_rs::scope::validate_folder_value(&args["v"])?),
        "folder_catalog" => {
            let paths: Vec<String> = args["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap().to_string())
                .collect();
            notes_rag_rs::scope::folder_catalog(&paths)?
        }
        "citation_url" => json!(citation_url(
            s(args, "rel"),
            s(args, "base"),
            s(args, "root")
        )?),
        "contextual_row" => {
            let neighbors: Vec<Neighbor> = args["neighbors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| Neighbor {
                    page_key: s(n, "page_key").into(),
                    ordinal: n["ordinal"].as_i64().unwrap(),
                    rag_priority: s(n, "rag_priority").into(),
                    content: s(n, "content").into(),
                })
                .collect();
            let maximum = args["maximum"].as_u64().unwrap() as usize;
            result_item(
                &contextual_row(&hit(&args["row"]), &neighbors, maximum),
                "https://notes.example.invalid",
                "OneNote/markdown",
                6000,
            )?
        }
        "result_item" => result_item(
            &hit(&args["row"]),
            "https://notes.example.invalid",
            "OneNote/markdown",
            args["maximum"].as_u64().unwrap() as usize,
        )?,
        "public_result" => Value::Object(notes_rag_rs::web::app::public_result(
            &args["result"],
            s(args, "folder"),
            "https://notes.cola16.app/OneNote/markdown/",
            "/OneNote/markdown/",
        )?),
        "public_folders" => {
            let empty = Map::new();
            Value::Object(notes_rag_rs::web::app::public_folders(
                args["result"].as_object().unwrap_or(&empty),
            )?)
        }
        "visible_graph" => {
            let paths: BTreeSet<String> = args["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap().to_string())
                .collect();
            notes_rag_rs::graph::visible_graph(&args["graph"], &paths)?
        }
        "slug" => json!(notes_rag_rs::reader::markdown::slug(s(args, "s"))),
        "frontmatter" => {
            let (metadata, body) = notes_rag_rs::reader::frontmatter(s(args, "s"))?;
            json!([metadata, body])
        }
        "render_body" => {
            let (safe, assets) = env.reader.render_body(s(args, "path"), s(args, "body"));
            json!([safe, assets])
        }
        "normalize" => {
            let out = notes_rag_rs::native::normalize(
                s(args, "raw").as_bytes(),
                s(args, "relative"),
                &env.selection,
                true,
            )?;
            json!(String::from_utf8(out).map_err(|_| Fault::Value("utf8"))?)
        }
        "target" => json!(env.reader.target(s(args, "path"), s(args, "value"))),
        "scope_signature" => {
            let ids: BTreeSet<String> = args["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap().to_string())
                .collect();
            json!(notes_rag_rs::compat::scope_signature(&ids))
        }
        "revision" => {
            let mut ids: Vec<String> = args["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap().to_string())
                .collect();
            ids.sort();
            json!(notes_rag_rs::policy::revision_of(&ids))
        }
        "config_signature" => json!(
            notes_rag_rs::compat::ProducerManifest::embedded()
                .map_err(|_| Fault::Other("manifest"))?
                .config_signature()
        ),
        "query_text" => json!(
            notes_rag_rs::compat::ProducerManifest::embedded()
                .map_err(|_| Fault::Other("manifest"))?
                .query_text(s(args, "s"))
        ),
        "token_count" => {
            let manifest = notes_rag_rs::compat::ProducerManifest::embedded()
                .map_err(|_| Fault::Other("manifest"))?;
            let path =
                std::env::var("RAG_TOKENIZER_PATH").map_err(|_| Fault::Other("tokenizer path"))?;
            let tokenizer = notes_rag_rs::tokenizer::QwenTokenizer::load(
                std::path::Path::new(&path),
                &manifest,
            )
            .map_err(|_| Fault::Other("tokenizer"))?;
            let texts: Vec<usize> = args["texts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| tokenizer.count(t.as_str().unwrap()).unwrap())
                .collect();
            json!(texts)
        }
        "char_classes" => {
            // For each code point: [is_word, is_space, lower, casefold]
            let codes: Vec<Value> = args["codes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let ch = char::from_u32(c.as_u64().unwrap() as u32).unwrap();
                    json!([
                        text::is_word(ch),
                        text::is_space(ch),
                        ch.to_lowercase().collect::<String>(),
                        text::casefold(&ch.to_string())
                    ])
                })
                .collect();
            json!(codes)
        }
        other => panic!("unknown probe function {other}"),
    })
}

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let cases: Vec<Value> = serde_json::from_str(&input).unwrap();
    let env = environment();
    let results: Vec<Value> = cases
        .iter()
        .map(
            |case| match run(&env, case["fn"].as_str().unwrap(), &case["args"]) {
                Ok(value) => json!({"ok": value}),
                Err(fault) => json!({"err": kind(&fault)}),
            },
        )
        .collect();
    println!("{}", serde_json::to_string(&results).unwrap());
}
