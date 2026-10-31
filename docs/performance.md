# notes-rag 서빙 성능·메모리 비교: Python 원본 vs Rust 포트 (합성 데이터, 로컬)

> **로컬 합성 벤치마크 측정 완료.** 이 측정에서는 운영 DB·코퍼스·AI·자격증명을 사용하지 않았습니다. 실제 배포 상태와 운영 검증 결과는 [README](../README.md)를 참조하세요.
> 수치는 이 호스트·이 합성 데이터·이 설정에서의 측정값이며, 운영 환경 성능을 보장하지 않는다.

## 1. 대상과 동일 조건

| 항목 | Python (oracle) | Rust (port) |
|---|---|---|
| 소스 | 원본 커밋 `754a97398ac1d6ef313605f84eb7249db8eb556e` `git archive` 사본, 150/150 파일 blob 해시 일치, **무수정** | 마커 지정 release 바이너리, sha256 `f30e2bec401065bf504de2e64e50aa8f7602c6e7c94e23d93513b6abe3554acb` (스냅샷 복사본 사용) |
| 런타임 | CPython 3.11.15 (운영 이미지 3.11.16), `pip freeze` == `requirements.lock` | rustc/cargo 1.99.1 빌드 (빌드는 구현 담당이 수행, 본 작업은 컴파일 안 함) |
| 기동 | `benchmarks/scripts/py_launch.py`: `main()`을 그대로 재현, bind host/port·MCP URL·selection/policy 경로만 변경 | ENV 전용 (`RAG_ROLE`, `RAG_LISTEN_ADDR`, `RAG_MCP_URL`, `RAG_WEB_*_PATH`) |

공통: 같은 합성 DB·코퍼스·ACL 파일, 같은 토크나이저(sha256 `83cdf8c3…b8d`), 같은 mock 임베딩, 같은 토큰, 같은 클라이언트·요청 바이트, 같은 CPU 배치. 동시성 상한은 원본 그대로(MCP 2슬롯, 공개 게이트웨이 1), 공개 쿼터(60/분, 60/시, 300/일)도 그대로다. 클라이언트가 상한을 넘기지 않으며, 거절은 0건이다.

- **DB**: PostgreSQL 18.6 + pgvector 0.8.1, 공식 소스로 scratch 빌드. 127.0.0.1 전용이며 TLS는 없다. 원본 `schema.sql`·`access_schema.sql`·`pruning_schema.sql`·`visibility_schema.sql`을 그대로 적용했다. 서빙 역할 `notes_rag_reader`는 SELECT만 가능하며, 쓰기 거부를 확인했다. 연결 정보는 테스트 환경에서만 보관하며 이 저장소에는 포함하지 않습니다.
- **합성 데이터**: 17 notebook(공개 13), 2,220 page, 3,161 chunk(normal 2,914 / history 247). 재현 가능한 합성 규모로 구성했다. 그 밖의 구성:
  - missing_pages 5행, 소스 파일이 없는 색인 page 4개
  - 1536차원 벡터
  - config_signature `f9ec8b90…`: 원본 코드로 계산했으며, Rust `compat/producer-manifest.json`과 일치한다.
- **Mock 임베딩**: 결정적 벡터를 만들고, 모델 alias·dimensions·encoding·ZDR provider·bearer를 검증한다. 지연 0 ms와 고정 30 ms 두 인스턴스를 사용했다. 전체 실행에서 거부 0, 사전계산 밖의 입력 0이었다.
- **CPU 배치 (10코어, SMT 없음)**: PG 2-3, mock 4, MCP 서버 5-6, 공개 웹 7, 클라이언트 8-9 (0-1은 호스트·타 작업용). Rust tokio 워커 수는 affinity를 따라 2개로 잡힌다.

## 2. 출력 동등성 (타이밍 전에 먼저 비교)

- `benchmarks/raw/parity.json`: MCP 148/149, 공개 62/62 동일. 불일치 1건은 문서화된 `GET /mcp` 차이다(Python 200 SSE 스트림, Rust 405).
  - 주의: 이 실행의 `parity.py`는 측정 직전(17:20) root 리뷰(`benchmarks/root-harness-review.md`)로 수정됐다. 차이·전송 오류가 있으면 타이밍을 중단하는 게이트를 추가했고, 다음 두 가지만 허용한다. 추가된 내용은 Pydantic 인자 검증 오류 텍스트에서 `input_value=…`와 도움말 URL을 제거하는 정규화, 그리고 SSE 예외 처리다.
- `benchmarks/raw/parity-strict.json`: 그 정규화를 런타임에 끄고 다시 비교한 결과다(파일은 수정하지 않음). **MCP 139 동일 / 9 상이 / SSE 1, 공개 62/62 동일.**
  - 9건은 모두 잘못된 인자에 대한 tool 오류 텍스트다. Rust는 입력값 echo와 pydantic URL을 생략한다. 필드명·제약·오류 type·`isError`·HTTP 상태는 같다.
- 타이밍 중의 모든 측정 요청(MCP 6,000쌍, 공개 870쌍)을 구현 간에 대조했다. `structuredContent`와 content text(raw/parsed)가 **6,000/6,000, 870/870 동일**이었다. 실패·빈 응답·429·503은 0건이다.
  - "no_matches" 정상 응답은 양쪽이 같은 요청에서 같은 내용으로 나왔다.

## 3. 측정 설계

- **라운드**: 6라운드. 순서를 회전·역전하고, 홀수 라운드는 공개 블록을 먼저 실행했다. trial마다 **새 서버 프로세스**를 띄우고, 공개 trial은 새 SQLite state를 쓴다. trial 최대 길이는 57.9 s다.
  - Python의 15분 임베딩 deadline 문제(첫 요청 시점 + 900 s, 리셋 없음)를 피하기 위한 설계다. 그 실패 경로는 측정하지 않았다.
- **MCP**: `tools/call search_notes`, stateless Streamable HTTP(JSON). 연결당 initialize는 1회이고 측정에서 제외했다.
  - 워밍업은 c=1 20건, c=2 20건이다. 이어서 측정 c=1 200건, c=2 300건을 trial마다 실행했다.
  - 모든 질의는 run 전체에서 처음 쓰이는 문자열이다(캐시 무효). 10건 중 1건은 folder 범위 검색이다.
- **공개 HTTP**: `POST /api/search` → 같은 언어의 MCP로 간다. 게이트웨이는 요청마다 MCP 세션 handshake를 수행한다. c=1로 실행했다.
  - uncached: 워밍업 5건 + 측정 50건 (지연 0 / 30 ms)
  - cached: 서로 다른 10건을 채운 뒤 cache hit 45건
- **계측**:
  - 서버 CPU: `/proc/PID/stat` utime+stime (스레드 전체).
  - 메모리: `/proc/PID/status` VmRSS/VmHWM, `smaps_rollup` PSS. 시점은 기동 직후, 워밍업 후 idle, 부하 중 0.2 s 샘플, 부하 후 idle이다.
  - PG CPU: postmaster와 backend, 회수된 자식 포함.
  - 호스트: CPU별 사용률.
  - 컴파일 프로세스(`cargo`/`rustc`/`cc1` 등)는 1초 간격으로 감시했다.
- **오염 규칙(사전 정의)**: 컴파일 프로세스가 관측된 trial은 contaminated로 표시한다. 같은 호스트의 다른 작업이 측정 중 `cargo`/`rustc`를 실행해 **10/60 trial**이 오염됐다(라운드 2의 9건, 라운드 4의 py/30 1건).
  - 아래 본표는 **컴파일 감지 구간 제외(clean-only)**다(모든 그룹이 5 clean 라운드 이상).
  - 전체 포함 요약은 `benchmarks/raw/summary-all.json`에 있다. p50 차이는 최대 약 1.6 ms이고, 결론은 같다.
- **통계**: 지연은 성공 응답 전체를 pool한 백분위다. p99는 그룹 표본이 1,000 이상일 때만(MCP) 낸다. "범위"는 라운드별 값의 min–max다.

## 4. 결과 (clean-only, 중앙값; Δ = Rust − Python, 비 = Rust/Python)

### 4.1 MCP `search_notes` (end-to-end, 클라이언트 측정)

| 경우 | 지표 | Python | Rust | Δ | 비 |
|---|---|---|---|---|---|
| mock 0 ms, c=1 (n=1000/구현) | p50 / p95 / p99 ms | 105.0 / 123.8 / 132.0 | 77.2 / 98.2 / 106.5 | −27.9 / −25.6 / −25.5 | 0.73 / 0.79 / 0.81 |
| | 처리량 req/s (범위) | 9.92 (9.64–10.11) | 13.32 (12.97–13.68) | +3.40 | 1.34 |
| mock 0 ms, c=2 (n=1500) | p50 / p95 / p99 ms | 105.2 / 130.7 / 146.5 | 75.7 / 96.5 / 105.6 | −29.5 / −34.2 / −40.9 | 0.72 / 0.74 / 0.72 |
| | 처리량 req/s | 19.30 (19.16–19.52) | 26.91 (26.84–27.54) | +7.61 | 1.39 |
| mock 30 ms, c=1 | p50 / p95 / p99 ms | 138.6 / 153.6 / 168.0 | 116.6 / 131.4 / 137.5 | −22.0 / −22.1 / −30.6 | 0.84 / 0.86 / 0.82 |
| | 처리량 req/s | 7.50 (7.47–7.55) | 8.93 (8.83–9.09) | +1.43 | 1.19 |
| mock 30 ms, c=2 | p50 / p95 / p99 ms | 139.1 / 162.1 / 175.3 | 113.3 / 132.0 / 142.0 | −25.8 / −30.2 / −33.3 | 0.81 / 0.81 / 0.81 |
| | 처리량 req/s | 14.67 (14.45–14.85) | 18.12 (17.95–18.50) | +3.46 | 1.24 |

**서버 CPU (요청당, /proc)**: Python은 29.7–31.0 ms, Rust는 8.2–9.5 ms다(비 0.28–0.30, 약 −21 ms). 라운드 범위는 Python 28.6–31.1, Rust 8.1–9.8로 겹치지 않는다.

**PG CPU (요청당)**: Python 67.9–73.7 ms, Rust 62.6–72.6 ms. 같은 SQL이므로 지배적인 공통 비용이다. exact scan에서 TOAST된 1536차원 벡터를 행마다 다시 읽는다.
- 지연 0 ms 조건에서 Rust가 약 5 ms 적다. 원인은 추정이며 검증하지 않았다. Python은 검색마다 PG 연결을 새로 2개(check + search) 열어 backend fork와 SCRAM 인증이 반복된다.
- 관측된 사실은 mock 임베딩 연결 수뿐이다. Python은 요청마다 새 연결을 열었고(trial당 541개), Rust는 연결을 재사용했다(3개).

**지연 0 → 30 ms 증가폭**: Python +33.6 ms, Rust +39.4 ms (p50, c=1). Rust는 upstream 30 ms 외에 약 9 ms가 더 늘어난다. 원인은 미확인이며 구현 측 조사를 권한다(예: 연결 재사용 경로의 timer/flush). 그래서 upstream 대기가 있을 때 Rust의 이득이 줄어든다(Δp50 −27.9 → −22.0 ms).

### 4.2 공개 검색 `/api/search` (게이트웨이 + MCP, c=1, n=250/구현)

| 경우 | p50 / p95 ms (Py) | p50 / p95 ms (Rs) | Δp50 / Δp95 | 비 p50 | 처리량 Py → Rs req/s |
|---|---|---|---|---|---|
| uncached, mock 0 ms | 139.9 / 165.1 | 88.4 / 106.2 | −51.5 / −58.8 | 0.63 | 7.18 → 11.37 |
| uncached, mock 30 ms | 171.9 / 199.8 | 123.6 / 137.5 | −48.2 / −62.3 | 0.72 | 5.82 → 8.34 |
| cache hit | 4.33 / 7.19 | 2.07 / 3.91 | −2.26 / −3.28 | 0.48 | 161 (160–239) → 322 (298–361) |

**요청당 서버 CPU (uncached)**:
- 웹: Python 28.0–28.4 ms, Rust 5.2–5.4 ms (비 약 0.19)
- MCP: Python 41.2–42.0 ms, Rust 10.4 ms (비 약 0.25). 직접 MCP 호출보다 크다. 요청마다 initialize handshake가 붙기 때문이다.
- cache hit 웹: Python 4.4 ms, Rust 1.3 ms

공개 처리량은 쿼터(60/분)가 실제 상한이다. 위 처리량은 짧은 burst 내의 서버 능력일 뿐이며, 운영 처리량 증가를 뜻하지 않는다.

### 4.3 서버 프로세스 메모리 (MiB, DB·클라이언트 제외, 단일 프로세스)

| 프로세스 | 상태 | Python RSS / PSS | Rust RSS / PSS | Δ RSS | 비 RSS |
|---|---|---|---|---|---|
| MCP | 기동 직후 idle | 275.2 / 263.8 | 104.6 / 102.4 | −170.6 | 0.38 |
| MCP | 워밍업 후 idle | 277.4 / 266.1 | 108.3 / 106.2 | −169.1 | 0.39 |
| MCP | 부하 중 최대 샘플 (c=2) | 279.5 / — | 109.4 / — | −170.1 | 0.39 |
| MCP | VmHWM (프로세스 수명 peak) | 280.2 | 167.8 | −112.4 | 0.60 |
| 웹 | 워밍업 후 idle | 81.9 / 65.7 | 18.3 / 10.7 | −63.6 | 0.22 |
| 웹 | VmHWM | 82.3 | 18.7 | −63.6 | 0.23 |

- **Rust MCP의 HWM 168 MiB**는 기동 중 일시 peak로 보인다. ready 시점 RSS 104.6 MiB가 이미 HWM보다 낮고, 부하 중에는 다시 도달하지 않았다. 토크나이저 JSON 파싱 중의 임시 할당으로 추정하며, 검증하지 않았다.
  - 컨테이너 memory limit은 steady가 아니라 peak 168 MiB 기준으로 잡아야 한다.
- **Python MCP 약 277 MiB의 대부분은 anon(약 240 MiB)**이다. 원본은 owner 토큰이 설정되면 `from_environment()`를 두 번 호출해, 토크나이저를 가진 SearchService가 두 개 생긴다. 운영도 owner 토큰을 쓰므로 그 구성을 그대로 측정했다.
  - Python은 reference counting과 cyclic GC를 쓰며, 측정 구간 내 RSS 변동은 2–3 MiB 이내였다.
- 스레드 수: Python MCP 3, 웹 2. Rust 4–5(tokio 워커 2와 보조 스레드).
- 기동 시간(spawn부터 `/healthz` 200까지, 중앙값): MCP는 Python 1.13 s, Rust 0.39 s. 웹은 Python 0.62 s, Rust 0.05 s.
- PG(공통) PSS는 약 99 MiB, 9 프로세스다.
- 종료: SIGTERM에 Rust는 exit 0, Python 프로세스는 -15(signal)로 끝났다. 양쪽 다 측정 후의 일이다.

## 5. 해석과 한계 (중요)

- 이 워크로드의 end-to-end 지연은 **PG의 exact-scan 비용(요청당 약 70 ms CPU)이 지배**한다. Rust의 지연 이득(MCP p50 약 −22~−30 ms, 공개 −48~−51 ms)은 서버 자체 CPU 감소분(MCP 약 −21 ms, 웹 약 −23 ms)과 크기가 비슷하다. SQL·벡터 검색 비용은 그대로다. 이 결과에서 "Rust가 검색 자체를 빠르게 했다"고 일반화하지 말 것.
- **TLS 없음**: 운영 PG는 verify-full TLS다. Python의 요청당 신규 연결 2개 비용(TLS handshake와 네트워크 RTT)은 운영에서 더 클 것이고, 여기서는 과소평가돼 있다. 같은 이유로 Rust 연결 재사용의 이점도 과소평가됐을 수 있다. 측정하지 않았다.
- 실제 임베딩 지연은 수백 ms일 수 있다. 그 경우 상대 이득은 30 ms 케이스보다 더 줄어든다. 고정 30 ms는 upstream 대기 효과를 보기 위한 모형이다.
- pgvector는 범용 빌드(`OPTFLAGS=""`), PG에는 JIT이 없다. 운영(CNPG) 빌드 플래그·설정과 다를 수 있다. 다만 양쪽에 공통인 부분이다.
- 공유 호스트다. clean trial에서도 CPU 0–1에 타 작업 부하가 있었다(loadavg 2–13). 라운드 간 범위가 좁아(MCP 처리량 라운드 범위 ±3% 내외) 영향은 작다고 보지만, 완전 격리는 아니다.
- Python 서버는 `py_launch.py`로 기동했다. `main()`과 동일한 factory·uvicorn 인자를 쓰고 bind와 경로만 바꿨다.
- 이 결과로 운영 효과를 주장하지 않는다. 운영 canary 측정은 별도로 필요하다.

## 6. 재현 명령

```sh
# 저장소 루트에서 시작. 먼저 README의 기능·동등성 검사를 통과해야 합니다.
cd docs/benchmarks
ORIGINAL=/absolute/path/to/python-notes-rag scripts/setup.sh

# 검증한 릴리스 바이너리를 지정하고 준비 마커를 기록합니다.
export RUST_BINARY=/absolute/path/to/notes-rag-rs/target/release/notes-rag-rs
python3 - <<'PY_READY'
import hashlib, json, os
from pathlib import Path
binary = Path(os.environ["RUST_BINARY"]).resolve()
Path("/tmp/rag-rs-ready-for-bench").write_text(json.dumps({
    "binary": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()
}))
PY_READY
scripts/final_run.sh

# 엄격 parity: 의도적인 오류 텍스트 차이 9건도 차이로 기록하며 비영 종료합니다.
(cd scripts && /tmp/rag-bench-venv/bin/python -c "import re,sys,asyncio;sys.argv=['parity.py','--rust-binary','/tmp/rag-bench/bin/notes-rag-rs','--out','../raw/parity-strict.json'];import parity;parity.PYDANTIC_DETAIL=re.compile(r'(?!)');asyncio.run(parity.main())")
# 원시 데이터로 요약 재계산
/tmp/rag-bench-venv/bin/python scripts/summarize.py raw/bench-raw.json --clean-only --out raw/summary-clean.json
```

## 7. 산출물

- `benchmarks/scripts/`:
  - `setup.sh`, `pgctl.sh`, `gen_fixtures.py`, `synth.py`, `load_db.py`: 준비
  - `mock_embed.py`, `py_launch.py`, `bench_env.py`, `harness.py`: 실행 구성 요소
  - `parity.py`(root 리뷰 수정 반영), `run_bench.py`, `summarize.py`, `final_run.sh`: 비교·측정·요약
- `benchmarks/requirements/requirements.lock`(원본 lock), `benchmarks/requirements/venv-freeze.txt`(동일).
- `benchmarks/fixtures/manifest.json`(모든 생성물 sha256), `markdown.json`, `access.json`. 전체 생성물은 `/tmp/rag-bench/fixture`에 있으며 시드로 재생성할 수 있다.
- `benchmarks/raw/`:
  - `bench-raw.json`: 60 trial, 요청별 기록. 노트 본문은 없고 digest만 담았다.
  - `summary-all.json`, `summary-clean.json`, `parity.json`, `parity-strict.json`
  - 절대경로·서비스 주소가 없음을 확인했다(인용 origin은 `<silverbullet-origin>`으로 redaction).
- 로컬 DB 연결 정보·생성된 테스트 토큰은 공개 산출물에 포함하지 않습니다.
- 재현 환경은 `benchmarks/scripts/setup.sh`로 생성합니다. 테스트용 자격증명은 실행 시 새로 만듭니다.
