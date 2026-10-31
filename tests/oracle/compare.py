#!/usr/bin/env python3
"""Differential test: pinned Python notes-rag functions vs the Rust port.

Usage (oracle venv with the pinned requirements.lock):
    RAG_TOKENIZER_PATH=/path/qwen-tokenizer.json \
    python -I tests/oracle/compare.py <pinned-python-source> <target/debug/examples/oracle_probe> [report.json]

Only synthetic inputs are used. Exit status 1 if any unexpected mismatch.
Known, documented intentional differences are listed in KNOWN and reported
separately instead of failing the run.
"""
import json
import os
import posixpath
import re
import subprocess
import sys
import tempfile
import unicodedata
from pathlib import Path, PurePosixPath

SOURCE = Path(sys.argv[1]).resolve()
PROBE = sys.argv[2]
REPORT = sys.argv[3] if len(sys.argv) > 3 else None
sys.path.insert(0, str(SOURCE))
os.environ['RAG_NATIVE_NOTES'] = '1'

from urllib.parse import quote, unquote, urlencode, urlsplit  # noqa: E402

from starlette.datastructures import QueryParams  # noqa: E402

import document_reader  # noqa: E402
import folder_scope  # noqa: E402
import native_notes  # noqa: E402
import public_search  # noqa: E402
import search  # noqa: E402
import semantic_graph  # noqa: E402
from access_policy import load_public_notebooks  # noqa: E402
from embedding_config import QwenTokenizer, TOKENIZER_MODEL, query_text  # noqa: E402
from index_plan import config_signature, sha256  # noqa: E402


def kind(error):
    if isinstance(error, document_reader.NotReadable):
        return 'NotReadable'
    if isinstance(error, search.SearchError):
        return 'SearchError'
    if isinstance(error, ValueError):
        return 'ValueError'
    if isinstance(error, OSError):
        return 'OSError'
    return 'Exception'


def environment():
    root = Path(tempfile.mkdtemp(prefix='rag-oracle-'))
    (root / 'corpus').mkdir()
    selection = root / 'markdown.json'
    selection.write_text(json.dumps({'schemaVersion': 1, 'sourceArchiveRoot': 'a', 'silverBulletRoot': 'OneNote/markdown',
        'notebooks': [{'id': 'nb-public', 'name': 'public'}, {'id': 'nb-private', 'name': 'private'},
                      {'id': 'dup1', 'name': 'dup'}, {'id': 'dup2', 'name': 'dup'}]}))
    policy = root / 'access.json'
    policy.write_text(json.dumps({'schemaVersion': 1, 'publicNotebookIds': ['nb-public']}))
    revision = load_public_notebooks(policy, {'nb-public', 'nb-private', 'dup1', 'dup2'})[1]
    return document_reader.DocumentReader(root / 'corpus', selection, policy, revision), selection


READER, SELECTION = environment()
TOKENIZER = QwenTokenizer() if os.environ.get('RAG_TOKENIZER_PATH') else None


def slug(content):
    value = re.sub(r'[^\w\s-]', '', content).strip().lower()
    return re.sub(r'\s+', '-', value) or 'heading'


def python(name, a):
    if name == 'quote': return quote(a['s'], safe=a['safe'])
    if name == 'unquote': return unquote(a['s'])
    if name == 'urlencode': return urlencode([tuple(p) for p in a['pairs']])
    if name == 'urlsplit': return list(urlsplit(a['s']))
    if name == 'query_param': return QueryParams(a['raw'].encode().decode('latin-1')).get(a['name'])
    if name == 'normpath': return posixpath.normpath(a['s'])
    if name == 'dirname': return posixpath.dirname(a['s'])
    if name == 'suffix': return PurePosixPath(a['s']).suffix
    if name == 'stem': return PurePosixPath(a['s']).stem
    if name == 'float_repr': return repr(a['x'])
    if name == 'dumps_ascii': return json.dumps(a['v'])
    if name == 'dumps_unicode': return json.dumps(a['v'], ensure_ascii=False)
    if name == 'normalize_ws': return ' '.join(a['s'].split())
    if name == 'strip': return a['s'].strip()
    if name == 'splitlines': return a['s'].splitlines(keepends=True)
    if name == 'casefold': return a['s'].casefold()
    if name == 'isidentifier': return a['s'].isidentifier()
    if name == 'py_str': return str(a['v'])
    if name == 'validate_folder': return folder_scope.validate_folder(a['v'])
    if name == 'folder_catalog': return folder_scope.folder_catalog(a['paths'])
    if name == 'citation_url': return search.citation_url(a['rel'], a['base'], a['root'])
    if name == 'contextual_row':
        row = search.contextual_row(a['row'], a['neighbors'], a['maximum'])
        return search.result_item(row, 'https://notes.example.invalid', 'OneNote/markdown')
    if name == 'result_item':
        return search.result_item(a['row'], 'https://notes.example.invalid', 'OneNote/markdown', a['maximum'])
    if name == 'public_result': return public_search.public_result(a['result'], a['folder'])
    if name == 'public_folders': return public_search.public_folders(a['result'])
    if name == 'visible_graph': return semantic_graph.visible_graph(a['graph'], set(a['paths']))
    if name == 'slug': return slug(a['s'])
    if name == 'frontmatter': return list(document_reader.frontmatter(a['s']))
    if name == 'render_body':
        safe, assets = READER.render_body(a['path'], a['body'])
        return [safe, sorted(assets)]
    if name == 'normalize': return native_notes.normalize(a['raw'].encode(), a['relative'], SELECTION).decode()
    if name == 'target': return READER.target(a['path'], a['value'])
    if name == 'scope_signature': return sha256(json.dumps(sorted(a['ids']), ensure_ascii=False).encode('utf-8'))
    if name == 'revision':
        import hashlib
        return hashlib.sha256(json.dumps(tuple(sorted(a['ids']))).encode()).hexdigest()
    if name == 'config_signature':
        manifest = json.loads((Path(__file__).resolve().parents[2] / 'compat' / 'producer-manifest.json').read_text())
        version = f"{manifest['tokenizer_revision']}:{manifest['tokenizer_sha256']}:tokenizers-{manifest['python_tokenizers_version']}"
        return config_signature(tokenizer_name=TOKENIZER_MODEL, tokenizer_version=version,
                                target_tokens=650, max_tokens=800, overlap_tokens=100)
    if name == 'query_text': return query_text(a['s'])
    if name == 'token_count': return [TOKENIZER.count(t) for t in a['texts']]
    if name == 'char_classes':
        return [[bool(re.match(r'\w', chr(c))), chr(c).isspace(), chr(c).lower(), chr(c).casefold()] for c in a['codes']]
    raise KeyError(name)


def case(fn, **args):
    return {'fn': fn, 'args': args}


def cases():
    out = []
    strings = ['', 'a b', '한글 #%', 'a (b)/한글 #%.md', '~._-', '\x00\x7f', '😀', 'a+b=c&d', '%41%zz%', '%ED%95', '한%41',
               'x%2Fy', '%E2%82%AC%20', 'ü%C3%BC']
    for s in strings:
        out += [case('quote', s=s, safe='/'), case('quote', s=s, safe=''), case('unquote', s=s),
                case('normalize_ws', s=' \t' + s + '\u00a0\u3000x '), case('strip', s='\u0085 ' + s + ' \u2028'),
                case('casefold', s=s + 'ßİΣς'), case('splitlines', s=s + '\r\n\x0b\x1c\u2029x\r')]
    out.append(case('urlencode', pairs=[['path', 'public/next note.md'], ['asset', 'a b/한.png'], ['x', '~*\'()']]))
    for u in ['HTTPS://notes.cola16.app/OneNote/markdown/a?x=1#f', 'note.assets/x.png', 'javascript:alert(1)', '  java\tscript:x',
              '//host/path', 'mailto:a@b', 'http://[::1]/x', '1a:b', 'a:b', 'https://notes.cola16.app', '#frag', '?q', 'C:\\x',
              'http://[bad/x', 'https://user:pw@notes.cola16.app/x', '../private/x.png', '/OneNote/markdown/x']:
        out.append(case('urlsplit', s=u))
    for raw, name in [('path=a%2Fb.md&path=c+d', 'path'), ('x', 'x'), ('y=1', 'x'), ('p=한', 'p'), ('p=%ED%95%9C', 'p'),
                      ('a=1&&b=2', 'b'), ('a=%zz', 'a'), ('path=public/note.md', 'path'), ('p=a;q=b', 'p')]:
        out.append(case('query_param', raw=raw, name=name))
    for p in ['public/note.md', '../a', 'a/../..', '', '//a//b/', 'a/./b/', '/x', 'note.md', 'a/b.tar.gz', 'a/.hidden', 'a/b.',
              '제목.md', 'a/b/']:
        out += [case('normpath', s=p), case('dirname', s=p), case('suffix', s=p), case('stem', s=p)]
    for x in [0.0, -0.0, 1.0, 0.35, 0.1 + 0.2, 1e-5, 1e-4, 1e16, 1e15, 123456.789, -2.5e-7, 1.7976931348623157e308, 5e-324,
              0.72154, 1761900123.456789]:
        out.append(case('float_repr', x=x))
    for v in [{'b': 1, 'a': '한'}, ['v', '한', None, True, 1.5], '\x7f\x01"\\\n', '😀', [1e-05, 0.5]]:
        out += [case('dumps_ascii', v=v), case('dumps_unicode', v=v)]
    for v in ['x', 3, 3.0, True, None, ['a', 1, None, 'it\'s'], {'k': 'v', 'n': [1.5]}, '', 0]:
        out.append(case('py_str', v=v))
    for v in ['', "실습 (test)/100%_O'Reilly", 'a/%2e%2e', None, [], 1, '/a', 'a/', 'a//b', '.', '..', 'a/../b', 'a\\b', 'a\n',
              'x' * 2049, 'x' * 2048, 'a\x7fb']:
        out.append(case('validate_folder', v=v))
    for paths in [['root.md', '노트/a.md', '노트/하위/b.md', '노트2/c.md'], ['b/x.md', 'A/x.md', 'a/x.md', 'ß/x.md', 'SS/y.md'],
                  ['../x.md'], ['x.txt'], ['/x.md'], ['a/b/c/d.md', 'a/b/e.md']]:
        out.append(case('folder_catalog', paths=paths))
    for rel, base in [('a (b)/한글 #%.md', 'https://notes.example.invalid'), ('../x.md', 'https://n.x'), ('/x.md', 'https://n.x'),
                      ('https://evil/x.md', 'https://n.x'), ('x.txt', 'https://n.x'), ('a\\b.md', 'https://n.x'),
                      ('x.md', 'http://n.x'), ('x.md', 'https://n.x/sub'), ('x.md', 'https://n.x/'), ('a/./b.md', 'https://n.x'),
                      ('x.md', 'https://u@n.x'), ('x.MD', 'https://n.x'), ('.md', 'https://n.x')]:
        out.append(case('citation_url', rel=rel, base=base, root='OneNote/markdown'))
    row = dict(page_key='a' * 64, ordinal=2, title='T', relative_path='실습/dnf-conf.md', status='current',
               rag_priority='normal', needs_visual_review=True, content='matched procedure', similarity=0.721544999)
    neighbors = [{**row, 'ordinal': i, 'content': f'chunk{i}'} for i in (0, 1, 3, 4)]
    neighbors += [{**row, 'page_key': 'b' * 64, 'ordinal': 3, 'content': 'OTHER'}, {**row, 'rag_priority': 'fallback', 'ordinal': 1, 'content': 'OLD'}]
    strip_row = lambda n: {k: n[k] for k in ('page_key', 'ordinal', 'rag_priority', 'content')}  # noqa: E731
    out.append(case('contextual_row', row=row, neighbors=[strip_row(n) for n in neighbors], maximum=6000))
    out.append(case('contextual_row', row=row, neighbors=[strip_row({**row, 'ordinal': i, 'content': 'x' * 80}) for i in (1, 3)], maximum=160))
    out.append(case('contextual_row', row={**row, 'content': '한' * 6001}, neighbors=[strip_row({**row, 'ordinal': 3, 'content': 'n'})], maximum=6000))
    out.append(case('result_item', row={**row, 'content': 'x' * 131073}, maximum=131072))
    prefix = 'https://notes.cola16.app/OneNote/markdown/'
    results = [
        ({'index': {'indexed_pages': 2178}, 'results': [{'title': '<script>test</script>', 'content': 'synthetic excerpt',
          'source_url': prefix + 'example', 'rag_priority': 'normal', 'page_key': 'x'}]}, ''),
        ({'results': [{'title': 'x', 'content': 'x', 'source_url': 'javascript:alert(1)'}]}, ''),
        ({'results': [{'title': 't', 'content': 'c', 'source_url': prefix + 'example2/a'}]}, 'example'),
        ({'results': [{'title': 't', 'content': 'c', 'source_url': prefix + 'example/a'}]}, 'example'),
        ({'results': [{'title': 5, 'content': ['a'], 'source_url': prefix + '%ED%95%9C/a', 'needs_visual_review': 'yes',
                       'truncated': 0, 'rag_priority': 'fallback'}] * 7, 'index': {'indexed_pages': '12', 'scoped_pages': True}}, '한'),
        ({'results': 'nope'}, ''),
        ({'results': [{'title': 'x' * 500, 'content': 'y' * 7000, 'source_url': prefix + 'a'}], 'index': {'indexed_pages': 3.9}}, ''),
    ]
    for result, folder in results:
        out.append(case('public_result', result=result, folder=folder))
    for result in [{'folders': [{'path': 'a/b', 'pages': 1}], 'indexed_pages': 1}, {'folders': [{'path': 'a', 'pages': True}], 'indexed_pages': 1},
                   {'folders': [{'path': 'a', 'pages': 1}] * 2, 'indexed_pages': 1},
                   {'folders': [{'path': 'a', 'pages': 2}, {'path': 'a/b', 'pages': 1}], 'indexed_pages': '2'},
                   {'folders': [{'path': '', 'pages': 1}], 'indexed_pages': 1}, {'folders': [{'path': 'a', 'pages': 1.0}], 'indexed_pages': 1}]:
        out.append(case('public_folders', result=result))
    graph = {'nodes': [{'id': i, 'path': p, 'title': p} for i, p in enumerate(['a.md', 'b.md', 'c.md', 'd.md'])],
             'edges': [{'source': 0, 'target': 1, 'similarity': 0.5}, {'source': 1, 'target': 2, 'similarity': 0.9}, {'source': 2, 'target': 3, 'similarity': 0.7}],
             'index': {'indexed_pages': 4, 'access_revision': 'r'}, 'method': 'm', 'generated_at': 't'}
    for paths in [['b.md', 'c.md'], [], ['a.md', 'b.md', 'c.md', 'd.md'], ['d.md', 'a.md']]:
        out.append(case('visible_graph', graph=graph, paths=paths))
    for heading in ['제목', '  Hello, World! **bold** ', 'a_b - c', '!!!', 'Ünïcödé Ⅻ ½ ²', 'tab\tsep\u00a0nbsp', 'İstanbul ΣΑΣ',
                    '`code` & <tags>', 'x\u0301y', '١٢٣ ٤', 'mixed—dash–en', '']:
        out.append(case('slug', s=heading))
    for text in ['---\ntitle: "x"\ntags: a, b\n---\nbody', '\ufeff---\r\na: 1\r\n---\r\nb', '---\nbad\n---\n', '---\n1x: 2\n---\n',
                 '---\na: 1\na: 2\n---\n', '---\na: [1, 2]\n---\nrest\n---\n', '---\na: NaN\n---\n', 'no front', '---\n' + 'a: 1\n' * 101,
                 '---\na: "unterminated\n---\n', '---\nk: {"x": null}\n---\n\n', '---\na:1\n---\n']:
        out.append(case('frontmatter', s=text))
    for raw, relative in [('# hi\n', 'public/sub/note.md'), ('', 'public/empty.md'), ('---\ntitle: x\n---\nbody', 'public/--old/x.md'),
                          ('x', 'public/old/a.md'), ('x', 'unknown/a.md'), ('x', 'dup/a.md'), ('x', 'root.md'), ('x', '.hidden/a.md'),
                          ('---\nsource_system: "onenote"\nonenote_notebook_id: "wrong"\ngenerated: false\n---\nb', 'public/a.md'),
                          ('---\nsource_system: "onenote"\nonenote_notebook_id: "nb-public"\n---\nb', 'public/a.md'),
                          ('---\nsource_system: "onenote"\ngenerated: true\n---\n', 'public/index.md'),
                          ('---\nsource_system: "other"\nonenote_id: "x"\n---\n', 'public/a.md'),
                          ('---\nsource_system: "onenote"\nonenote_notebook_id: "a"\nonenote_notebook_id: "b"\n---\n', 'public/a.md'),
                          ('---\r\nsource_system: "onenote"\r\nonenote_notebook_id: "old"\r\n---\r\nbody', 'private/x.md'),
                          ('---\nunterminated\n', 'public/a.md'), ('  ---  \n---\nz', 'public/a.md')]:
        out.append(case('normalize', raw=raw, relative=relative))
    for value in ['note.assets/x.png', '../private/note.assets/x.png', 'https://notes.cola16.app/OneNote/markdown/public/next',
                  'https://notes.cola16.app/OneNote/markdown/private/x.png', 'https://other/x', 'http://notes.cola16.app/OneNote/markdown/a',
                  '/OneNote/markdown/public/a', '/abs', '//notes.cola16.app/OneNote/markdown/a', 'next#frag', '%ED%95%9C.md',
                  'sub/../../escape.md', '.hidden/x', 'a//b']:
        out.append(case('target', path='public/dir/note.md', value=value))
    for ids in [['b', '한', 'a'], [], ['nb-public']]:
        out += [case('scope_signature', ids=ids), case('revision', ids=ids)]
    out.append(case('config_signature'))
    out.append(case('query_text', s='zypper update'))
    if TOKENIZER:
        out.append(case('token_count', texts=['한글 문서를 검색합니다.', 'token ' * 3000, '', 'zypper update 방법 OR 사용법',
                                               query_text('테스트'), '😀\u200b\x00 tab\t', 'x' * 4000, '스크립트 <script>']))
    out.append(case('char_classes', codes=[c for c in range(0x20, 0x3400) if unicodedata.category(chr(c)) != 'Cn' and not 0xd800 <= c < 0xe000]
                    + list(range(0xac00, 0xac00 + 200)) + [0x1f600, 0x10400, 0x1d7ce]))
    out += markdown_cases()
    for v in ['onenote_id', '_x1', '1x', 'a-b', '', 'ä', 'x\u0301', '𝔘']:
        out.append(case('isidentifier', s=v))
    return out


MARKDOWN = [
    '# 제목\n# 제목\n## 제목\n',
    '# Heading with **bold** and `code`\n\nSetext\n======\n\nSub\n---\n',
    '```fish\n# comment\necho hello\n```\n',
    '~~~python title="x"\nprint(1)\n~~~\n\n    indented code\n',
    '![test](note.assets/image.png)\n[download](note.assets/file.svg)\n',
    '![private](../private/note.assets/image.png)\n![remote](https://evil.invalid/p.png)\n',
    '[next](https://notes.cola16.app/OneNote/markdown/public/next)\n[frag](next#Section%20One)\n[ext](https://example.com/a?b=1)\n',
    '[mail](mailto:a@example.com) [js](javascript:alert(1)) [data](data:text/html;base64,PHNjcmlwdD4=) [vb](vbscript:x)\n',
    '<script>alert(1)</script><img src="https://evil.invalid/pixel" onerror="alert(1)">\n',
    '<iframe src="https://evil.invalid"></iframe><a href="javascript:alert(1)">x</a>\n',
    '<table><tr><td onclick="evil()" colspan="2" style="color:red">preserved</td></tr></table>\n',
    '| a | b |\n|:--|--:|\n| 1 | **2** |\n| 3 |\n',
    'Text ~~struck~~ and ~single~ and <del>raw</del>\n',
    '<details><summary>More</summary>\n\nHidden *md*\n\n</details>\n',
    '<svg onload="x()"><circle/></svg><math><mi>x</mi></math><style>p{}</style><object data="x"></object><embed src="x">\n',
    '<!-- secret comment --> visible\n',
    '![[note.assets/a.png]] and ![[public/note.assets/b.png|200]] and ![[OneNote/markdown/public/c.png]]\n',
    '![[unterminated and ![[x\ny]]\n',
    '[![[note.assets/inside.png]]](note.assets/outer.png)\n',
    '> quote\n> > nested\n\n- a\n  - b\n1. one\n2. two\n\n3) three\n\n7. start\n',
    'Line  \nbreak\\\nhard\n\n***\n\n___\n',
    '&lt;b&gt; &amp; &copy; &#x3C;script&#x3E; &nbsp; &bogus;\n',
    '<a href="https://notes.cola16.app/OneNote/markdown/public/a.md" title="t" target="_blank" rel="x">raw</a>\n',
    '<img src="note.assets/raw.png" alt="raw" title="t" width="10">\n',
    '<a href="&#106;avascript:alert(1)">ent</a> <a href=" javascript:alert(1)">space</a> <a href="JaVaScRiPt:x">case</a>\n',
    '<div class="x" id="y" lang="ko" title="tt"><span data-x="1">s</span><kbd>K</kbd><sub>1</sub><sup>2</sup></div>\n',
    '<code class="language-js">x</code> <pre class="p">pre</pre> <ol start="5" type="a"><li>x</li></ol>\n',
    '<h2 id="custom" onclick="x">raw heading</h2>\n\n# 제목\n',
    '[ref]\n\n[ref]: note.assets/ref.png "Title"\n\n[link]: https://notes.cola16.app/OneNote/markdown/public/x\n\n[link]\n',
    '<https://example.com/auto> <mailto:a@b.c> <javascript:alert(1)> www.example.com https://bare.example.com\n',
    'Korean 한국어 문단과 **강조** 그리고 _기울임_\n\n',
    '\r\nCRLF\r\n# H\r\n\r\ntext\r\n',
    'Tab\tseparated\tline\n\n\x00 nul char\n',
    '> ' * 30 + 'deep\n',
    '> ' * 19 + 'edge\n',
    '>\n\n> > \n\n- \n\n1.\n\n<blockquote></blockquote>\n',
    ''.join('  ' * i + '- item\n' for i in range(25)),
    '*' * 25 + 'emph' + '*' * 25 + '\n',
    '[' * 25 + 'x' + ']' * 25 + '(note.assets/n.png)\n',
    '_a ' * 30 + '_\n',
    '[a](<note.assets/space name.png>) ![b](note.assets/%ED%95%9C.png) [c](./sub/./x.md) [d](../dir/y)\n',
    '[up](../../escape.md) [abs](/abs/path) [proto](//notes.cola16.app/OneNote/markdown/x)\n',
    '<a href="https://notes.cola16.app/OneNote/markdown/private/x.png">cross asset</a>\n',
    '<img src="data:image/png;base64,AAAA"> ![d](data:image/gif;base64,R0lGOD)\n',
    'Emphasis *nested **strong** text* and ***both***\n',
    '<p onmouseover="x">raw p</p><form action="x"><input value="1"></form><button>b</button>\n',
    '<a href="#local">anchor</a> [hash](#제목)\n',
    '# \n\n#\n\n#   \n',
    '- [ ] task\n- [x] done\n',
    '<table>\n<thead><tr><th rowspan="2">h</th></tr></thead>\n<tbody><tr><td>c</td></tr></tbody>\n<tfoot><tr><td>f</td></tr></tfoot>\n<caption>cap</caption></table>\n',
    '<<<script>>>alert(1)<</script>\n<scr<script>ipt>alert(1)</scr</script>ipt>\n',
    '<a href="http://[bad">bad v6</a> <img src="http://[bad">\n',
    'trailing html <br> <br/> <hr> <img>\n',
    '[t](note.assets/x.png "title &quot; x") ![alt *em*](note.assets/y.jpg)\n',
]


def markdown_cases():
    return [case('render_body', path='public/dir/note.md', body=body) for body in MARKDOWN] + \
           [case('render_body', path='public/note.md', body=body) for body in MARKDOWN[4:8]]


# Documented, intentional differences (README "의도적인 차이").
KNOWN = {
    # nh3 keeps the original attribute when Python's urlsplit raises inside the
    # filter (so a remote <img src="http://[bad"> survived); Rust drops it.
    ('render_body', 'bad v6'),
    ('target', 'http://[bad'),
    # markdown-it-py >= 13 bumps state.level inside skipToken, so >= 20 nested
    # unclosed '[' label parses give up; the Rust port has no such bump and
    # still finds the outer link (pathological input, still sanitized).
    ('render_body', '[[[[[[[[[[[[[[[[[[[[[[[[['),
    # Python json.loads accepts NaN; serde_json rejects it -> stricter NotReadable.
    ('frontmatter', 'a: NaN'),
}


def known(item):
    text = json.dumps(item['args'], ensure_ascii=False)
    return any(fn == item['fn'] and marker in text for fn, marker in KNOWN)


def main():
    all_cases = cases()
    expected = []
    for item in all_cases:
        try:
            expected.append({'ok': python(item['fn'], item['args'])})
        except Exception as error:  # noqa: BLE001 - classify like the Rust Fault
            expected.append({'err': kind(error)})
    output = subprocess.run([PROBE], input=json.dumps(all_cases), capture_output=True, text=True, check=True,
                            env={**os.environ, 'RAG_NATIVE_NOTES': '1'})
    actual = json.loads(output.stdout)
    mismatches, documented = [], []
    for item, want, got in zip(all_cases, expected, actual):
        if item['fn'] == 'char_classes':
            codes = item['args']['codes']
            diffs = [(c, w, g) for c, w, g in zip(codes, want['ok'], got['ok']) if w != g]
            if diffs:
                mismatches.append({'fn': 'char_classes', 'count': len(diffs), 'first': [[hex(c), w, g] for c, w, g in diffs[:12]]})
            continue
        if want != got:
            entry = {'fn': item['fn'], 'args': item['args'], 'python': want, 'rust': got}
            (documented if known(item) else mismatches).append(entry)
    total = len(all_cases)
    summary = {'cases': total, 'mismatches': len(mismatches), 'documented_differences': len(documented),
               'python': sys.version.split()[0]}
    if REPORT:
        Path(REPORT).write_text(json.dumps({'summary': summary, 'mismatches': mismatches, 'documented': documented},
                                           indent=1, ensure_ascii=False))
    print(json.dumps(summary))
    for entry in mismatches[:40]:
        print(json.dumps(entry, ensure_ascii=False)[:1500])
    return 1 if mismatches else 0


if __name__ == '__main__':
    raise SystemExit(main())
