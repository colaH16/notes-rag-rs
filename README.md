# notes-rag-rs

notes-rag의 **공개 검색 웹 게이트웨이**와 **내부 읽기 전용 MCP 검색 서비스**를 Rust로 옮긴 구현입니다.
바이너리 하나가 환경 변수 `RAG_ROLE=web|mcp`로 두 역할 중 하나를 실행합니다.

> **현재 상태: 로컬 검증 완료 · 코드/이미지 게시 진행 중 · 운영 전환 전**
>
> - 게시 대상: [colaH16/notes-rag-rs](https://github.com/colaH16/notes-rag-rs), 이미지 `ghcr.io/colah16/notes-rag-rs`.
> - 아래 검증 결과는 합성 데이터와 모의 서비스에서 얻었습니다. CI·컨테이너·실제 운영 검증 결과는 확인 후 갱신합니다.
> - Python 인덱서·OCR·잡은 기존 구현을 계속 사용합니다. 성능·메모리 비교는 별도로 반복 측정 중입니다.

## 1. 이전 범위

| 구분 | 이 저장소(Rust) | 원본 Python에 그대로 남음 |
|---|---|---|
| 공개 검색 UI 서버 (`public_search.py`) | `/`, `/api/search`, `/api/folders`, `/api/session`, 정적 파일 | – |
| 본인 로그인 (`web_auth.py`) | OIDC 코드+PKCE, 소유자 세션, 로그아웃 | – |
| 원문 보기 (`document_reader.py`, `vision_overlay.py`) | `/read`, `/read-asset` (fd 고정, 정제) | – |
| 의미 지도 어댑터 (`semantic_graph.py`) | `/api/graph`, MCP 내부 `/graph`, 1시간 캐시, 가시성 필터 | 그래프 SQL/알고리즘은 **변경 없음** |
| MCP 서버 (`mcp_server.py`, `search.py`) | `search_notes`, `list_note_folders`, `get_note_chunk` | – |
| 질의 임베딩 (`embedding_client.py`의 질의 경로) | 1회 시도, 30초, 쿨다운 공유 | – |
| 색인기, 청커, 세그멘터, OCR, 백업, 알림, 모니터, 스냅샷, 잡 | **이전하지 않음** | 원본 이미지/CronJob 그대로 |
| 프론트엔드 JS/CSS/HTML, 브라우저 Barnes-Hut 워커 | 바이트 단위로 동일하게 내장 (`static/`, SHA-256 검사) | 원본이 정본 |
| PostgreSQL/pgvector 스키마, 내용, PVC | **변경하지 않음** (마이그레이션 없음) | 원본 DDL이 정본 |

**이미지 교체 범위:** 이 이미지는 기존 **web Deployment와 MCP Deployment만** 대체할 수 있습니다.
원본 이미지에는 indexer/ocr/monitor/snapshot/dispatch 명령도 들어 있으므로, 그 잡들은 계속 원본 Python 이미지를 써야 합니다.

## 2. 원본 출처와 저작권

- 원본: 같은 소유자(colaH16)의 Python 저장소 `notes-rag`, 커밋 `754a97398ac1d6ef313605f84eb7249db8eb556e`.
- 원본 저장소에는 **라이선스 파일이 없습니다.** 따라서 이 포트에도 라이선스를 새로 붙이지 않았으며(`Cargo.toml`에 license 없음), 권리는 원저작자에게 있습니다.
- `static/`의 프론트엔드 파일 12개와 `src/reader/page.html`(원문 보기 HTML 틀)은 원본에서 그대로 가져왔습니다. `compat/static-assets.sha256`과 테스트가 원본과 바이트 단위로 같은지 확인합니다.
- `tests/fixtures/python-schema/*.sql`은 테스트용 임시 DB를 만들 때만 쓰는 원본 DDL 사본입니다(출처는 `SOURCE.txt`).
- 의존 크레이트는 각자의 라이선스를 따릅니다(대부분 MIT/Apache-2.0). 특히 HTML 정제기 `ammonia`(MIT/Apache-2.0)는 원본이 쓰던 Python `nh3`의 내부 엔진과 같은 라이브러리입니다.

## 3. 구조

```
                 브라우저 (원본 JS/CSS 그대로)
                        │ HTTPS (ingress)
              ┌─────────▼──────────┐   SQLite (/state/search.db, 기존 스키마)
              │ RAG_ROLE=web       │── 예산·트래픽·캐시·소유자 세션
              │  axum + hyper      │── 코퍼스(읽기 전용, fd 고정)
              │  rmcp 클라이언트    │── OIDC 공급자 (로그인할 때만)
              └─────────┬──────────┘
                        │ Streamable HTTP MCP (Bearer 공개/소유자)
              ┌─────────▼──────────┐
              │ RAG_ROLE=mcp       │── PostgreSQL+pgvector (REPEATABLE READ READ ONLY)
              │  rmcp 서버(공식)    │── 임베딩 엔드포인트 (질의 1개, ZDR)
              │  + 내부 /graph     │── 코퍼스(가시성 확인)
              └────────────────────┘
```

| 경로 | 역할 |
|---|---|
| `src/main.rs` | 인자 없는 진입점, `RAG_ROLE` 분기 |
| `src/config.rs` | 환경 변수 해석 (기존 이름 유지, 하드코딩 값 → 기본값이 같은 변수) |
| `src/compat.rs`, `compat/producer-manifest.json` | Python 색인기와의 서명 호환 계약 |
| `src/py/` | 원본이 의존하는 CPython 동작(코드포인트 길이, `json.dumps`, `urllib.parse`, `posixpath`, `repr(float)`) |
| `src/search/` | `SearchStore`(SQL), `SearchService`, 문맥 확장, 인용 URL, DB 연결(`conninfo.rs`, `db.rs`) |
| `src/graph.rs` | 그래프 조립·가시성 필터·1시간 캐시 (SQL은 원본 그대로) |
| `src/reader/` | fd 고정 파일 열기, Markdown 렌더링+정제, OCR 보충, 원문 HTML |
| `src/mcp/` | 게이트(Bearer/Host/Origin/본문 제한), rmcp 핸들러, 인자 검증 |
| `src/web/` | 라우터, SQLite 상태, OIDC, MCP 게이트웨이, HTTP 헬퍼 |
| `src/serve.rs` | HTTP/1.1 서버, keep-alive 5초, 동시성 32, SIGTERM 정리 |

## 4. Python → Rust 학습 대응표

| Python (원본) | Rust (이 저장소) | 배울 점 |
|---|---|---|
| 예외 클래스로 상태 코드 결정 (`except (NotReadable, OSError, ValueError)`) | `fault::Fault` 열거형 + `match` | 예외 계층 대신 값으로 오류 종류를 전달 |
| `asyncio.Lock`, `BoundedSemaphore(2)` | `tokio::sync::Mutex::try_lock`, `Semaphore::try_acquire_owned` | 비차단 획득으로 "바쁨" 응답 |
| `asyncio.to_thread(reader.visible_paths, …)` | `tokio::task::spawn_blocking` | 블로킹 파일 I/O를 런타임 밖으로 |
| `os.open(part, O_NOFOLLOW, dir_fd=d)` + `os.close` | `rustix::fs::openat` → `OwnedFd`(drop 시 닫힘) | RAII로 fd 누수 방지 |
| `psycopg.connect()` + `connection.transaction()` | `tokio_postgres` + 명시적 `BEGIN … COMMIT`, 타입 지정 바인딩 | 드라이버가 세션 설정을 바꾸지 않게 선택 |
| `sqlite3` `with db:` / `BEGIN IMMEDIATE` | `rusqlite` + 같은 SQL 문 | 같은 DB 파일을 이어서 사용 |
| `MCPServer` 데코레이터가 타입힌트로 스키마 생성 | `rmcp::ServerHandler` + 원본에서 캡처한 계약 JSON | 프로토콜은 SDK, 스키마는 계약 파일로 고정 |
| pydantic lax/strict 검증 | `mcp/args.rs`의 명시적 규칙 | 암묵적 형변환을 코드로 드러냄 |
| Authlib (OIDC) + Starlette SessionMiddleware | `openidconnect` + 자체 HMAC 흐름 쿠키 | 검증 단계를 한 함수에서 확인 |
| markdown-it-py + nh3 | `markdown-it`(Rust 포트) + `ammonia`(nh3와 같은 엔진) | 정제기를 같은 라이브러리로 맞춤 |
| `len(s)`, `s[:6000]` | `py::text::len`, `py::text::prefix` | UTF-8 바이트가 아닌 코드포인트 기준 |
| `json.dumps(sort_keys=True)` 해시 | `py::json::dumps` | 서명 호환을 위해 공백·이스케이프까지 재현 |

## 5. 환경 변수

**기존 이름은 모두 같은 의미로 유지**했습니다. 원본 코드에 하드코딩되어 있던 값은 새 변수로 꺼냈고, **기본값은 원본 상수와 같습니다.** 따라서 기존 Deployment의 환경을 그대로 두면 동작도 같습니다.
배포 인자나 entrypoint 덮어쓰기는 쓰지 않습니다. 이미지 ENTRYPOINT는 바이너리만 실행하며 인자를 받지 않습니다(인자가 있으면 거부).

### 공통

| 변수 | 기본값 | 설명 |
|---|---|---|
| `RAG_ROLE` (신규) | 없음(필수) | `web` 또는 `mcp` |
| `RAG_NATIVE_NOTES` | 없음 | 반드시 `1` (원본과 같이, 아니면 시작 거부) |
| `RAG_LISTEN_ADDR` (신규) | web `0.0.0.0:8080`, mcp `0.0.0.0:8000` | 원본의 고정 host/port |
| `RAG_SHUTDOWN_GRACE_SECONDS` (신규) | `20` | SIGTERM 후 진행 중 요청을 기다리는 최대 시간 |
| `RAG_NOTE_PATH_PREFIX` (신규) | `/OneNote/markdown/` | 원본 `NOTE_PREFIX` |
| `RAG_VISION_STATE` | 없음 | 게시된 OCR 보충 디렉터리(원문 보기) |

### `RAG_ROLE=mcp`

| 변수 | 기본값 | 설명 |
|---|---|---|
| `RAG_MCP_TOKEN` | 필수 | 공개 Bearer 토큰(40자 이상, 출력 가능한 ASCII) |
| `RAG_OWNER_MCP_TOKEN` | 없음 | 소유자 Bearer 토큰(공개 토큰과 달라야 함) |
| `RAG_MCP_HOSTS` | `127.0.0.1:*,localhost:*` | 허용 Host(정확히 일치 또는 `host:*`), 쉼표 분리 |
| `RAG_ALLOWLIST` | 필수 | `markdown.json` 선택 파일 |
| `RAG_ACCESS_POLICY` | 필수 | 공개 노트북 정책 |
| `RAG_CORPUS_ROOT` | 필수 | 코퍼스(읽기 전용) |
| `RAG_DATABASE_URL` + `PG*` | URL 없음 | libpq와 같은 우선순위: `PGHOST`·`PGPORT`·`PGDATABASE`·`PGUSER`·`PGPASSWORD`·`PGSSLMODE`·`PGSSLROOTCERT` 등을 기본값으로 쓰고, URL/`key=value`에 있는 키가 덮어씁니다. 지원하지 못하는 키(서비스 파일, 비밀번호 파일, CRL, `PGTZ` 등)는 **거부**합니다. |
| `RAG_LITELLM_BASE_URL`, `RAG_LITELLM_API_KEY` | 필수 | 임베딩 엔드포인트 |
| `RAG_ALLOW_INTERNAL_HTTP` | 없음 | `true`면 http 허용 |
| `RAG_SILVERBULLET_URL` | 필수 | 인용 URL 기준 |
| `RAG_TOKENIZER_PATH` | `/app/qwen-tokenizer.json` | 고정 토크나이저(SHA-256 확인) |
| `RAG_ALERT_DIR` | 없음 | 있으면 임베딩 쿨다운을 Python 잡과 같은 파일 형식으로 공유 |

### `RAG_ROLE=web`

| 변수 | 기본값 | 설명 |
|---|---|---|
| `SEARCH_STATE_DB` | `/state/search.db` | 기존 SQLite 파일을 그대로 사용 |
| `SEARCH_DAILY_LIMIT`, `SEARCH_HOURLY_LIMIT` | `300`, `60` | 서버 전체 유료 검색 예산 |
| `RAG_MCP_TOKEN`, `RAG_OWNER_MCP_TOKEN` | 필수 | MCP 호출용(브라우저로 나가지 않음) |
| `RAG_OIDC_CLIENT_ID`, `RAG_OIDC_CLIENT_SECRET`, `RAG_SESSION_KEY`, `RAG_OWNER_SUBJECT` | 필수 | 본인 로그인 |
| `RAG_CORPUS_ROOT`, `RAG_PUBLIC_ACCESS_REVISION` | 필수 | 원문 보기와 정책 리비전 |
| `RAG_MCP_URL` (신규) | 원본에 하드코딩된 내부 MCP 주소 | `/mcp`로 끝나야 함 |
| `RAG_PUBLIC_ORIGIN` (신규) | 원본 `OwnerAuth.ORIGIN` | https, 끝 슬래시 없음 |
| `RAG_OIDC_ISSUER` (신규) | 원본 `OwnerAuth.ISSUER` | discovery 기준 |
| `RAG_SILVERBULLET_URL` | 원본 하드코딩 값 | web에서는 선택(기본값 = 원본 상수) |
| `RAG_WEB_SELECTION_PATH` (신규) | `/selection/markdown.json` | 원본 하드코딩 경로 |
| `RAG_WEB_POLICY_PATH` (신규) | `/policy/access.json` | 원본 하드코딩 경로 |

web의 선택/정책 경로는 MCP의 `RAG_ALLOWLIST`/`RAG_ACCESS_POLICY`와 **이름을 일부러 다르게** 했습니다. 기존 web Deployment에 다른 용도로 그 이름이 들어 있어도 의미가 바뀌지 않게 하려는 것입니다.

## 6. Python 색인기와의 공존 (서명 호환)

Python 색인기는 `notes_rag.generation`에 두 서명을 남깁니다.

- `scope_signature = sha256(json.dumps(sorted(선택 ID), ensure_ascii=False))`
- `config_signature = sha256(json.dumps({model, dimensions, tokenizer, tokenizer_version, 650/800/100, code_signature}, sort_keys=True))`

여기서 `code_signature`는 **Python 소스** `segmenter.py`·`chunker.py`·`embedding_config.py`의 바이트 해시이고, `tokenizer_version`에는 **Python** `tokenizers` 휠 버전이 들어갑니다. Rust 서버가 자기 소스를 해시하면 서명이 어긋나 검색이 막히거나, 잘못 고치면 전체 재색인을 부르게 됩니다.

그래서 이 저장소는 **명시적인 생산자 매니페스트**(`compat/producer-manifest.json`)를 가집니다.

- `scripts/producer_manifest.py`가 고정된 Python 커밋에서 세 파일의 해시, 모델·차원·토크나이저 고정값, 휠 버전을 읽어 만듭니다(임포트·DB·네트워크 없음).
- `expected_config_signature`는 **그 커밋의 `index_plan.config_signature()`가 직접 계산한 값**(`f9ec8b90…eba68ec8`)입니다. Rust는 시작할 때와 테스트에서 매니페스트로 같은 값을 다시 계산해 맞는지 확인합니다.
- 매 검색마다 스키마 버전과 두 서명을 같은 읽기 전용 스냅샷 안에서 확인하고, 다르면 **실패로 닫습니다**(검색 중단, 재색인 요청 없음).
- E2E 시험에서 원본 Python 색인기가 실제로 쓴 DB 서명이 매니페스트 값과 같음을 확인했습니다.

**Python 생산자(색인기)를 바꿀 때 절차**

1. 새 Python 커밋에서 `python3 -I scripts/producer_manifest.py <python-checkout> <commit> > compat/producer-manifest.json`를 실행하고, 그 커밋의 `index_plan.config_signature(...)` 값으로 `expected_config_signature`를 갱신합니다.
2. 세 파일(`segmenter.py`, `chunker.py`, `embedding_config.py`)이나 `tokenizers` 버전이 바뀌면 서명이 바뀝니다. 이 경우 Python은 원래 정책대로 새 세대를 써야 하고, Rust 이미지는 **새 매니페스트로 다시 빌드해 같은 시점에** 배포해야 합니다. 그 전까지 Rust MCP는 의도적으로 실패로 닫힙니다.
3. 쿼리 지시문(`QUERY_INSTRUCTION`)이나 모델 별칭이 바뀌면 매니페스트와 함께 갱신합니다.

DB 스키마는 바꾸지 않습니다. Rust는 DDL을 실행하지 않으며 마이그레이션이 없습니다.

## 7. 상태·세션·프로토콜 호환

- **SQLite 상태:** 테이블, 인덱스, SQL 문, `BEGIN IMMEDIATE` 사용이 원본과 같습니다. 전환해도 일·시간 예산과 트래픽 기록이 **초기화되지 않습니다.** 정책 리비전 변경은 캐시만 지우고 예산은 유지합니다. 캐시 본문도 원본과 같은 `json.dumps` 형식으로 저장해 롤백 시 Python이 읽을 수 있습니다.
- **소유자 세션:** 무작위 토큰의 SHA-256만 `owner_sessions`에 있으므로 Python이 발급한 세션은 Rust에서도, 그 반대도 유효합니다.
- **로그인 흐름 쿠키(`__Host-notes-oidc`, 5분):** 원본은 Starlette/itsdangerous 서명 세션이었습니다. 다른 언어의 서명 형식을 흉내 내지 않고 자체 HMAC-SHA256 형식을 씁니다. 따라서 **전환 순간에 진행 중이던 로그인은 실패하며 다시 시도해야 합니다.** 이미 로그인한 세션에는 영향이 없습니다.
- **MCP 프로토콜:** 공식 Rust SDK(rmcp 3.8.0)로 2025-11-25 이전 핸드셰이크 방식과 2026-07-28 무상태 방식을 모두 처리합니다(원본 Python SDK 2.2.0과 같음). 무상태, JSON 응답, 본문 32 KiB 제한을 유지합니다.
- **MCP 도구 계약:** 이름, 설명, 입력/출력 스키마, 주석, 서버 정보, 지시문, capabilities를 원본 서버에서 캡처해 `compat/mcp-contract.json`으로 고정했습니다. 운영 이미지의 Python은 **3.11.16**이라 docstring 들여쓰기가 그대로 남으므로, 설명은 그 의미(원 문자열 그대로)로 맞췄습니다.
- **DB 세션:** `tokio-postgres`는 psycopg처럼 `TimeZone` 등을 바꾸지 않아 `updated_at` 문자열이 같습니다(sqlx는 `TimeZone=UTC`를 강제하므로 쓰지 않았습니다).

## 8. 보안

- 공개/소유자 분리: 소유자는 고정 issuer+subject의 서버 측 세션뿐입니다. 메일, 사용자 이름, 헤더, 클라이언트 플래그는 믿지 않습니다. 느린 작업 뒤에 세션을 다시 확인해 로그아웃된 사용자에게 소유자 결과를 주지 않습니다(409).
- OIDC: state(상수 시간 비교), nonce, PKCE S256, 서명(비대칭 알고리즘만, 클라이언트 비밀을 HMAC 키로 쓰지 않음), issuer, audience, 만료(30초 여유), `at_hash`(있을 때), `nonce_supported=false` 거부, 흐름 1회 사용, 키 교체 시 메타데이터 1회 재조회.
- CSRF/Origin: 소유자 검색과 로그아웃은 정확한 Origin을 요구합니다. `Sec-Fetch-Site: cross-site`를 거부합니다. 쿠키는 `__Host-`, Secure, HttpOnly, SameSite=Lax입니다.
- 응답 헤더: 모든 응답에 `Cache-Control: no-store`, `nosniff`, `no-referrer`, `noindex`, 원본과 같은 CSP.
- MCP: Bearer 정확히 1개(상수 시간 비교), Host 허용 목록(421), 브라우저 Origin 거부(403), POST Content-Type(400), 32 KiB(413).
- 파일: 루트부터 모든 경로 요소를 `O_NOFOLLOW`로 `openat`하고, 마지막 fd를 `fstat`한 뒤 **그 fd를 그대로** 읽거나 스트리밍합니다. 경로를 다시 열지 않으므로 심볼릭 링크 교체나 이름 바꾸기(TOCTOU)로 다른 파일을 내줄 수 없습니다. 요청마다 선택/정책 파일을 다시 읽고 리비전이 다르면 실패로 닫습니다.
- Markdown: 원본과 같은 태그·속성 허용 목록, 같은 링크/첨부 재작성, 같은 정제 엔진. 외부 이미지는 넣지 않고, 다른 노트북의 첨부는 허용하지 않습니다.
- 비용: 업스트림 호출 전에 예산을 예약하고 실패도 차감합니다. 동시에 1건만 업스트림으로 보냅니다(429, Retry-After 3). MCP는 청중별 동시 2건이며, 호출자가 끊겨도 작업이 끝날 때까지 슬롯을 유지합니다.
- 로그: 고정된 시작 문구와, 실패 시 비밀이 없는 사유만 출력합니다. 요청/본문/토큰/서명 키를 기록하지 않습니다.

## 9. 의도적인 차이와 원본의 알려진 문제

**원본의 실제 문제(보고 대상):** `embedding_client.py`는 장기 실행 MCP 프로세스에서 첫 질의 때 `deadline = monotonic() + 900`을 정하고 **다시 초기화하지 않습니다.** 그래서 첫 질의 후 15분이 지나면 이후 모든 질의 임베딩이 HTTP 호출 없이 `retry_deadline`으로 실패합니다(로컬에서 재현). Rust는 900초 예산을 호출마다 새로 시작합니다. 운영 중인 Python MCP도 이 영향을 받을 수 있으니 따로 확인이 필요합니다.

그 밖의 차이(모두 더 엄격하거나 표현만 다름):

| 항목 | 원본 | Rust |
|---|---|---|
| 도구 인자 검증 메시지 | pydantic 문장 + 입력값 반복 | 같은 형식, **입력값은 반복하지 않음** |
| front matter의 `NaN`/`Infinity` | 허용 | 읽을 수 없음(404) |
| `schemaVersion: true`/`1.0` | `== 1`로 통과 | 거부 |
| 20단계 넘게 중첩된 `[` 링크 라벨 | 링크 아님 | 링크로 해석(정제는 동일 적용) |
| `/read-asset` Range | Starlette 다중 범위 | 단일 범위만(다중이면 전체 전송), ETag 없음 |
| 경로 끝 `/` | 307 리다이렉트 | 404 |
| UTF-16/32 JSON 요청 본문 | 자동 감지 | 400 (UTF-8과 BOM만) |
| uvicorn 동시성 32 | 유휴 keep-alive 연결도 셈 | 진행 중 요청만 셈 |
| 405 `Allow` 순서 | 집합 순서(비결정) | `GET, HEAD` 고정 |
| DB 연결 | libpq 전체 | 지원하지 않는 키는 거부(조용히 무시하지 않음), `~/.postgresql/root.crt` 자동 탐색 없음 |
| `GET /mcp` (SSE 수락) | 무상태인데도 메시지가 오지 않는 SSE 스트림을 열어 둠(200) | 405 (명세상 허용, 공식 SDK 클라이언트 정상 동작 확인) |
| 잘못된 JSON-RPC 본문(배치 등) | 400, -32602, pydantic 오류 문장 | 400, -32602, 고정 문장 |
| 쿨다운 공유(`RAG_ALERT_DIR`) | 파일 잠금 + 원자적 교체 | 같은 파일 형식·잠금으로 구현, Python 잡과의 동시 실행은 시험하지 않음 |

## 10. 시험 방법과 결과

아래 표는 로컬 검증 결과이며, 재현용 Python 비교 도구는 `tests/oracle/`에 있습니다.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                                   # 단위 + 통합(웹/인증/MCP)
RAG_TEST_DATABASE_URL=postgresql://postgres@127.0.0.1:PORT/postgres cargo test --test db   # 격리된 로컬 DB만
# Python 오라클(고정 커밋 사본 + 고정 requirements.lock venv)
RAG_TOKENIZER_PATH=… python -I tests/oracle/compare.py <python-src> target/debug/examples/oracle_probe
python -I tests/oracle/e2e.py --source <python-src> --binary target/debug/notes-rag-rs --pg … --tokenizer …
python -I tests/oracle/dump_mcp_contract.py <python-src>      # 계약 재캡처
```

### 검증 결과 (2026-10-31, 로컬, 합성 데이터)

| 항목 | 결과 |
|---|---|
| `cargo fmt --check`, `cargo clippy --all-targets -D warnings` | 통과 |
| `cargo test` | **129개 통과**: 단위 71, 인증(OIDC 목 공급자·RS256 서명) 12, PostgreSQL 18.6 + pgvector 0.8.1 통합 7, MCP 11, 웹·원문·그래프 28 |
| 원본 Python 시험(고정 커밋 사본, 오라클 venv, 같은 격리 DB) | 428개 통과 — 오라클 환경 자체가 원본과 같게 동작함을 확인 |
| 함수 단위 차이 시험 `compare.py` | **398개 사례 중 불일치 0**, 문서화한 의도적 차이 2 (front matter `NaN`, 20단계 넘는 `[` 중첩). Markdown/XSS 본문 54종(일부는 두 경로), Unicode `\w`/`isspace`/소문자/casefold 12,410개 코드포인트(CPython 3.13 기준), 토크나이저 개수, 인용 URL, 문맥 확장, 서명 등 |
| E2E `e2e.py` | **101개 점검 중 100개 동일**, 남은 1건은 문서화한 `GET /mcp` SSE 차이. Python 색인기가 합성 코퍼스 8쪽/17청크를 실제로 커밋한 DB에서 수행. 원본 Python 색인기가 쓴 세대 서명 = 매니페스트, MCP 원시 HTTP/JSON-RPC 경계 사례와 도구 결과 전체, 공식 Python SDK `legacy`/`auto`(2026-07-28) × 공개/소유자, 웹 공개 API 32종, Rust web → Python MCP 혼합 구성, 임베딩 요청이 질문 1개 + ZDR 설정만 보내는지 |
| 브라우저(Chromium + Playwright, 1280/390px) | 원본 프런트엔드가 Rust web에서 검색·원문 보기·지도(워커 레이아웃)까지 Python web과 같은 결과, CSP 위반·스크립트 오류 없음, 가로 넘침 없음. 1280px에서 콘솔 404 한 건이 Python web에서도 똑같이 나타남(브라우저가 자동 요청하는 `/favicon.ico`로 보이며, 두 구현 모두 해당 경로가 없음) |
| 릴리스 바이너리 | 20.9 MB, glibc ≤ 2.34 심볼만 사용 |

**시험하지 못한 것:** 컨테이너 이미지 빌드(로컬 rootless podman 사용 불가), CI 실행, arm64, PostgreSQL TLS 연결, 실제 OIDC 공급자, 실제 코퍼스/DB/임베딩, Open WebUI 클라이언트, 부하·지연 측정(성능 주장 없음).

## 11. 컨테이너와 CI

- `Dockerfile`: Ubuntu 24.04(다이제스트 고정) + `ca-certificates`, 미리 빌드한 바이너리, SHA-256 확인한 토크나이저. UID/GID 4261(기존 PVC 소유권과 같음), 루트 파일 시스템 읽기 전용 가능. 쓰기가 필요한 곳은 web의 `SEARCH_STATE_DB` 디렉터리와(설정했다면) MCP의 `RAG_ALERT_DIR`뿐입니다. 비밀값은 이미지에 없습니다.
- `scripts/fetch_tokenizer.sh`: 원본 Docker 흐름과 같은 게시자 리비전 + SHA-256 확인(값은 매니페스트에서 읽음).
- `.github/workflows/ci.yml`: 고정 커밋 액션, `--locked`, amd64/arm64 네이티브 러너, pgvector 서비스 컨테이너(원본과 같은 다이제스트)로 DB 시험. **main 브랜치에서만 이미지를 게시**하며, 저장소 이름에서 나온 GHCR 이름을 사용합니다. PR에서는 빌드·검증만 수행합니다.

## 12. 운영 전환 계획

1. **게시·이미지 검증:** 승인된 저장소에서 CI를 통과한 이미지의 다이제스트와 실행 권한·CA 인증서·기동 설정을 확인합니다.
2. **MCP 카나리(읽기 전용):** 기존 MCP Deployment는 그대로 두고, 같은 env/마운트/네트워크 정책으로 Rust MCP를 별도 Deployment·Service로 띄웁니다(DB 읽기 전용 계정, 코퍼스 읽기 전용). 내부에서 `scripts/check_mcp.py`(원본)와 공식 SDK로 legacy/auto 연결, 도구 목록이 계약과 같은지, 같은 질의를 Python/Rust에 보내 결과 JSON이 같은지 비교합니다. 임베딩 비용이 드는 질의는 최소한으로 합니다.
3. **web 카나리:** Rust web을 별도 Service로 띄우되 **별도 SQLite**와 낮은 예산(`SEARCH_DAILY_LIMIT`)을 줍니다(같은 PVC를 두 파드가 공유하지 않음). 비공개 경로로 `/`, `/api/folders`, `/api/search`, `/api/graph`, `/read`, 로그인/로그아웃을 확인합니다. Python web은 공개 트래픽을 계속 받습니다.
4. **전환:** Python web을 0으로 줄인 다음 같은 PVC로 Rust web을 올립니다(예산·세션 유지). MCP는 Service 선택자를 Rust로 바꿉니다. 진행 중이던 로그인만 다시 해야 합니다.
5. **롤백:** 이전 Deployment 매니페스트와 이미지 다이제스트를 보존해 두고 되돌립니다. SQLite와 DB 형식이 같으므로 데이터 변환이 없습니다.
6. **원본 900초 문제**는 전환과 별개로 Python 쪽에서 확인합니다.

## 13. 개인정보·보안 한계

- 합성 데이터와 목 서비스로만 시험했습니다. 실제 코퍼스의 Markdown이 모두 같은 HTML로 렌더링된다는 보장은 차이 시험 범위(398개 사례, Markdown 본문 54종)까지입니다. 카나리에서 실제 문서 일부를 비공개로 비교해야 합니다.
- 로컬 PostgreSQL은 OpenSSL 헤더가 없어 TLS 없이 루프백으로만 시험했습니다. TLS 경로(verify-full 등)는 단위 시험만 했습니다.
- 실제 OIDC 공급자(authentik)와의 상호 운용은 시험하지 않았습니다(서명된 RS256 토큰을 쓰는 목 공급자로만 시험).
- 공개 검색 예산은 여전히 서버 전체 공유라서 익명 사용자가 소진시킬 수 있습니다(원본과 같음).
